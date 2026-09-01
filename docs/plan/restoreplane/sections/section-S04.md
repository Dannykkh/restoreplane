# S04 — MySqlSource (BackupSource 두 번째 구현)

> 의존: S05(Pipeline — `DumpArtifact`/`DumpStream`/`DumpExit`/`PipelineCfg.fatal_patterns` 소비처).
> 레이어 3 · plan.md 슬라이스 3("MySQL 소스, 두 번째 BackupSource") · 목적은 트레이트 시임(seam) 검증 —
> S03(PostgresSource)이 정의한 `BackupSource` 계약이 근본적으로 다른 도구(mysqldump)에도 억지 없이
> 들어맞는지 증명한다.
> 관련 리뷰: domain-technical.md §2(MySQL/MariaDB), plan.md Critical Findings 매핑의
> "DT 엔진스캔 → MyISAM 경고, non_transactional_tables" / "DT 버전 → 매 run 재검증 하드페일".

## Background

PostgreSQL과 달리 MySQL/MariaDB 생태계는 **같은 이름(mysqldump)의 도구가 두 개의 다른 배포판**(Oracle
MySQL, MariaDB)에서 나오고, 그 둘이 지원하는 플래그 집합이 다르다. 이 섹션은 domain-technical.md §2가
지적한 다섯 가지 함정을 하나의 `MySqlSource` 구현으로 흡수한다.

1. **`--single-transaction`의 컨센시 경계** — InnoDB에서만 REPEATABLE READ 스냅샷을 보장한다. MyISAM
   테이블은 트랜잭션이 없으므로 덤프 도중 변경되면 **에러 없이 조용히** 비일관 데이터가 들어간다.
   사후에 알 방법이 없으므로, **덤프 실행 전** `information_schema.tables` 엔진 스캔으로 비-InnoDB
   테이블을 찾아 매니페스트에 기록하고 경고해야 한다(§2.1).
2. **기본값이 서로 다른 세 플래그** — `--routines`/`--events`는 기본 OFF, `--triggers`는 기본 ON이다.
   세 값을 명시적으로 통일해 관리하지 않으면 저장 프로시저/함수/이벤트가 조용히 누락된다(§2.2).
3. **`--column-statistics`** — Oracle MySQL 8.0 client에만 존재하는 플래그다. MariaDB 서버 대상이거나
   구버전(8.0 미만) MySQL 서버 대상이면 컬럼 통계 테이블이 없어 에러가 난다. 이 플래그 자체가
   **MariaDB mysqldump 바이너리에는 없다** — 서버 종류가 아니라 **어느 클라이언트 바이너리를 쓰는지**로
   분기해야 한다(§2.2/§2.5, 아래 구현에서 흔한 오분기를 별도로 짚는다).
4. **GTID/binlog 좌표** — 이 프로젝트는 PITR을 스코프 밖으로 확정했지만, `--source-data=2`(또는
   MariaDB의 `--master-data=2`)로 덤프 시점의 binlog 좌표/GTID를 저비용으로 캡처해두지 않으면 그 시점은
   **영구히 재현 불가능한 앵커 공백**이 된다(§2.3, 비가역성).
5. **DDL-during-dump** — 덤프 도중 `ALTER TABLE` 등 DDL이 실행되면 mysqldump가 에러로 죽거나 일부
   테이블만 새 스키마로 담긴 채 끝날 수 있다. 이건 **부분 성공으로 취급하면 안 되고 전체 실패**로
   catalog에 기록해야 한다(§2.1 DDL 이슈).

`MySqlSource`는 S03이 정의한 `BackupSource` 트레이트(plan.md §확정 아키텍처, architecture.md P1-1)를
그대로 구현한다 — 이 문서는 자립성을 위해 그 계약을 다시 기술하되, 정식 소유권은 S03에 있다.

## Requirements

| # | 요구 | 근거 |
|---|------|------|
| R1 | 덤프 실행 **전** `information_schema.tables` 엔진 스캔 → 비-InnoDB 테이블을 `non_transactional_tables`로 매니페스트에 기록, 비어있어도 명시적 `[]` | §2.1, DT 엔진스캔 |
| R2 | `--single-transaction --routines --triggers --events`를 항상 명시(기본값 불일치 회피) | §2.2 |
| R3 | `--default-character-set=utf8mb4` 고정 + DB의 실제 charset/collation을 매니페스트에 캡처 | §2.4 |
| R4 | `--column-statistics=0`은 **client_kind**(바이너리 종류)로 분기하지, server_kind만으로 분기하지 않음 | §2.2/§2.5 |
| R5 | `SELECT VERSION()`에 `MariaDB` 문자열 포함 여부로 서버 종류 감지, 결과를 `db_engine`(mysql\|mariadb)로 매니페스트에 반영 | §2.5 |
| R6 | mysqldump 바이너리를 **MariaDB축 / Oracle MySQL축** 별도 매트릭스에서 선택, 미매칭(서버 메이저 ≥ 번들 최대)이면 "가장 가까운 걸로 시도" 금지, 하드 실패 | §2.5, PG §1.6과 동일 원칙 |
| R7 | 매 `run`마다(설치 시점 1회가 아니라) 버전/엔진 재감지 | DT 버전 재검증 |
| R8 | `--source-data=2`(Oracle) / `--master-data=2`(MariaDB)로 binlog 좌표·GTID를 캡처해 매니페스트 `binlog_coordinates`/`gtid_set`에 저장(PITR 미구현이어도) | §2.3 |
| R9 | DDL-race stderr 패턴을 감지해 S05 `PipelineCfg.fatal_patterns`에 병합 — exit=0이어도 하드 실패로 강등 | §2.1 DDL, S05 R2 |
| R10 | 비밀번호를 mysqldump argv에 노출하지 않음(`MYSQL_PWD` env 사용) | S11 정합(비밀 스크럽) |
| R11 | `DumpArtifact::Stream`으로 stdout을 그대로 S05 파이프라인에 전달(추가 버퍼링/디스크 스필 없음) | plan §파이프라인, M2 |
| R12 | 대상 DB 이름이 시스템 스키마(`mysql`/`information_schema`/`performance_schema`/`sys`)이면 즉시 거부 | 방어적 설정 오류 차단 |

