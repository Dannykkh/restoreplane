// crates/restoreplane-source-mysql/src/binary.rs
//! S04 §3 — mysqldump 바이너리 매트릭스. **MariaDB축 / Oracle MySQL축을 별도 축으로 분리**한다.
//! 하나의 "MySQL 매트릭스"에 두 배포판을 욱여넣으면 --column-statistics 같은 배포판 전용 플래그를
//! 잘못된 바이너리에 넘겨 unknown option으로 죽는다(§2.5). source-pg BundledToolMatrix와 평행하되
//! 배포판 축이 하나 더 있다는 점이 다르다. 미매칭이면 "가장 가까운 걸로 시도" 금지, 하드 실패(R6).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io;
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
    trusted_sha256: BTreeMap<PathBuf, [u8; 32]>,
    trusted_dependencies: BTreeMap<PathBuf, Vec<(PathBuf, [u8; 32])>>,
    require_trusted_identity: bool,
}

impl MysqldumpBinaryMatrix {
    /// 테스트/명시 구성용: 두 축의 메이저→경로 맵을 직접 지정.
    pub fn from_axes(
        oracle_mysql: BTreeMap<u32, PathBuf>,
        mariadb: BTreeMap<u32, PathBuf>,
    ) -> Self {
        Self {
            oracle_mysql,
            mariadb,
            trusted_sha256: BTreeMap::new(),
            trusted_dependencies: BTreeMap::new(),
            require_trusted_identity: false,
        }
    }

    /// 디렉토리 관례: `tools/mysql/{mysql|mariadb}{major}/mysqldump[.exe]`.
    /// v1 지원 범위: MySQL 5~8, MariaDB 10~11. 디렉토리가 없는 메이저는 건너뛴다.
    pub fn from_base_dir(base: &Path) -> Self {
        let mut oracle_mysql = BTreeMap::new();
        for major in 5..=8u32 {
            let dir = base.join(format!("mysql{major}"));
            if dir.is_dir() {
                oracle_mysql.insert(major, dir.join(bin_name_for(ClientKind::OracleMySql)));
            }
        }
        if oracle_mysql.is_empty() {
            for major in 5..=8u32 {
                oracle_mysql.insert(major, PathBuf::from(bin_name_for(ClientKind::OracleMySql)));
            }
        }
        let mut mariadb = BTreeMap::new();
        for major in 10..=11u32 {
            let dir = base.join(format!("mariadb{major}"));
            if dir.is_dir() {
                mariadb.insert(major, dir.join(bin_name_for(ClientKind::MariaDb)));
            }
        }
        if mariadb.is_empty() {
            for major in 10..=11u32 {
                mariadb.insert(major, PathBuf::from(bin_name_for(ClientKind::MariaDb)));
            }
        }
        Self {
            oracle_mysql,
            mariadb,
            trusted_sha256: BTreeMap::new(),
            trusted_dependencies: BTreeMap::new(),
            require_trusted_identity: false,
        }
    }

    /// Builds the production service matrix from installed vendor bundle files only.
    ///
    /// Missing axes remain empty, so resolution fails instead of executing a bare command from
    /// the ambient process `PATH`.
    pub fn from_bundle_dir(base: &Path) -> Self {
        let oracle_mysql = (5..=8_u32)
            .filter_map(|major| {
                let candidate = base
                    .join(format!("mysql{major}"))
                    .join(bin_name_for(ClientKind::OracleMySql));
                candidate.is_file().then_some((major, candidate))
            })
            .collect();
        let mariadb = (10..=11_u32)
            .filter_map(|major| {
                let candidate = base
                    .join(format!("mariadb{major}"))
                    .join(bin_name_for(ClientKind::MariaDb));
                candidate.is_file().then_some((major, candidate))
            })
            .collect();
        Self {
            oracle_mysql,
            mariadb,
            trusted_sha256: BTreeMap::new(),
            trusted_dependencies: BTreeMap::new(),
            require_trusted_identity: false,
        }
    }

    pub fn with_trusted_identities<I>(mut self, identities: I) -> Self
    where
        I: IntoIterator<Item = (PathBuf, [u8; 32])>,
    {
        let identities = identities.into_iter().collect::<Vec<_>>();
        self.trusted_sha256 = identities
            .iter()
            .map(|(path, sha256)| (canonical_key(path), *sha256))
            .collect();
        self.trusted_dependencies = identities
            .into_iter()
            .map(|(path, _)| (canonical_key(&path), Vec::new()))
            .collect();
        self.require_trusted_identity = true;
        self
    }

