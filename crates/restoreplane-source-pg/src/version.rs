// crates/restoreplane-source-pg/src/version.rs
//! S03 §3~4 — 버전 재검증 + 번들 매트릭스(PG14~18). 순수 로직이라 DB 없이 유닛 테스트한다(R4/R5/R8).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::time::Duration;

use crate::PgSourceError;

#[derive(Clone, Debug)]
pub struct ToolVersion {
    pub major: u32,
    pub raw: String,
}

/// "pg_dump (PostgreSQL) 16.3" → 16. 마지막 토큰의 첫 '.' 앞부분만 취함.
pub fn parse_version_major(raw: &str) -> Option<u32> {
    raw.split_whitespace()
        .last()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// 선택된 pg_dump 바이너리를 실제 실행해 버전을 재확인한다(공급망 치환 방어, R4).
pub async fn probe_client_version(pg_dump_path: &Path) -> Result<ToolVersion, PgSourceError> {
    probe_client_version_with_identity(pg_dump_path, None, &[]).await
}

pub async fn probe_client_version_with_identity(
    pg_dump_path: &Path,
    expected_sha256: Option<[u8; 32]>,
    trusted_dependencies: &[(PathBuf, [u8; 32])],
) -> Result<ToolVersion, PgSourceError> {
    #[cfg(windows)]
    let raw = {
        use restoreplane_runtime::{hash_executable, LaunchSpec, ProcessSupervisor};

        let path = pg_dump_path.to_path_buf();
        let dependencies = trusted_dependencies.to_vec();
        let expected = match expected_sha256 {
            Some(expected) => expected,
            None => hash_executable(&path)
                .map_err(|_| PgSourceError::DumpToolUntrusted { path: path.clone() })?,
        };
        tokio::task::spawn_blocking(move || {
            let working_directory = path
                .parent()
                .ok_or_else(|| PgSourceError::DumpToolUntrusted { path: path.clone() })?;
            let spec = LaunchSpec::new(path.clone(), working_directory.to_path_buf(), expected)
                .and_then(|spec| spec.with_args(["--version"]))
                .and_then(|spec| spec.with_environment(std::env::vars_os()))
                .and_then(|spec| spec.with_trusted_dependencies(dependencies))
                .map_err(|_| PgSourceError::DumpToolUntrusted { path: path.clone() })?;
            let mut process = ProcessSupervisor::launch(&spec)
                .map_err(|_| PgSourceError::DumpToolUntrusted { path: path.clone() })?;
            let Some(exit) = process
                .wait_timeout(Duration::from_secs(30))
                .map_err(|_| PgSourceError::DumpToolUntrusted { path: path.clone() })?
            else {
                let _ = process.terminate();
                return Err(PgSourceError::DumpToolUntrusted { path });
            };
            if exit.exit_code() != 0 || exit.stdout().truncated() {
                return Err(PgSourceError::DumpToolUntrusted { path });
            }
            Ok(String::from_utf8_lossy(exit.stdout().tail())
                .trim()
                .to_owned())
        })
        .await
        .map_err(|_| PgSourceError::DumpToolUntrusted {
            path: pg_dump_path.to_path_buf(),
        })??
    };

    #[cfg(not(windows))]
    let raw = {
        if expected_sha256.is_some() {
            return Err(PgSourceError::DumpToolUntrusted {
                path: pg_dump_path.to_path_buf(),
            });
        }
        if !trusted_dependencies.is_empty() {
            return Err(PgSourceError::DumpToolUntrusted {
                path: pg_dump_path.to_path_buf(),
            });
        }
        let out = tokio::process::Command::new(pg_dump_path)
            .arg("--version")
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| PgSourceError::DumpToolSpawnFailed {
                path: pg_dump_path.to_path_buf(),
                source: e,
            })?;
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let major =
        parse_version_major(&raw).ok_or_else(|| PgSourceError::VersionParseFailed(raw.clone()))?;
    Ok(ToolVersion { major, raw })
}

/// 서버 메이저 버전 → 번들된 클라이언트 디렉토리 매핑.
/// 디렉토리 관례: `tools/pg/pg{major}/pg_dump[.exe]`.
pub struct BundledToolMatrix {
    roots: BTreeMap<u32, PathBuf>,
    require_existing_binary: bool,
    trusted_sha256: BTreeMap<PathBuf, [u8; 32]>,
    trusted_dependencies: BTreeMap<PathBuf, Vec<(PathBuf, [u8; 32])>>,
    require_trusted_identity: bool,
}

impl BundledToolMatrix {
    /// 현재 upstream 지원 범위: PG14~18. 디렉토리가 없는 메이저는 건너뛴다.
    pub fn from_base_dir(base: &Path) -> Self {
        let mut roots = BTreeMap::new();
        for major in bundled_supported_majors() {
            let dir = base.join(format!("pg{major}"));
            if dir.is_dir() {
                roots.insert(major, dir);
            }
        }
        if roots.is_empty() {
            for major in bundled_supported_majors() {
                roots.insert(major, PathBuf::new());
            }
        }
        Self {
            roots,
            require_existing_binary: false,
            trusted_sha256: BTreeMap::new(),
            trusted_dependencies: BTreeMap::new(),
            require_trusted_identity: false,
        }
    }

    /// Builds the production service matrix from installed bundle files only.
    ///
    /// Unlike [`Self::from_base_dir`], an empty bundle stays empty and selection never resolves to
    /// a bare command name from the ambient process `PATH`.
    pub fn from_bundle_dir(base: &Path) -> Self {
        let roots = bundled_supported_majors()
            .filter_map(|major| {
                let directory = base.join(format!("pg{major}"));
                directory.is_dir().then_some((major, directory))
            })
            .collect();
        Self {
            roots,
            require_existing_binary: true,
            trusted_sha256: BTreeMap::new(),
            trusted_dependencies: BTreeMap::new(),
            require_trusted_identity: false,
        }
    }

    /// 테스트/명시 구성용: 메이저→디렉토리 맵을 직접 지정.
    pub fn from_roots(roots: BTreeMap<u32, PathBuf>) -> Self {
        Self {
            roots,
            require_existing_binary: false,
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

    pub fn expected_sha256(&self, path: &Path) -> Result<Option<[u8; 32]>, PgSourceError> {
        let identity = self.trusted_sha256.get(&canonical_key(path)).copied();
        if self.require_trusted_identity && identity.is_none() {
            return Err(PgSourceError::DumpToolUntrusted {
                path: path.to_path_buf(),
            });
        }
        Ok(identity)
    }

    pub fn trusted_dependencies(
        &self,
        path: &Path,
    ) -> Result<Vec<(PathBuf, [u8; 32])>, PgSourceError> {
        if !self.require_trusted_identity {
            return Ok(Vec::new());
        }
        self.trusted_dependencies
            .get(&canonical_key(path))
            .cloned()
            .ok_or_else(|| PgSourceError::DumpToolUntrusted {
                path: path.to_path_buf(),
            })
    }

    pub fn select_pg_dump(&self, server_major: u32) -> Result<PathBuf, PgSourceError> {
        self.select_binary(server_major, bin_name("pg_dump"))
    }

    pub fn select_pg_dumpall(&self, server_major: u32) -> Result<PathBuf, PgSourceError> {
        self.select_binary(server_major, bin_name("pg_dumpall"))
    }

    /// 정확 일치가 있으면 그것. 없으면 server_major 이상 중 가장 낮은 메이저. 아니면 하드 실패.
    fn select_binary(&self, server_major: u32, filename: &str) -> Result<PathBuf, PgSourceError> {
        self.roots
            .range(server_major..)
            .map(|(_, directory)| directory.join(filename))
            .find(|candidate| {
                (!self.require_existing_binary || candidate.is_file())
                    && (!self.require_trusted_identity
                        || self.trusted_sha256.contains_key(&canonical_key(candidate)))
            })
            .ok_or(PgSourceError::NoBundledClientForServer { server_major })
    }
}

fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub const fn bundled_supported_majors() -> std::ops::RangeInclusive<u32> {
    14..=18
}

#[cfg(windows)]
pub fn bin_name(stem: &str) -> &'static str {
    match stem {
        "pg_dump" => "pg_dump.exe",
        "pg_dumpall" => "pg_dumpall.exe",
        _ => unreachable!(),
    }
}
#[cfg(not(windows))]
pub fn bin_name(stem: &str) -> &'static str {
    match stem {
        "pg_dump" => "pg_dump",
        "pg_dumpall" => "pg_dumpall",
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_major_from_pg_dump_banner() {
        assert_eq!(parse_version_major("pg_dump (PostgreSQL) 16.3"), Some(16));
        assert_eq!(parse_version_major("pg_dump (PostgreSQL) 17rc1"), None); // "17rc1" 파싱 실패
        assert_eq!(parse_version_major("PostgreSQL 15.6 on x86_64"), None); // 마지막 토큰이 아키텍처
        assert_eq!(parse_version_major(""), None);
    }

    fn roots(majors: &[u32]) -> BundledToolMatrix {
        let mut m = BTreeMap::new();
        for &v in majors {
            m.insert(v, PathBuf::from(format!("tools/pg/pg{v}")));
        }
        BundledToolMatrix::from_roots(m)
    }

    #[test]
    fn t5_exact_match() {
        let m = roots(&[13, 14, 15, 16, 17]);
        let p = m.select_pg_dump(16).unwrap();
        assert!(p.ends_with(bin_name("pg_dump")));
        assert!(p.to_string_lossy().contains("pg16"));
    }

    #[test]
    fn production_matrix_skips_unattested_lower_axes() {
        let m = roots(&[16, 18]);
        let trusted = PathBuf::from("tools/pg/pg18").join(bin_name("pg_dump"));
        let m = m.with_trusted_identities([(trusted.clone(), [7; 32])]);
        assert_eq!(
            m.select_pg_dump(16).expect("trusted PG18 fallback"),
            trusted
        );
        assert_eq!(m.expected_sha256(&trusted).unwrap(), Some([7; 32]));
        assert!(m.select_pg_dumpall(16).is_err());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn trusted_version_probe_rejects_digest_mismatch_before_resume() {
        let executable = std::env::current_exe().expect("test executable");
        let error = probe_client_version_with_identity(&executable, Some([0; 32]), &[])
            .await
            .expect_err("mismatched attestation must block process launch");
        assert!(matches!(error, PgSourceError::DumpToolUntrusted { .. }));
    }

    #[test]
    fn t6_fallback_to_lowest_ge_server() {
        let m = roots(&[16, 17]); // pg15 없음
        let p = m.select_pg_dump(15).unwrap();
        assert!(p.to_string_lossy().contains("pg16"));
    }

    #[test]
    fn t7_unsupported_major_hard_fail() {
        let m = roots(&[14, 15, 16, 17, 18]);
        let err = m.select_pg_dump(19).unwrap_err();
        assert!(matches!(
            err,
            PgSourceError::NoBundledClientForServer { server_major: 19 }
        ));
    }

    #[test]
    fn bundled_matrix_tracks_current_upstream_supported_majors() {
        assert_eq!(bundled_supported_majors(), 14..=18);
    }

    #[test]
    fn empty_bundle_uses_path_clients_without_expanding_supported_majors() {
        let matrix = BundledToolMatrix::from_base_dir(Path::new("missing-tool-root"));
        assert_eq!(
            matrix.select_pg_dump(16).unwrap(),
            PathBuf::from(bin_name("pg_dump"))
        );
        assert!(matrix.select_pg_dump(19).is_err());
    }

    #[test]
    fn strict_bundle_matrix_never_returns_an_ambient_path_client() {
        let matrix = BundledToolMatrix::from_bundle_dir(Path::new("missing-tool-root"));
        assert!(matches!(
            matrix.select_pg_dump(16),
            Err(PgSourceError::NoBundledClientForServer { server_major: 16 })
        ));
        assert!(matches!(
            matrix.select_pg_dumpall(16),
            Err(PgSourceError::NoBundledClientForServer { server_major: 16 })
        ));
    }
}