## Dependencies

- **입력(S01 Config/CLI)** — `add-db`로 등록된 MySQL 연결 정보(host/port/user/database), 번들
  mysqldump 바이너리 경로 매트릭스. **S01의 기존 "PG 버전 매트릭스"에 MariaDB/Oracle MySQL 축을
  별도로 추가해야 한다** — 이 축을 놓치면 PG만 다버전 지원하고 MySQL은 단일 바이너리로 오분기된다
  (domain-technical.md §2.5의 명시적 경고).
- **출력 소비(S05 Pipeline)** — `spawn()`이 반환하는 `DumpArtifact::Stream`을 그대로 소비. `wait`
  퓨처의 `DumpExit`을 `is_fatal_stderr(stderr_tail, cfg.fatal_patterns)`로 검사하므로, `MySqlSource`는
  DDL-race 패턴 목록을 **cfg 쪽에 병합**하는 방식으로 개입한다(직접 `DumpExit`을 조작하지 않음).
- **출력 소비(S08 Catalog/Manifest)** — `MySqlPreDumpReport`를 `MySqlManifestFields`로 변환해
  `Manifest`의 `non_transactional_tables`/`charset`/`collation`/`dump_flags`/`binlog_coordinates`/
  `gtid_set`/`server_version`/`db_engine` 필드에 채운다.
- **경계 규칙**: S04는 압축/암호화/스풀/서명을 하지 않는다(S05/S02/S08 소관). S04의 책임은 정확히
  "mysqldump를 올바른 플래그로 실행하고, 실행 전후 도메인 지식(엔진/버전/DDL레이스)을 S05/S08이
  쓸 수 있는 형태로 노출"하는 것으로 한정한다.

## Reference Libraries

| 크레이트 | 버전 | 역할 | 주의 |
|---------|------|------|------|
| `sqlx` | 0.8.x | `MySqlPool` — 엔진 스캔/버전 감지/charset 조회(런타임 쿼리, 컴파일타임 매크로 아님) | `features = ["mysql","runtime-tokio","tls-native-tls"]`. 커넥션 풀은 짧은 조회용(작게 유지, 대용량 덤프 자체는 mysqldump가 별도 접속으로 수행) |
| `tokio` | 1.x | `process::Command`(mysqldump 기동), `io::AsyncRead` | S05와 동일 피처셋 공유(`process`,`io-util`,`rt-multi-thread`) |
| `regex` + `once_cell` | 1.x / 1.x | `VERSION()` 원문에서 major.minor.patch 접두 추출 | MySQL 버전 문자열은 semver 엄격 파서로 못 읽음(`8.0.37-0ubuntu...`, `10.11.6-MariaDB-...`) — 접두 정규식이 더 견고 |
| `uuid` | 1.x | `DumpPlanPart.backup_id` 발급 | v4 |
| `tracing` | 0.1 | 엔진 스캔 경고, DDL-race 감지 로그 | `warn!`에 비밀번호/연결 문자열 포함 금지(S11) |
| `anyhow` + `thiserror` | 1.x / 2.x | 에러 처리(다른 섹션과 통일) | |
| `serde` | 1.x | `BinlogAnchor`/`MySqlManifestFields` 직렬화(S08 Manifest 필드로 흘려보냄) | |

`Cargo.toml`(발췌):
```toml
[dependencies]
sqlx = { version = "0.8", default-features = false, features = ["mysql","runtime-tokio","tls-native-tls"] }
tokio = { version = "1", features = ["rt-multi-thread","process","io-util","macros","sync"] }
regex = "1"
once_cell = "1"
uuid = { version = "1", features = ["v4"] }
tracing = "0.1"
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
```

> `[확인 필요]`: sqlx 0.8의 MySQL 드라이버가 대상 서버(구버전 MySQL 5.7, MariaDB 10.x)와의 handshake
> 호환성을 완전히 커버하는지는 실제 서버군 확정 후 검증. 불확실하면 엔진 스캔/버전 감지 전용 조회는
> `mysql_async`로 교체 가능하도록 `MySqlMetaConn` 트레이트로 얇게 감싼다(아래 구현 참고).

## Implementation

### 모듈 구조

```
src/source/mysql/
  mod.rs        // MySqlSource, BackupSource 구현, MySqlPreDumpReport
  version.rs    // ServerKind/ServerVersionInfo, detect_server_version, parse_semver_prefix
  binary.rs     // ClientKind, MysqldumpBinaryMatrix::resolve (하드페일-not-nearest)
  engine_scan.rs// NonTransactionalTable, scan_non_transactional_tables, fetch_db_charset
  flags.rs      // build_flags (column-statistics/source-data 분기), DDL_RACE_FATAL_PATTERNS
  anchor.rs     // AnchorScanningReader, BinlogAnchor (§2.3 tee)
  proc.rs       // spawn_mysqldump, drain_stderr_tail
```

### 1. 공유 트레이트 계약 (S03과 공유, 자립성을 위해 재기술)

