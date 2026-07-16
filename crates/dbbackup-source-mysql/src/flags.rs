// crates/dbbackup-source-mysql/src/flags.rs
//! S04 §5 — mysqldump 플래그 조립(순수 함수). R2/R4/R8/R9. 표 기반 유닛 테스트로 커버(T6~T11).
//! §2.5의 핵심: --column-statistics는 **client_kind(바이너리 종류)**로 분기해야지 server_kind만
//! 보고 분기하면 안 된다(MariaDB 바이너리엔 이 옵션 자체가 없어 unknown option으로 죽음).

use crate::binary::ClientKind;
use crate::version::{ServerKind, ServerVersionInfo};

/// mysqldump 인자를 조립한다(연결정보/DB명 제외 — 그건 dump.rs에서 붙인다).
/// - --single-transaction/--routines/--triggers/--events: 기본값 불일치 회피를 위해 항상 명시(R2).
/// - --default-character-set=utf8mb4: 고정(R3, §2.4).
/// - --column-statistics=0: Oracle client + (MariaDB 서버 or 8.0 미만 서버)일 때만(R4).
/// - --source-data=2 / --master-data=2: binlog 앵커 캡처(R8), client_kind별 문법 분기.
pub fn build_flags(
    server: &ServerVersionInfo,
    client_kind: ClientKind,
    capture_binlog: bool,
) -> Vec<String> {
    let mut args = vec![
        "--single-transaction".to_string(), // InnoDB REPEATABLE READ 스냅샷(§2.1)
        "--routines".to_string(),           // 기본 OFF — 명시 필요(§2.2)
        "--triggers".to_string(),           // 기본 ON이지만 통일을 위해 명시(§2.2)
        "--events".to_string(),             // 기본 OFF — 명시 필요(§2.2)
        "--default-character-set=utf8mb4".to_string(), // §2.4
    ];

    // --column-statistics: Oracle MySQL 8.0 client 전용 플래그. MariaDB 바이너리엔 이 옵션 자체가
    // 없어 넘기면 즉시 에러 종료한다. binary.rs resolve()가 server_kind==client_kind 축을 강제하므로
    // (server.kind == MariaDb) 분기는 정상 경로에선 도달하지 않지만, 향후 매트릭스 완화에 대비해
    // 방어적으로 남긴다. **client_kind로 게이트**하는 것이 회귀 방지의 핵심(R4).
    if client_kind == ClientKind::OracleMySql
        && (server.kind == ServerKind::MariaDb || server.major < 8)
    {
        args.push("--column-statistics=0".to_string());
    }

    // §2.3: PITR 스코프 밖이어도 앵커 포인트는 저비용으로 남긴다.
    if capture_binlog {
        args.push(match client_kind {
            ClientKind::OracleMySql => "--source-data=2".to_string(), // MySQL 8.x 신문법(구 --master-data)
            ClientKind::MariaDb => "--master-data=2".to_string(),     // MariaDB는 개명되지 않음
        });
    }

    args
}

/// mysqldump가 덤프 도중 DDL을 만났을 때 남기는 전형적 stderr 메시지(§2.1 DDL 이슈).
/// exit code가 0으로 보이는 경계 케이스까지 잡기 위해 S05 PipelineCfg.fatal_patterns에 병합한다
/// (부분 스키마 덤프를 "부분 성공"이 아니라 전체 실패로 강등, R9).
pub const DDL_RACE_FATAL_PATTERNS: &[&str] = &[
    "Lost connection to MySQL server during query when dumping table",
    "Couldn't execute 'SHOW CREATE TABLE",
    "Error 2013",
    "Error 1146",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn server(kind: ServerKind, major: u32) -> ServerVersionInfo {
        ServerVersionInfo {
            raw: format!("{major}.0.0"),
            kind,
            major,
            minor: 0,
            patch: 0,
        }
    }

    fn has(args: &[String], needle: &str) -> bool {
        args.iter().any(|a| a == needle)
    }

    #[test]
    fn always_present_flags_r2_r3() {
        let a = build_flags(
            &server(ServerKind::OracleMySql, 8),
            ClientKind::OracleMySql,
            false,
        );
        assert!(has(&a, "--single-transaction"));
        assert!(has(&a, "--routines"));
        assert!(has(&a, "--triggers"));
        assert!(has(&a, "--events"));
        assert!(has(&a, "--default-character-set=utf8mb4"));
    }

    #[test]
    fn t6_oracle_client_mariadb_server_adds_column_statistics() {
        // 방어 경로 강제 트리거: Oracle client + MariaDB 서버.
        let a = build_flags(
            &server(ServerKind::MariaDb, 10),
            ClientKind::OracleMySql,
            false,
        );
        assert!(has(&a, "--column-statistics=0"));
    }

    #[test]
    fn t7_oracle_client_old_server_adds_column_statistics() {
        let a = build_flags(
            &server(ServerKind::OracleMySql, 5),
            ClientKind::OracleMySql,
            false,
        );
        assert!(has(&a, "--column-statistics=0"));
    }

    #[test]
    fn mysql8_oracle_client_omits_column_statistics() {
        // MySQL 8 서버는 컬럼통계 테이블이 있으므로 =0 불필요.
        let a = build_flags(
            &server(ServerKind::OracleMySql, 8),
            ClientKind::OracleMySql,
            false,
        );
        assert!(!has(&a, "--column-statistics=0"));
    }

    #[test]
    fn t8_mariadb_client_never_gets_column_statistics() {
        // 어떤 서버 조합이든 MariaDB 바이너리엔 이 플래그가 절대 실리지 않는다(unknown option 방지, R4).
        for major in [5, 8, 10, 11] {
            let a = build_flags(
                &server(ServerKind::MariaDb, major),
                ClientKind::MariaDb,
                false,
            );
            assert!(
                a.iter().all(|f| !f.starts_with("--column-statistics")),
                "MariaDB client got column-statistics for major={major}"
            );
        }
    }

    #[test]
    fn column_statistics_gated_by_client_kind_not_server_kind() {
        // 회귀 가드(R4): server.kind만 보면 MariaDB 서버라 =0을 넣고 싶겠지만, client가 MariaDB면 금지.
        let maria_client =
            build_flags(&server(ServerKind::MariaDb, 10), ClientKind::MariaDb, false);
        assert!(maria_client
            .iter()
            .all(|f| !f.starts_with("--column-statistics")));
    }

    #[test]
    fn t10_oracle_client_binlog_uses_source_data() {
        let a = build_flags(
            &server(ServerKind::OracleMySql, 8),
            ClientKind::OracleMySql,
            true,
        );
        assert!(has(&a, "--source-data=2"));
        assert!(!has(&a, "--master-data=2"));
    }

    #[test]
    fn t11_mariadb_client_binlog_uses_master_data() {
        let a = build_flags(&server(ServerKind::MariaDb, 10), ClientKind::MariaDb, true);
        assert!(has(&a, "--master-data=2"));
        assert!(!has(&a, "--source-data=2"));
    }

    #[test]
    fn no_binlog_flag_when_capture_disabled() {
        let a = build_flags(
            &server(ServerKind::OracleMySql, 8),
            ClientKind::OracleMySql,
            false,
        );
        assert!(a
            .iter()
            .all(|f| f != "--source-data=2" && f != "--master-data=2"));
    }
}