    pub fn with_trusted_launch_identities<I>(mut self, identities: I) -> Self
    where
        I: IntoIterator<Item = (PathBuf, [u8; 32], Vec<(PathBuf, [u8; 32])>)>,
    {
        let identities = identities.into_iter().collect::<Vec<_>>();
        self.trusted_sha256 = identities
            .iter()
            .map(|(path, sha256, _)| (canonical_key(path), *sha256))
            .collect();
        self.trusted_dependencies = identities
            .into_iter()
            .map(|(path, _, dependencies)| (canonical_key(&path), dependencies))
            .collect();
        self.require_trusted_identity = true;
        self
    }

    pub fn expected_sha256(&self, path: &Path) -> Result<Option<[u8; 32]>, MySqlSourceError> {
        let identity = self.trusted_sha256.get(&canonical_key(path)).copied();
        if self.require_trusted_identity && identity.is_none() {
            return Err(MySqlSourceError::DumpToolUntrusted {
                path: path.to_path_buf(),
            });
        }
        Ok(identity)
    }

    pub fn trusted_dependencies(
        &self,
        path: &Path,
    ) -> Result<Vec<(PathBuf, [u8; 32])>, MySqlSourceError> {
        if !self.require_trusted_identity {
            return Ok(Vec::new());
        }
        self.trusted_dependencies
            .get(&canonical_key(path))
            .cloned()
            .ok_or_else(|| MySqlSourceError::DumpToolUntrusted {
                path: path.to_path_buf(),
            })
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
            .find(|(_, path)| {
                !self.require_trusted_identity
                    || self.trusted_sha256.contains_key(&canonical_key(path))
            })
            .ok_or(MySqlSourceError::NoBundledClientForServer {
                server_kind: server.kind,
                server_major: server.major,
            })?;
        Ok((kind, path.clone()))
    }
}

fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub const fn bin_name_for(kind: ClientKind) -> &'static str {
    match (kind, cfg!(windows)) {
        (ClientKind::OracleMySql, true) => "mysqldump.exe",
        (ClientKind::OracleMySql, false) => "mysqldump",
        (ClientKind::MariaDb, true) => "mariadb-dump.exe",
        (ClientKind::MariaDb, false) => "mariadb-dump",
    }
}

pub const fn tool_name_for(kind: ClientKind) -> &'static str {
    match kind {
        ClientKind::OracleMySql => "mysqldump",
        ClientKind::MariaDb => "mariadb-dump",
    }
}

/// Resolves the lightweight SQL client used for metadata probes.
///
/// A configured path is an explicit override and therefore must be absolute and point to an
/// existing file. Without an override, the resolver selects the newest installed vendor bundle
/// under `tools/mysql`, then falls back to an existing file in an absolute `PATH` directory. It
/// never returns an unresolved command name.
pub fn resolve_metadata_client(
    kind: ClientKind,
    configured_path: Option<&Path>,
    bundle_base: Option<&Path>,
    search_path: Option<&OsStr>,
) -> io::Result<PathBuf> {
    if let Some(path) = configured_path {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "configured MySQL metadata client path must be absolute",
            ));
        }
        return canonical_existing_file(path);
    }

    let (vendor_prefix, first_major, last_major) = match kind {
        ClientKind::OracleMySql => ("mysql", 5_u32, 8_u32),
        ClientKind::MariaDb => ("mariadb", 10_u32, 11_u32),
    };
    if let Some(bundle_base) = bundle_base {
        for major in (first_major..=last_major).rev() {
            let candidate = bundle_base
                .join(format!("{vendor_prefix}{major}"))
                .join(metadata_bin_name_for(kind));
            if candidate.is_file() {
                return canonical_existing_file(&candidate);
            }
        }
    }

    if let Some(search_path) = search_path {
        for directory in std::env::split_paths(search_path).filter(|path| path.is_absolute()) {
            let candidate = directory.join(metadata_bin_name_for(kind));
            if candidate.is_file() {
                return canonical_existing_file(&candidate);
            }
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{} metadata client was not found in the configured bundle or PATH",
            metadata_bin_name_for(kind)
        ),
    ))
}

/// Resolves a production metadata client from the installed vendor bundle only.
///
/// The service deliberately has no `PATH` parameter: a missing bundle is a terminal condition,
/// not permission to execute an ambient binary with database credentials.
pub fn resolve_bundled_metadata_client(
    kind: ClientKind,
    bundle_base: &Path,
) -> io::Result<PathBuf> {
    resolve_metadata_client(kind, None, Some(bundle_base), None)
}