```rust
// src/source/mod.rs (S03이 정식 소유)
#[async_trait::async_trait]
pub trait BackupSource: Send + Sync {
    /// 이 잡이 몇 개의 파트로 나뉘는지 계획한다. MySQL은 항상 1파트(globals 개념 없음, plan.md §파이프라인 1).
    /// 서버 접속이 필요하므로 Result.
    fn plan(&self) -> anyhow::Result<DumpPlan>;

    /// 파트 1개를 실행해 DumpArtifact를 반환한다. **동기 함수** — mysqldump 기동(`Command::spawn`) 자체는
    /// 논블로킹이고, 실제 I/O는 반환된 `wait`/`reader`가 담당한다(S05가 `source.spawn(&part)?`로 호출).
    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact>;
}

pub struct DumpPlan {
    pub parts: Vec<DumpPlanPart>,
}

pub struct DumpPlanPart {
    pub backup_id: String,     // UUID — S05가 스풀 파일명에 사용
    pub kind: DumpPartKind,
    pub db_alias: String,      // add-db 등록 이름(매니페스트 db_alias)
    pub job_name: String,
}

pub enum DumpPartKind {
    Globals,             // PostgreSQL 전용(S03) — MySqlSource는 이 분기를 만들지 않음
    Database(String),    // 실제 DB 이름
}
```

`DumpArtifact`/`DumpStream`/`DumpExit`은 S05가 소유한 타입을 그대로 재사용한다(정의는 S05 §1 참조).

### 2. 서버 종류/버전 감지 (version.rs, R5/R7)

```rust
use once_cell::sync::Lazy;
use regex::Regex;
use sqlx::MySqlPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    OracleMySql,
    MariaDb,
}

#[derive(Debug, Clone)]
pub struct ServerVersionInfo {
    pub raw: String,   // SELECT VERSION() 원문(매니페스트 server_version에 그대로 저장)
    pub kind: ServerKind,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// "8.0.37-0ubuntu0.22.04.1" / "10.11.6-MariaDB-1:10.11.6+maria~ubu2204" 앞부분만 추출.
/// semver 크레이트를 쓰지 않는 이유: 두 배포판 모두 접미사가 엄격 semver 문법을 어긴다.
static VERSION_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\d+)\.(\d+)\.(\d+)").unwrap());

fn parse_semver_prefix(raw: &str) -> Option<(u32, u32, u32)> {
    let c = VERSION_RE.captures(raw)?;
    Some((c[1].parse().ok()?, c[2].parse().ok()?, c[3].parse().ok()?))
}

/// 매 run마다 재호출(DT 버전 재검증 — init 1회성 설정이 아님, R7).
pub async fn detect_server_version(pool: &MySqlPool) -> anyhow::Result<ServerVersionInfo> {
    let raw: String = sqlx::query_scalar("SELECT VERSION()").fetch_one(pool).await?;
    let kind = if raw.to_ascii_uppercase().contains("MARIADB") {
        ServerKind::MariaDb
    } else {
        ServerKind::OracleMySql
    };
    let (major, minor, patch) = parse_semver_prefix(&raw)
        .ok_or_else(|| anyhow::anyhow!("VERSION() 파싱 실패, 원문 유지 필요: {raw}"))?;
    Ok(ServerVersionInfo { raw, kind, major, minor, patch })
}
```

### 3. 바이너리 매트릭스 — MariaDB축 / Oracle MySQL축 분리 (binary.rs, R6)

PG 리뷰(§1.6)와 동일한 원칙: **매칭되는 번들 클라이언트가 없으면 "가장 가까운 걸로 시도"가 아니라
하드 실패**. MySQL/MariaDB는 이 매트릭스가 PG축과 **직교하는 별도 축**이라는 점이 함정이다 — 하나의
"MySQL 매트릭스"에 두 배포판을 욱여넣으면 안 된다.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    OracleMySql,
    MariaDb,
}

/// S01 Config에서 로드되는 번들 바이너리 경로표. 메이저 버전 → 실행 파일 경로.
pub struct MysqldumpBinaryMatrix {
    pub oracle_mysql: std::collections::BTreeMap<u32, std::path::PathBuf>,
    pub mariadb: std::collections::BTreeMap<u32, std::path::PathBuf>,
}

impl MysqldumpBinaryMatrix {
    /// 서버 종류에 대응하는 축에서, 서버 메이저 버전 이상인 최소 클라이언트를 고른다.
    /// 없으면 하드 실패(클라이언트가 서버보다 오래된 채로 강행하지 않음 — PG §1.6과 동일 원칙).
    pub fn resolve(&self, server: &ServerVersionInfo) -> anyhow::Result<(ClientKind, std::path::PathBuf)> {
        let (table, kind) = match server.kind {
            ServerKind::OracleMySql => (&self.oracle_mysql, ClientKind::OracleMySql),
            ServerKind::MariaDb => (&self.mariadb, ClientKind::MariaDb),
        };
        let (_, path) = table
            .range(server.major..)
            .next()
            .ok_or_else(|| anyhow::anyhow!(
                "번들 mysqldump 없음: server_kind={:?} server_major={} — 매트릭스에 이 서버를 커버하는 \
                 클라이언트가 없음(가장 가까운 걸로 강행하지 않음, 하드 실패)",
                server.kind, server.major
            ))?;
        Ok((kind, path.clone()))
    }
}
```

### 4. PRE-DUMP 엔진 스캔 + charset 캡처 (engine_scan.rs, R1/R3/R12)

```rust
use sqlx::MySqlPool;

pub struct NonTransactionalTable {
    pub schema: String,
    pub table: String,
    pub engine: String,
}

const SYSTEM_SCHEMAS: &[&str] = &["mysql", "information_schema", "performance_schema", "sys"];

