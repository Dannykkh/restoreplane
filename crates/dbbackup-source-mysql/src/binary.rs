// crates/dbbackup-source-mysql/src/binary.rs
//! S04 §3 — mysqldump 바이너리 매트릭스. **MariaDB축 / Oracle MySQL축을 별도 축으로 분리**한다.
//! 하나의 "MySQL 매트릭스"에 두 배포판을 욱여넣으면 --column-statistics 같은 배포판 전용 플래그를
//! 잘못된 바이너리에 넘겨 unknown option으로 죽는다(§2.5). source-pg BundledToolMatrix와 평행하되
//! 배포판 축이 하나 더 있다는 점이 다르다. 미매칭이면 "가장 가까운 걸로 시도" 금지, 하드 실패(R6).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::version::{ServerKind, ServerVersionInfo};
use crate::MySqlSourceError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientKind {
    OracleMySql,
    MariaDb,
}

/// S01 Config에서 로드되는 번들 바이너리 경로표. 배포판축 × (메이저 → 실행 파일 경로).
pub struct MysqldumpBinaryMatrix {
    oracle_mysql: BTreeMap<u32, PathBuf>,
    mariadb: BTreeMap<u32, PathBuf>,
}

impl MysqldumpBinaryMatrix {
    /// 테스트/명시 구성용: 두 축의 메이저→경로 맵을 직접 지정.
    pub fn from_axes(
        oracle_mysql: BTreeMap<u32, PathBuf>,
        mariadb: BTreeMap<u32, PathBuf>,
    ) -> Self {
        Self { oracle_mysql, mariadb }
    }

    /// 디렉토리 관례: `tools/mysql/{mysql|mariadb}{major}/mysqldump[.exe]`.
    /// v1 지원 범위: MySQL 5~8, MariaDB 10~11. 디렉토리가 없는 메이저는 건너뛴다.
    pub fn from_base_dir(base: &Path) -> Self {
        let mut oracle_mysql = BTreeMap::new();
        for major in 5..=8u32 {
            let dir = base.join(format!("mysql{major}"));
            if dir.is_dir() {
                oracle_mysql.insert(major, dir.join(bin_name()));
            }
        }
        let mut mariadb = BTreeMap::new();
        for major in 10..=11u32 {
            let dir = base.join(format!("mariadb{major}"));
            if dir.is_dir() {
                mariadb.insert(major, dir.join(bin_name()));
            }
        }
        Self { oracle_mysql, mariadb }
    }

    /// 서버 종류에 대응하는 **같은 배포판 축**에서, 서버 메이저 이상인 최소 클라이언트를 고른다.
    /// 없으면 하드 실패(클라이언트가 서버보다 오래된 채로 강행하지 않음 — R6, source-pg §1.6 원칙).
    /// server_kind == client_kind를 강제하므로, MariaDB 서버에 Oracle mysqldump가 붙는 오분기가 원천 차단된다.
    pub fn resolve(
        &self,
        server: &ServerVersionInfo,
    ) -> Result<(ClientKind, PathBuf), MySqlSourceError> {
        let (table, kind) = match server.kind {
            ServerKind::OracleMySql => (&self.oracle_mysql, ClientKind::OracleMySql),
            ServerKind::MariaDb => (&self.mariadb, ClientKind::MariaDb),
        };
        let (_, path) = table
            .range(server.major..)
            .next()
            .ok_or(MySqlSourceError::NoBundledClientForServer {
                server_kind: server.kind,
                server_major: server.major,
            })?;
        Ok((kind, path.clone()))
    }
}

#[cfg(windows)]
pub fn bin_name() -> &'static str {
    "mysqldump.exe"
}
#[cfg(not(windows))]
pub fn bin_name() -> &'static str {
    "mysqldump"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::ServerVersionInfo;

    fn server(kind: ServerKind, major: u32) -> ServerVersionInfo {
        ServerVersionInfo { raw: format!("{major}.0.0"), kind, major, minor: 0, patch: 0 }
    }

    fn matrix() -> MysqldumpBinaryMatrix {
        let mut oracle = BTreeMap::new();
        oracle.insert(5, PathBuf::from("tools/mysql/mysql5/mysqldump"));
        oracle.insert(8, PathBuf::from("tools/mysql/mysql8/mysqldump"));
        let mut maria = BTreeMap::new();
        maria.insert(10, PathBuf::from("tools/mysql/mariadb10/mysqldump"));
        maria.insert(11, PathBuf::from("tools/mysql/mariadb11/mysqldump"));
        MysqldumpBinaryMatrix::from_axes(oracle, maria)
    }

    #[test]
    fn resolve_oracle_axis_exact() {
        let (kind, path) = matrix().resolve(&server(ServerKind::OracleMySql, 8)).unwrap();
        assert_eq!(kind, ClientKind::OracleMySql);
        assert!(path.to_string_lossy().contains("mysql8"));
    }

    #[test]
    fn resolve_mariadb_axis_never_uses_oracle_binary() {
        // MariaDB 서버는 MariaDB 축에서만 선택 — Oracle 바이너리로 새지 않는다(§2.5 오분기 차단).
        let (kind, path) = matrix().resolve(&server(ServerKind::MariaDb, 10)).unwrap();
        assert_eq!(kind, ClientKind::MariaDb);
        assert!(path.to_string_lossy().contains("mariadb10"));
    }

    #[test]
    fn resolve_lowest_ge_server_on_axis() {
        // Oracle 서버 major=6, 축엔 5와 8만 존재 → 6 이상 최소인 8 선택.
        let (_, path) = matrix().resolve(&server(ServerKind::OracleMySql, 6)).unwrap();
        assert!(path.to_string_lossy().contains("mysql8"));
    }

    #[test]
    fn t9_no_client_covering_server_hard_fails() {
        // Oracle 서버 major=9, 축 최대는 8 → 하드 실패(가장 가까운 걸로 강행 금지, R6).
        let err = matrix().resolve(&server(ServerKind::OracleMySql, 9)).unwrap_err();
        assert!(matches!(
            err,
            MySqlSourceError::NoBundledClientForServer {
                server_kind: ServerKind::OracleMySql,
                server_major: 9
            }
        ));
    }

    #[test]
    fn mariadb_server_with_empty_mariadb_axis_hard_fails() {
        // MariaDB 축이 비어 있으면 Oracle 축으로 폴백하지 않고 하드 실패.
        let oracle = BTreeMap::from([(8u32, PathBuf::from("tools/mysql/mysql8/mysqldump"))]);
        let m = MysqldumpBinaryMatrix::from_axes(oracle, BTreeMap::new());
        assert!(m.resolve(&server(ServerKind::MariaDb, 10)).is_err());
    }
}