fn canonical_existing_file(path: &Path) -> io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    if canonical.is_file() {
        Ok(canonical)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "MySQL metadata client path is not a file",
        ))
    }
}

pub const fn metadata_bin_name_for(kind: ClientKind) -> &'static str {
    match (kind, cfg!(windows)) {
        (ClientKind::OracleMySql, true) => "mysql.exe",
        (ClientKind::OracleMySql, false) => "mysql",
        (ClientKind::MariaDb, true) => "mariadb.exe",
        (ClientKind::MariaDb, false) => "mariadb",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::ServerVersionInfo;

    fn server(kind: ServerKind, major: u32) -> ServerVersionInfo {
        ServerVersionInfo {
            raw: format!("{major}.0.0"),
            kind,
            major,
            minor: 0,
            patch: 0,
        }
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
        let (kind, path) = matrix()
            .resolve(&server(ServerKind::OracleMySql, 8))
            .unwrap();
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
        let (_, path) = matrix()
            .resolve(&server(ServerKind::OracleMySql, 6))
            .unwrap();
        assert!(path.to_string_lossy().contains("mysql8"));
    }

    #[test]
    fn t9_no_client_covering_server_hard_fails() {
        // Oracle 서버 major=9, 축 최대는 8 → 하드 실패(가장 가까운 걸로 강행 금지, R6).
        let err = matrix()
            .resolve(&server(ServerKind::OracleMySql, 9))
            .unwrap_err();
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

    #[test]
    fn mariadb_axis_uses_vendor_current_mariadb_dump_name() {
        assert_eq!(
            bin_name_for(ClientKind::MariaDb),
            if cfg!(windows) {
                "mariadb-dump.exe"
            } else {
                "mariadb-dump"
            }
        );
        assert_eq!(
            bin_name_for(ClientKind::OracleMySql),
            if cfg!(windows) {
                "mysqldump.exe"
            } else {
                "mysqldump"
            }
        );
        assert_eq!(tool_name_for(ClientKind::MariaDb), "mariadb-dump");
        assert_eq!(tool_name_for(ClientKind::OracleMySql), "mysqldump");
    }

    #[test]
    fn empty_bundle_uses_vendor_specific_path_clients() {
        let matrix = MysqldumpBinaryMatrix::from_base_dir(Path::new("missing-tool-root"));
        let (mysql_kind, mysql_path) = matrix.resolve(&server(ServerKind::OracleMySql, 8)).unwrap();
        assert_eq!(mysql_kind, ClientKind::OracleMySql);
        assert_eq!(
            mysql_path,
            PathBuf::from(bin_name_for(ClientKind::OracleMySql))
        );
        let (maria_kind, maria_path) = matrix.resolve(&server(ServerKind::MariaDb, 11)).unwrap();
        assert_eq!(maria_kind, ClientKind::MariaDb);
        assert_eq!(maria_path, PathBuf::from(bin_name_for(ClientKind::MariaDb)));
        assert!(matrix.resolve(&server(ServerKind::OracleMySql, 9)).is_err());
    }

    #[test]
    fn metadata_client_prefers_the_install_bundle_for_each_vendor() {
        let temp = tempfile::TempDir::new().expect("tool fixture");
        let bundle = temp.path().join("tools").join("mysql");
        let path_dir = temp.path().join("path-decoy");
        std::fs::create_dir_all(&path_dir).expect("PATH fixture");
        for kind in [ClientKind::OracleMySql, ClientKind::MariaDb] {
            std::fs::write(path_dir.join(metadata_bin_name_for(kind)), b"path")
                .expect("PATH client");
        }
        let mysql = bundle
            .join("mysql8")
            .join(metadata_bin_name_for(ClientKind::OracleMySql));
        let maria = bundle
            .join("mariadb11")
            .join(metadata_bin_name_for(ClientKind::MariaDb));
        for client in [&mysql, &maria] {
            std::fs::create_dir_all(client.parent().expect("client parent"))
                .expect("bundle directory");
            std::fs::write(client, b"bundle").expect("bundle client");
        }
        let search_path = std::env::join_paths([&path_dir]).expect("search path");

        assert_eq!(
            resolve_metadata_client(
                ClientKind::OracleMySql,
                None,
                Some(&bundle),
                Some(search_path.as_os_str()),
            )
            .expect("bundled MySQL client"),
            std::fs::canonicalize(mysql).expect("canonical MySQL client")
        );
        assert_eq!(
            resolve_metadata_client(
                ClientKind::MariaDb,
                None,
                Some(&bundle),
                Some(search_path.as_os_str()),
            )
            .expect("bundled MariaDB client"),
            std::fs::canonicalize(maria).expect("canonical MariaDB client")
        );
    }

    #[test]
    fn explicit_metadata_client_is_validated_and_wins_over_bundle() {
        let temp = tempfile::TempDir::new().expect("tool fixture");
        let bundle = temp.path().join("tools").join("mysql");
        let bundled = bundle
            .join("mysql8")
            .join(metadata_bin_name_for(ClientKind::OracleMySql));
        let explicit = temp
            .path()
            .join("configured")
            .join(metadata_bin_name_for(ClientKind::OracleMySql));
        for client in [&bundled, &explicit] {
            std::fs::create_dir_all(client.parent().expect("client parent"))
                .expect("client directory");
            std::fs::write(client, b"client").expect("client fixture");
        }

        assert_eq!(
            resolve_metadata_client(
                ClientKind::OracleMySql,
                Some(&explicit),
                Some(&bundle),
                None,
            )
            .expect("explicit client"),
            std::fs::canonicalize(explicit).expect("canonical explicit client")
        );
        let missing = temp.path().join("missing").join("mysql.exe");
        assert!(resolve_metadata_client(
            ClientKind::OracleMySql,
            Some(&missing),
            Some(&bundle),
            None,
        )
        .is_err());
    }

    #[test]
    fn metadata_client_path_fallback_returns_only_an_existing_file() {
        let temp = tempfile::TempDir::new().expect("tool fixture");
        let path_dir = temp.path().join("path-client");
        std::fs::create_dir_all(&path_dir).expect("PATH fixture");
        let client = path_dir.join(metadata_bin_name_for(ClientKind::MariaDb));
        std::fs::write(&client, b"client").expect("PATH client");
        let search_path = std::env::join_paths([&path_dir]).expect("search path");

        assert_eq!(
            resolve_metadata_client(
                ClientKind::MariaDb,
                None,
                Some(&temp.path().join("missing-bundle")),
                Some(search_path.as_os_str()),
            )
            .expect("validated PATH client"),
            std::fs::canonicalize(client).expect("canonical PATH client")
        );
        assert!(resolve_metadata_client(
            ClientKind::OracleMySql,
            None,
            Some(&temp.path().join("missing-bundle")),
            None,
        )
        .is_err());
    }

    #[test]
    fn strict_dump_matrix_never_returns_an_ambient_path_client() {
        let matrix = MysqldumpBinaryMatrix::from_bundle_dir(Path::new("missing-tool-root"));
        assert!(matrix.resolve(&server(ServerKind::OracleMySql, 8)).is_err());
        assert!(matrix.resolve(&server(ServerKind::MariaDb, 11)).is_err());
    }

    #[test]
    fn production_matrix_selects_only_attested_family_axes() {
        let mysql5 =
            PathBuf::from("tools/mysql/mysql5").join(bin_name_for(ClientKind::OracleMySql));
        let mysql8 =
            PathBuf::from("tools/mysql/mysql8").join(bin_name_for(ClientKind::OracleMySql));
        let mut oracle = BTreeMap::new();
        oracle.insert(5, mysql5);
        oracle.insert(8, mysql8.clone());
        let matrix = MysqldumpBinaryMatrix::from_axes(oracle, BTreeMap::new())
            .with_trusted_identities([(mysql8.clone(), [8; 32])]);
        let (_, selected) = matrix
            .resolve(&server(ServerKind::OracleMySql, 5))
            .expect("trusted MySQL8 fallback");
        assert_eq!(selected, mysql8);
        assert_eq!(matrix.expected_sha256(&selected).unwrap(), Some([8; 32]));
        assert!(matrix.resolve(&server(ServerKind::MariaDb, 11)).is_err());
    }

    #[test]
    fn strict_metadata_resolver_ignores_an_available_path_client() {
        let temp = tempfile::TempDir::new().expect("tool fixture");
        let path_dir = temp.path().join("path-client");
        std::fs::create_dir_all(&path_dir).expect("PATH fixture");
        std::fs::write(
            path_dir.join(metadata_bin_name_for(ClientKind::OracleMySql)),
            b"ambient path client",
        )
        .expect("PATH client");
        let search_path = std::env::join_paths([&path_dir]).expect("search path");

        assert!(resolve_bundled_metadata_client(
            ClientKind::OracleMySql,
            &temp.path().join("missing-bundle"),
        )
        .is_err());
        assert!(search_path.to_string_lossy().contains("path-client"));
    }
}