/// mysqldump 실행 **전**에 반드시 호출한다. 여기서 발견된 테이블은 `--single-transaction`의
/// 컨센시 보장 밖이며(§2.1), 사후에는 알 방법이 없다 — 그래서 "사전" 스캔이어야 한다.
pub async fn scan_non_transactional_tables(
    pool: &MySqlPool,
    db_name: &str,
) -> anyhow::Result<Vec<NonTransactionalTable>> {
    anyhow::ensure!(
        !SYSTEM_SCHEMAS.contains(&db_name),
        "add-db 대상이 시스템 스키마입니다: {db_name}"
    ); // R12: 방어적 설정 오류 차단

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        r#"
        SELECT table_schema, table_name, engine
        FROM information_schema.tables
        WHERE table_schema = ?
          AND table_type = 'BASE TABLE'
          AND engine IS NOT NULL
          AND engine NOT IN ('InnoDB')
          AND table_schema NOT IN ('mysql','information_schema','performance_schema','sys')
        "#,
    )
    .bind(db_name)
    .fetch_all(pool)
    .await?;
    // table_schema=? 로 대상 DB만 스캔하면서도 시스템 스키마 제외를 중복 유지하는 이유:
    // db_name 자체가 실수로 시스템 스키마와 같은 값으로 설정돼도 이중으로 막기 위함(belt-and-suspenders).

    Ok(rows
        .into_iter()
        .map(|(schema, table, engine)| NonTransactionalTable { schema, table, engine })
        .collect())
}

/// §2.4 mojibake 추적용. `--default-character-set=utf8mb4`로 덤프를 고정하더라도,
/// 원본이 실제로 어떤 charset/collation이었는지 매니페스트에 남겨야 복원 후 문제 원인 추적이 가능하다.
pub async fn fetch_db_charset(pool: &MySqlPool, db_name: &str) -> anyhow::Result<(String, String)> {
    let row: (String, String) = sqlx::query_as(
        "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME \
         FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
    )
    .bind(db_name)
    .fetch_one(pool)
    .await?;
    Ok(row)
}
```

### 5. 플래그 조립 (flags.rs, R2/R4/R8/R9)

```rust
/// mysqldump 인자를 조립한다. client_kind(실제 실행할 바이너리 종류)로 분기해야 하는 이유는
/// §2.5 주석 참조 — server_kind만으로 분기하면 MariaDB 바이너리에 존재하지 않는 플래그를 넘겨
/// "unknown option"으로 죽는다.
pub fn build_flags(
    server: &ServerVersionInfo,
    client_kind: ClientKind,
    db_name: &str,
    capture_binlog: bool,
) -> Vec<String> {
    let mut args = vec![
        "--single-transaction".to_string(), // InnoDB REPEATABLE READ 스냅샷(§2.1)
        "--routines".to_string(),           // 기본 OFF — 명시 필요(§2.2)
        "--triggers".to_string(),           // 기본 ON이지만 통일을 위해 명시(§2.2)
        "--events".to_string(),             // 기본 OFF — 명시 필요(§2.2)
        "--default-character-set=utf8mb4".to_string(), // §2.4
    ];

    // --column-statistics: Oracle MySQL 8.0 client 전용 플래그. MariaDB 바이너리에는 이 옵션
    // 자체가 없어 넘기면 즉시 에러 종료한다. binary.rs의 resolve()가 server_kind==client_kind
    // 축을 강제하므로 (server.kind == MariaDb) 분기는 정상 경로에서는 도달하지 않지만,
    // 향후 매트릭스가 완화되는 경우를 대비해 방어적으로 남겨둔다.
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

    args.push(db_name.to_string());
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
```

### 6. binlog 좌표/GTID 앵커 캡처 (anchor.rs, R8)

`--source-data=2`/`--master-data=2`는 좌표를 덤프 **본문 상단 주석**(`-- CHANGE MASTER TO ...`,
`SET @@GLOBAL.GTID_PURGED=...`)으로 남긴다. 이걸 매니페스트 필드로도 뽑아내려면 암호화된 스풀을
나중에 열어 파싱하는 대신, **스트림이 파이프라인으로 흘러가는 동시에** 헤더 구간만 훔쳐보는 tee가
필요하다 — S05의 `HashingWriter`(blake3 tee)와 동일한 패턴을 읽기 쪽에 적용한다.

```rust
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// 헤더가 보통 수 KB 안에 있으므로, 그 이후는 스캔을 끄고 순수 pass-through로 전환한다
/// (수 GB 스트림 전체를 문자열 스캔하지 않기 위한 상한).
const ANCHOR_SCAN_CAP: usize = 64 * 1024;

#[derive(Default, Clone, Debug, serde::Serialize)]
pub struct BinlogAnchor {
    pub binlog_coordinates: Option<String>, // "CHANGE MASTER TO MASTER_LOG_FILE=..., MASTER_LOG_POS=...;"
    pub gtid_set: Option<String>,           // "SET @@GLOBAL.GTID_PURGED='...';"
}

/// mysqldump stdout을 감싸 바이트를 있는 그대로 통과시키면서, 첫 ANCHOR_SCAN_CAP 바이트에서
/// 좌표/GTID 주석 라인을 찾아 anchor에 채운다. 스트림 내용을 절대 변형하지 않는다(순수 tee).
pub struct AnchorScanningReader<R> {
    inner: R,
    scan_buf: Vec<u8>,
    scan_done: bool,
    anchor: Arc<Mutex<BinlogAnchor>>,
}

impl<R> AnchorScanningReader<R> {
    pub fn new(inner: R, anchor: Arc<Mutex<BinlogAnchor>>) -> Self {
        Self { inner, scan_buf: Vec::with_capacity(ANCHOR_SCAN_CAP), scan_done: false, anchor }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for AnchorScanningReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut self.inner).poll_read(cx, buf);
        if res.is_ready() && res.as_ref().map(|r| r.is_ok()).unwrap_or(false) && !self.scan_done {
            let chunk = &buf.filled()[before..];
            let remaining = ANCHOR_SCAN_CAP.saturating_sub(self.scan_buf.len());
            let take = remaining.min(chunk.len());
            self.scan_buf.extend_from_slice(&chunk[..take]);

            if let Ok(text) = std::str::from_utf8(&self.scan_buf) {
                let mut anchor = self.anchor.lock().unwrap();
                if anchor.binlog_coordinates.is_none() {
                    anchor.binlog_coordinates = extract_stmt(text, "CHANGE MASTER TO");
                }
                if anchor.gtid_set.is_none() {
                    anchor.gtid_set = extract_stmt(text, "GTID_PURGED=");
                }
            }
            if self.scan_buf.len() >= ANCHOR_SCAN_CAP || {
                let a = self.anchor.lock().unwrap();
                a.binlog_coordinates.is_some() && a.gtid_set.is_some()
            } {
                self.scan_done = true;
                self.scan_buf = Vec::new(); // 스캔 종료 후 메모리 반환
            }
        }
        res
    }
}

