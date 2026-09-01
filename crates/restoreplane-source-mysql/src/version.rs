// crates/restoreplane-source-mysql/src/version.rs
//! S04 §2 — 서버 종류/버전 감지 + 클라이언트≥서버 게이트. 순수 로직이라 DB 없이 유닛 테스트한다
//! (R5/R7, T4/T5). source-pg의 parse_version_major/probe_client_version과 평행하다.

use crate::MySqlSourceError;
#[cfg(windows)]
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerKind {
    OracleMySql,
    MariaDb,
}

#[derive(Debug, Clone)]
pub struct ServerVersionInfo {
    /// SELECT VERSION() 원문(매니페스트 server_version에 그대로 저장).
    pub raw: String,
    pub kind: ServerKind,
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ServerVersionInfo {
    /// VERSION() 원문에서 종류/버전을 추출한다(R5). 순수 함수 — DB 불필요.
    pub fn from_raw(raw: &str) -> Result<Self, MySqlSourceError> {
        let kind = detect_server_kind(raw);
        let (major, minor, patch) = parse_semver_prefix(raw)
            .ok_or_else(|| MySqlSourceError::VersionParseFailed(raw.to_string()))?;
        Ok(ServerVersionInfo {
            raw: raw.to_string(),
            kind,
            major,
            minor,
            patch,
        })
    }
}

/// VERSION() 문자열에 'MariaDB'가 포함되면 MariaDB, 아니면 Oracle MySQL(R5).
pub fn detect_server_kind(raw: &str) -> ServerKind {
    if raw.to_ascii_uppercase().contains("MARIADB") {
        ServerKind::MariaDb
    } else {
        ServerKind::OracleMySql
    }
}

/// "8.0.37-0ubuntu0.22.04.1" / "10.11.6-MariaDB-1:10.11.6+maria~ubu2204" 앞부분 major.minor.patch만 추출.
/// semver 크레이트/regex를 쓰지 않는 이유: 두 배포판 모두 접미사가 엄격 semver 문법을 어긴다 —
/// 접두 숫자 3개만 손으로 긁는 편이 더 견고하다(source-pg parse_version_major와 동일 취지).
pub fn parse_semver_prefix(raw: &str) -> Option<(u32, u32, u32)> {
    let mut it = raw.split('.');
    let major = leading_number(it.next()?)?;
    let minor = leading_number(it.next()?)?;
    let patch = leading_number(it.next()?)?;
    Some((major, minor, patch))
}

/// mysqldump 배너("mysqldump ... Distrib 8.0.37, ...")에서 클라이언트 메이저를 추출한다.
/// Oracle: "Distrib 8.0.37" / MariaDB: "Distrib 10.11.6-MariaDB". 'Distrib' 다음 토큰의 접두만 취함.
pub fn parse_client_major_from_banner(raw: &str) -> Option<u32> {
    let after = raw.split("Distrib").nth(1)?;
    let token = after
        .split(|c: char| c == ',' || c.is_whitespace())
        .find(|s| !s.is_empty())?;
    let (major, _, _) = parse_semver_prefix(token)?;
    Some(major)
}

/// 선택된 mysqldump 바이너리를 실제 실행해 클라이언트 버전을 재확인한다(공급망 치환 방어, R7).
/// source-pg probe_client_version과 평행. 반환: (client_major, banner_raw).
pub async fn probe_client_version(
    binary: &std::path::Path,
) -> Result<(u32, String), MySqlSourceError> {
    probe_client_version_with_identity(binary, None, &[]).await
}

pub async fn probe_client_version_with_identity(
    binary: &std::path::Path,
    expected_sha256: Option<[u8; 32]>,
    trusted_dependencies: &[(std::path::PathBuf, [u8; 32])],
) -> Result<(u32, String), MySqlSourceError> {
    #[cfg(windows)]
    let raw = {
        use restoreplane_runtime::{hash_executable, LaunchSpec, ProcessSupervisor};

        let path = binary.to_path_buf();
        let dependencies = trusted_dependencies.to_vec();
        let expected = match expected_sha256 {
            Some(expected) => expected,
            None => hash_executable(&path)
                .map_err(|_| MySqlSourceError::DumpToolUntrusted { path: path.clone() })?,
        };
        tokio::task::spawn_blocking(move || {
            let working_directory = path
                .parent()
                .ok_or_else(|| MySqlSourceError::DumpToolUntrusted { path: path.clone() })?;
            let spec = LaunchSpec::new(path.clone(), working_directory.to_path_buf(), expected)
                .and_then(|spec| spec.with_args(["--version"]))
                .and_then(|spec| spec.with_environment(std::env::vars_os()))
                .and_then(|spec| spec.with_trusted_dependencies(dependencies))
                .map_err(|_| MySqlSourceError::DumpToolUntrusted { path: path.clone() })?;
            let mut process = ProcessSupervisor::launch(&spec)
                .map_err(|_| MySqlSourceError::DumpToolUntrusted { path: path.clone() })?;
            let Some(exit) = process
                .wait_timeout(Duration::from_secs(30))
                .map_err(|_| MySqlSourceError::DumpToolUntrusted { path: path.clone() })?
            else {
                let _ = process.terminate();
                return Err(MySqlSourceError::DumpToolUntrusted { path });
            };
            if exit.exit_code() != 0 || exit.stdout().truncated() {
                return Err(MySqlSourceError::DumpToolUntrusted { path });
            }
            Ok(String::from_utf8_lossy(exit.stdout().tail())
                .trim()
                .to_owned())
        })
        .await
        .map_err(|_| MySqlSourceError::DumpToolUntrusted {
            path: binary.to_path_buf(),
        })??
    };

    #[cfg(not(windows))]
    let raw = {
        if expected_sha256.is_some() {
            return Err(MySqlSourceError::DumpToolUntrusted {
                path: binary.to_path_buf(),
            });
        }
        if !trusted_dependencies.is_empty() {
            return Err(MySqlSourceError::DumpToolUntrusted {
                path: binary.to_path_buf(),
            });
        }
        let out = tokio::process::Command::new(binary)
            .arg("--version")
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| MySqlSourceError::DumpToolSpawnFailed {
                path: binary.to_path_buf(),
                source: e,
            })?;
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let major = parse_client_major_from_banner(&raw)
        .ok_or_else(|| MySqlSourceError::VersionParseFailed(raw.clone()))?;
    Ok((major, raw))
}

/// 클라이언트 메이저 < 서버 메이저면 하드 실패(R6/R7, source-pg ClientTooOld와 동일 원칙).
pub fn enforce_client_not_older(
    client_major: u32,
    server_major: u32,
) -> Result<(), MySqlSourceError> {
    if client_major < server_major {
        return Err(MySqlSourceError::ClientTooOld {
            client_major,
            server_major,
        });
    }
    Ok(())
}

/// 문자열 앞부분의 연속된 숫자만 파싱한다("37-0ubuntu" → 37, "6+maria" → 6).
fn leading_number(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t4_detect_mariadb() {
        let v = ServerVersionInfo::from_raw("10.11.6-MariaDB-1:10.11.6+maria~ubu2204").unwrap();
        assert_eq!(v.kind, ServerKind::MariaDb);
        assert_eq!(v.major, 10);
        assert_eq!(v.minor, 11);
        assert_eq!(v.patch, 6);
    }

    #[test]
    fn t5_detect_oracle_mysql() {
        let v = ServerVersionInfo::from_raw("8.0.37-0ubuntu0.22.04.1").unwrap();
        assert_eq!(v.kind, ServerKind::OracleMySql);
        assert_eq!(v.major, 8);
        assert_eq!(v.minor, 0);
        assert_eq!(v.patch, 37);
    }

    #[test]
    fn mariadb_detection_is_case_insensitive() {
        assert_eq!(
            detect_server_kind("5.5.5-10.6.16-mariadb"),
            ServerKind::MariaDb
        );
        assert_eq!(detect_server_kind("5.7.44-log"), ServerKind::OracleMySql);
    }

    #[test]
    fn parse_prefix_rejects_garbage() {
        assert!(parse_semver_prefix("MariaDB").is_none());
        assert!(parse_semver_prefix("8.0").is_none()); // patch 없음
        assert_eq!(parse_semver_prefix("5.7.44-log"), Some((5, 7, 44)));
    }

    #[test]
    fn client_banner_major() {
        assert_eq!(
            parse_client_major_from_banner("mysqldump  Ver 10.13 Distrib 8.0.37, for Linux"),
            Some(8)
        );
        assert_eq!(
            parse_client_major_from_banner("mysqldump  Ver 10.19 Distrib 10.11.6-MariaDB, for"),
            Some(10)
        );
        assert_eq!(parse_client_major_from_banner("no distrib token"), None);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn trusted_version_probe_rejects_digest_mismatch_before_resume() {
        let executable = std::env::current_exe().expect("test executable");
        let error = probe_client_version_with_identity(&executable, Some([0; 32]), &[])
            .await
            .expect_err("mismatched attestation must block process launch");
        assert!(matches!(error, MySqlSourceError::DumpToolUntrusted { .. }));
    }

    #[test]
    fn version_matrix_client_older_hard_fails() {
        // 클라이언트 5 < 서버 8 → 하드 실패.
        assert!(enforce_client_not_older(5, 8).is_err());
        // 클라이언트 8 >= 서버 8 → OK.
        assert!(enforce_client_not_older(8, 8).is_ok());
        // 클라이언트 10 >= 서버 8 → OK.
        assert!(enforce_client_not_older(10, 8).is_ok());
    }
}