/// marker가 포함된 지점부터 다음 `;`까지를 한 문장으로 추출.
fn extract_stmt(text: &str, marker: &str) -> Option<String> {
    let start = text.find(marker)?;
    let stmt_start = text[..start].rfind(['\n', '\r']).map(|p| p + 1).unwrap_or(0);
    let end = text[start..].find(';').map(|e| start + e + 1).unwrap_or(text.len());
    Some(text[stmt_start..end].trim().to_string())
}
```

### 7. 자식 프로세스 기동 (proc.rs, R10)

```rust
use std::process::Stdio;
use tokio::process::{Child, Command};
use tokio::io::{AsyncBufReadExt, BufReader};
use std::collections::VecDeque;

pub struct MySqlConnConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: zeroize::Zeroizing<String>, // S02 규율과 동일하게 zeroize(값 자체는 S01 Config가 관리)
    pub database: String,
}

/// 비밀번호를 argv에 절대 넣지 않는다 — `ps`/작업 관리자에 평문 노출 방지(R10, S11 정합).
pub fn spawn_mysqldump(
    binary: &std::path::Path,
    args: &[String],
    conn: &MySqlConnConfig,
) -> anyhow::Result<Child> {
    let mut cmd = Command::new(binary);
    cmd.args(args)
        .arg(format!("--host={}", conn.host))
        .arg(format!("--port={}", conn.port))
        .arg(format!("--user={}", conn.user))
        .env("MYSQL_PWD", conn.password.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(cmd.spawn()?)
}

/// stderr를 별도로 계속 비우면서(파이프 풀로 인한 데드락 방지) 최근 N줄만 유지.
pub async fn drain_stderr_tail(stderr: tokio::process::ChildStderr, cap_lines: usize) -> String {
    let mut lines = BufReader::new(stderr).lines();
    let mut ring: VecDeque<String> = VecDeque::with_capacity(cap_lines);
    while let Ok(Some(line)) = lines.next_line().await {
        if ring.len() == cap_lines {
            ring.pop_front();
        }
        ring.push_back(line);
    }
    ring.into_iter().collect::<Vec<_>>().join("\n")
}
```

### 8. `MySqlSource` 조립 + `BackupSource` 구현 (mod.rs)

트레이트 시그니처(`fn spawn(&self, ..) -> anyhow::Result<DumpArtifact>`)는 동기다. 하지만 엔진
스캔/버전 감지/charset 조회는 `sqlx` 비동기 쿼리다. `tokio::task::block_in_place` +
`Handle::current().block_on`으로 안전하게 구동한다 — **일반 `block_on`을 이미 실행 중인 async
컨텍스트 안에서 직접 호출하면 panic**하지만, `block_in_place`는 멀티스레드 런타임(S05 Cargo.toml이
이미 `rt-multi-thread`를 요구)에서 현재 워커 스레드를 블로킹 전용으로 전환해 안전하게 우회한다.

```rust
use std::sync::{Arc, Mutex};
use sqlx::MySqlPool;

/// 동기 트레이트 메서드 안에서 async DB 조회를 구동하는 유틸.
/// 전제: 멀티스레드 tokio 런타임(S05가 이미 요구) — current-thread 런타임에서는 panic한다.
fn block_on_pool<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

pub struct MySqlSource {
    pub job_name: String,
    pub db_alias: String,
    pub pool: MySqlPool,               // 메타데이터 조회 전용(엔진 스캔/버전/charset)
    pub conn: MySqlConnConfig,         // mysqldump CLI 인증용
    pub binary_matrix: MysqldumpBinaryMatrix,
    pub capture_binlog: bool,          // R8, 기본 true 권장(비가역성)
    /// spawn() 직후 채워지는 사이드채널. 트레이트 시그니처가 DumpArtifact만 반환하도록 고정돼
    /// 있어(S05 계약), 매니페스트용 도메인 메타데이터는 이 채널로 노출하고 커밋 직후 회수한다.
    pre_dump_report: Arc<Mutex<Option<MySqlPreDumpReport>>>,
}

#[derive(Clone)]
pub struct MySqlPreDumpReport {
    pub server_version: ServerVersionInfo,
    pub client_kind: ClientKind,
    pub charset: String,
    pub collation: String,
    pub non_transactional_tables: Vec<NonTransactionalTable>,
    pub dump_flags: String, // 실행된 전체 커맨드라인 원문(비밀번호 제외, 매니페스트 dump_flags)
    pub binlog_anchor: Arc<Mutex<BinlogAnchor>>, // 스트림 소비가 끝난 뒤 값이 채워짐
}

#[async_trait::async_trait]
impl BackupSource for MySqlSource {
    fn plan(&self) -> anyhow::Result<DumpPlan> {
        // MySQL은 항상 단일 파트(plan.md §파이프라인 1: "MySQL이면 [db] (+엔진 스캔)") — globals 없음.
        Ok(DumpPlan {
            parts: vec![DumpPlanPart {
                backup_id: uuid::Uuid::new_v4().to_string(),
                kind: DumpPartKind::Database(self.conn.database.clone()),
                db_alias: self.db_alias.clone(),
                job_name: self.job_name.clone(),
            }],
        })
    }

    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
        let db_name = match &part.kind {
            DumpPartKind::Database(n) => n.clone(),
            DumpPartKind::Globals => anyhow::bail!("MySqlSource는 globals part를 지원하지 않음(PG 전용)"),
        };

        // (1) 매 run 재검증 — 설치 시점 1회성 아님(R7).
        let server = block_on_pool(detect_server_version(&self.pool))?;
        let (client_kind, binary) = self.binary_matrix.resolve(&server)?; // R6: 미매칭 하드 실패

        // (2) PRE-DUMP 엔진 스캔 — 덤프 시작 전에 반드시 완료(R1).
        let non_tx = block_on_pool(scan_non_transactional_tables(&self.pool, &db_name))?;
        if !non_tx.is_empty() {
            tracing::warn!(
                db = %db_name,
                count = non_tx.len(),
                tables = ?non_tx.iter().map(|t| format!("{}.{}({})", t.schema, t.table, t.engine)).collect::<Vec<_>>(),
                "non-InnoDB 테이블 발견 — --single-transaction 컨센시 보장 밖, 덤프 도중 변경 시 조용히 비일관 가능"
            );
        }

        // (3) charset/collation 캡처(R3).
        let (charset, collation) = block_on_pool(fetch_db_charset(&self.pool, &db_name))?;

        // (4) 플래그 조립(R2/R4/R8).
        let args = build_flags(&server, client_kind, &db_name, self.capture_binlog);
        let dump_flags_repr = format!(
            "{} {} --host=<redacted> --port=<redacted> --user=<redacted>",
            binary.display(),
            args.join(" ")
        ); // 매니페스트 dump_flags — 연결정보는 재현성에 불필요, secret 스크럽(S11 정합)

        // (5) 자식 프로세스 기동(R10).
        let mut child = spawn_mysqldump(&binary, &args, &self.conn)?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // (6) binlog 앵커 tee로 감싸되 바이트는 원본 그대로 파이프라인에 전달(R8, R11).
        let anchor = Arc::new(Mutex::new(BinlogAnchor::default()));
        let reader = AnchorScanningReader::new(stdout, anchor.clone());

        let wait = Box::pin(async move {
            let stderr_tail = drain_stderr_tail(stderr, 200).await;
            let status = child.wait().await?;
            Ok(DumpExit {
                code: status.code(),
                success: status.success(),
                stderr_tail,
            })
        });

        *self.pre_dump_report.lock().unwrap() = Some(MySqlPreDumpReport {
            server_version: server,
            client_kind,
            charset,
            collation,
            non_transactional_tables: non_tx,
            dump_flags: dump_flags_repr,
            binlog_anchor: anchor,
        });

        Ok(DumpArtifact::Stream(DumpStream {
            reader: Box::new(reader),
            wait,
        }))
    }
}

impl MySqlSource {
    /// S05 PipelineCfg 구성 시 이 잡의 fatal_patterns에 병합해야 하는 MySQL 전용 패턴(R9).
    pub fn fatal_patterns() -> Vec<String> {
        DDL_RACE_FATAL_PATTERNS.iter().map(|s| s.to_string()).collect()
    }

    /// 커밋(SpoolCommit) 직후, 같은 backup_id 처리 흐름 안에서 정확히 1회 호출해 매니페스트용
    /// 메타데이터를 회수한다. spawn()을 다시 호출하면 이전 값은 덮어써진다(잡당 1파트이므로 안전).
    pub fn take_pre_dump_report(&self) -> Option<MySqlPreDumpReport> {
        self.pre_dump_report.lock().unwrap().take()
    }
}
```

### 9. 매니페스트 필드 변환 (S08 `Manifest`로 흘려보내기)

```rust
pub struct MySqlManifestFields {
    pub db_engine: String,                     // "mysql" | "mariadb"
    pub server_version: String,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub non_transactional_tables: Vec<String>,  // ["app.legacy_log(MyISAM)", ...] 비었으면 []
    pub dump_tool: String,                      // "mysqldump"
    pub dump_flags: String,
    pub binlog_coordinates: Option<String>,
    pub gtid_set: Option<String>,
}

impl MySqlPreDumpReport {
    /// 스트림 소비(=커밋)가 끝난 뒤 호출해야 binlog_anchor가 채워져 있다(§6 tee가 헤더 구간까지
    /// 읽힌 뒤에만 값이 존재).
    pub fn into_manifest_fields(self) -> MySqlManifestFields {
        let anchor = self.binlog_anchor.lock().unwrap().clone();
        MySqlManifestFields {
            db_engine: match self.server_version.kind {
                ServerKind::MariaDb => "mariadb".to_string(),
                ServerKind::OracleMySql => "mysql".to_string(),
            },
            server_version: self.server_version.raw,
            charset: Some(self.charset),
            collation: Some(self.collation),
            non_transactional_tables: self
                .non_transactional_tables
                .into_iter()
                .map(|t| format!("{}.{}({})", t.schema, t.table, t.engine))
                .collect(),
            dump_tool: "mysqldump".to_string(),
            dump_flags: self.dump_flags,
            binlog_coordinates: anchor.binlog_coordinates,
            gtid_set: anchor.gtid_set,
        }
    }
}
```

S08의 `Manifest` 구조체(§8.2)에 이 필드들을 그대로 매핑한다: `non_transactional_tables`,
`charset`/`collation`, `dump_flags`, `binlog_coordinates`, `gtid_set`, `server_version`, `db_engine`.

## Test Scenarios

| # | 시나리오 | 기대 |
|---|---------|------|
| T1 | MyISAM 테이블 1개 포함 DB 스캔 | `non_transactional_tables`에 1건, `warn!` 로그 발생 |
| T2 | 전 테이블 InnoDB인 DB 스캔 | 빈 벡터(매니페스트엔 명시적 `[]`) |
| T3 | `db_name`을 `mysql`로 설정 후 스캔 시도 | `R12` 위반으로 즉시 에러(방어적 거부) |
| T4 | `SELECT VERSION()`이 `10.11.6-MariaDB-...` | `ServerKind::MariaDb`, major=10 감지 |
| T5 | `SELECT VERSION()`이 `8.0.37-0ubuntu...` | `ServerKind::OracleMySql`, major=8 감지 |
| T6 | Oracle client, 서버 MariaDB(방어 경로 강제 트리거) | `--column-statistics=0` 추가됨(R4) |
| T7 | Oracle client, server major=5(구버전) | `--column-statistics=0` 추가됨(R4) |
| T8 | MariaDB client, 어떤 서버든 | `--column-statistics` 플래그 자체가 인자에 없음(unknown option 방지) |
| T9 | binary_matrix에 서버 메이저를 커버하는 클라이언트 없음 | `resolve()` 하드 실패, "가장 가까운 것"으로 강행하지 않음(R6) |
| T10 | `capture_binlog=true`, Oracle client | 인자에 `--source-data=2` 포함 |
| T11 | `capture_binlog=true`, MariaDB client | 인자에 `--master-data=2` 포함(`--source-data` 아님) |
| T12 | fake stdout에 `CHANGE MASTER TO ...;` 헤더 삽입 후 스트림 소비 | `binlog_coordinates`에 해당 문장 캡처, 바이트 스트림 자체는 변형 없이 통과 |
| T13 | 헤더에 좌표 없이 65KB 이후 유사 텍스트 등장 | 스캔 상한 초과로 캡처 안 됨(의도된 상한 동작 확인) |
| T14 | mysqldump가 exit=1 | S05 `is_fatal_stderr` 이전에 이미 exit≠0로 FAIL(R2 상속) |
| T15 | mysqldump가 exit=0이지만 stderr에 "Lost connection to MySQL server during query when dumping table" 포함 | `MySqlSource::fatal_patterns()`가 병합된 cfg에서 FAIL로 강등(R9) |
| T16 | `spawn_mysqldump` 호출 후 프로세스 인자 덤프 | argv에 비밀번호 리터럴 없음(`MYSQL_PWD` env로만 전달, R10) |
| T17 | DB 실제 charset=`latin1`, collation 캡처 | 매니페스트 `charset="latin1"`로 정확히 기록(§2.4 추적성) |
| T18 | `plan()` 호출 | 항상 파트 1개, `DumpPartKind::Database(..)`(globals 없음) |
| T19 | 동일 `MySqlSource`로 `spawn()` 2회 연속 호출(재시도 시나리오) | 두 번째 호출의 `pre_dump_report`가 첫 번째 값을 덮어씀(잡당 1파트라 안전, 문서화된 계약) |

## Implementation Strategy

1. **version.rs + binary.rs 먼저** — 순수 로직(문자열 파싱, BTreeMap 조회), DB/프로세스 없이
   단위 테스트 가능(T4~T9). 하드 실패 경로(T9)를 조기 검증.
2. **engine_scan.rs** — sqlx 통합 테스트(로컬 MySQL/MariaDB 컨테이너 대상, T1~T3, T17). CI에
   두 서버(MySQL 8, MariaDB 10.x) 컨테이너를 병렬로 띄워 동일 테스트를 양쪽에서 돌린다(§2.5
   시스템 카탈로그 차이 조기 발견).
3. **flags.rs** — 순수 함수, T6~T11을 표 기반 단위 테스트로 커버(입력 조합 × 기대 플래그 목록).
4. **anchor.rs** — `AsyncRead` 목(mock) 스트림으로 T12/T13. 실제 자식 프로세스 없이 바이트 청크
   주입만으로 검증 가능(빠른 반복).
5. **proc.rs + mod.rs 통합** — 실제 mysqldump 바이너리(테스트 컨테이너)로 end-to-end(T14~T16, T18,
   T19). 여기서 S05 `PipelineCfg.fatal_patterns` 병합까지 포함한 통합 테스트로 T15를 검증.
6. **S08 연동** — `into_manifest_fields()` → `Manifest` 필드 매핑을 스냅샷 테스트로 고정.

## Quality Gate

- [ ] `cargo test -p source_mysql` 전 시나리오(T1~T19) 통과.
- [ ] `--column-statistics=0`이 **client_kind**로만 분기됨을 보장하는 단위 테스트 존재(server_kind만
      보고 분기하는 회귀를 잡음, R4).
- [ ] MySQL 8 컨테이너 + MariaDB 10.x 컨테이너 양쪽에서 CI 통합 테스트 통과(§2.5 카탈로그 차이).
- [ ] binary_matrix 미매칭 시 하드 실패하고 "가장 가까운 것"으로 폴백하는 코드 경로가 존재하지
      않음(grep으로 확인, R6).
- [ ] `spawn_mysqldump`가 생성한 `Command`의 `args()`에 비밀번호 문자열이 포함되지 않음(R10).
- [ ] `non_transactional_tables`가 항상 채워지거나(발견 시) 명시적 빈 배열로 매니페스트에 기록됨
      (필드 생략 없음, R1).
- [ ] DDL-race 패턴이 S05 `PipelineCfg.fatal_patterns`에 병합되는 호출 지점이 잡 조립 코드에
      존재함(R9, 단순 상수 정의만으로는 미충족).

## Risk & Rollback

| 리스크 | 완화 | 롤백 |
|-------|------|------|
| sqlx 0.8 MySQL 드라이버가 구버전 서버(5.7 이하)와 handshake 실패 `[확인 필요]` | 엔진 스캔/버전 감지 전용 조회를 `MySqlMetaConn` 트레이트로 추상화해 `mysql_async`로 교체 가능하게 유지 | 드라이버 교체는 `engine_scan.rs`/`version.rs` 내부에 국한, `MySqlSource` 공개 API 불변 |
| `--column-statistics` 분기를 server_kind로 잘못 구현(회귀) | Quality Gate 단위 테스트로 고정(client_kind 기준), 코드 리뷰 체크리스트에 §2.5 명시 | 플래그 조립 함수는 순수 함수라 즉시 되돌리기 가능 |
| binlog 앵커 스캔이 64KB 상한 내에 좌표를 못 찾음(비표준 mysqldump 출력 순서) | `binlog_coordinates`/`gtid_set`을 `Option`으로 두어 "캡처 실패"를 허용된 상태로 취급(하드 실패 아님 — PITR 자체가 스코프 밖이므로 앵커 부재가 백업 실패 사유는 아님) | 상한 값(`ANCHOR_SCAN_CAP`)을 늘리는 것으로 대응, 인터페이스 불변 |
| DDL-race stderr 패턴 목록이 mysqldump 버전마다 문구가 달라 누락 | `fatal_patterns()`를 설정 파일에서 오버라이드 가능한 형태로 확장 여지 남김(현재는 상수, 향후 config 이관 가능) | 패턴 추가는 상수 배열 수정만으로 완결, 로직 변경 없음 |
| MariaDB 전용 바이너리를 아직 번들하지 못해 `binary_matrix.mariadb`가 비어있음 | 하드 실패로 명확히 알림(§R6) — "Oracle 클라이언트로 대충 시도"하지 않음. 초기 슬라이스는 Oracle MySQL만 지원 범위로 좁히고 문서화 가능 | MariaDB 대상 잡을 config에서 비활성화, 나머지 MySQL 잡은 영향 없음 |
| `block_in_place`가 current-thread 런타임에서 panic | S05가 이미 `rt-multi-thread`를 요구하므로 프로젝트 전역에서 위반 불가능하도록 런타임 빌더를 한 곳(S01 진입점)에서만 생성 | 위반 시 즉시 panic으로 드러나 조용한 오동작 없음(fail-fast) |

## Acceptance Criteria

- AC1: 덤프 실행 전 엔진 스캔이 항상 먼저 수행되고, 비-InnoDB 테이블이 매니페스트
  `non_transactional_tables`에 정확히 기록된다(T1~T3, R1).
- AC2: `--single-transaction --routines --triggers --events`가 항상 명시적으로 포함되어 기본값
  불일치로 인한 조용한 누락이 없다(R2).
- AC3: `--column-statistics=0`이 client_kind(바이너리 종류)로만 분기되며, MariaDB 바이너리에는 이
  플래그가 절대 전달되지 않는다(T6~T8, R4).
- AC4: 서버 종류(MariaDB/Oracle MySQL)가 `VERSION()` 문자열로 매 run마다 재감지되고, 매칭되는 번들
  클라이언트가 없으면 하드 실패한다(T4/T5/T9, R5/R6/R7).
- AC5: `--source-data=2`/`--master-data=2`로 캡처된 binlog 좌표/GTID가 클라이언트 종류에 맞는 문법으로
  요청되고, 성공 시 매니페스트에 기록된다(T10~T13, R8).
- AC6: DDL-race stderr 패턴이 감지되면 exit code와 무관하게 전체 실패로 처리되고 부분 성공으로
  커밋되지 않는다(T15, R9).
- AC7: mysqldump 프로세스 argv 어디에도 비밀번호가 노출되지 않는다(T16, R10).
- AC8: `MySqlSource`가 S05의 `BackupSource`/`DumpArtifact` 계약을 변경 없이 구현해, 트레이트 시임이
  PostgresSource 전용으로 설계되지 않았음을 증명한다(모듈 전체, plan.md 슬라이스 3 목표).

## Files

| 파일 | 내용 |
|------|------|
| `src/source/mysql/mod.rs` | `MySqlSource`, `BackupSource` 구현, `MySqlPreDumpReport`, `MySqlManifestFields` |
| `src/source/mysql/version.rs` | `ServerKind`, `ServerVersionInfo`, `detect_server_version`, `parse_semver_prefix` |
| `src/source/mysql/binary.rs` | `ClientKind`, `MysqldumpBinaryMatrix::resolve` |
| `src/source/mysql/engine_scan.rs` | `NonTransactionalTable`, `scan_non_transactional_tables`, `fetch_db_charset` |
| `src/source/mysql/flags.rs` | `build_flags`, `DDL_RACE_FATAL_PATTERNS` |
| `src/source/mysql/anchor.rs` | `AnchorScanningReader`, `BinlogAnchor`, `extract_stmt` |
| `src/source/mysql/proc.rs` | `MySqlConnConfig`, `spawn_mysqldump`, `drain_stderr_tail` |
| `tests/mysql_flags.rs` | T6~T11(순수 함수, 표 기반) |
| `tests/mysql_anchor.rs` | T12~T13(mock AsyncRead) |
| `tests/mysql_engine_scan.rs` | T1~T3, T17(컨테이너 대상 통합) |
| `tests/mysql_source_e2e.rs` | T14~T16, T18~T19(실제 mysqldump, MySQL/MariaDB 컨테이너 매트릭스) |

`#tags: mysqlsource, backupsource, mysqldump, mariadb, engine-scan, myisam, binlog-anchor, gtid, column-statistics, rust, section-s04`
