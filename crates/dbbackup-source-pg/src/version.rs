// crates/dbbackup-source-pg/src/version.rs
//! S03 §3~4 — 버전 재검증 + 번들 매트릭스(PG13~17). 순수 로직이라 DB 없이 유닛 테스트한다(R4/R5/R8).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
    let out = tokio::process::Command::new(pg_dump_path)
        .arg("--version")
        .output()
        .await
        .map_err(|e| PgSourceError::DumpToolSpawnFailed {
            path: pg_dump_path.to_path_buf(),
            source: e,
        })?;
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let major =
        parse_version_major(&raw).ok_or_else(|| PgSourceError::VersionParseFailed(raw.clone()))?;
    Ok(ToolVersion { major, raw })
}

/// 서버 메이저 버전 → 번들된 클라이언트 디렉토리 매핑.
/// 디렉토리 관례: `tools/pg/pg{major}/pg_dump[.exe]`.
pub struct BundledToolMatrix {
    roots: BTreeMap<u32, PathBuf>,
}

impl BundledToolMatrix {
    /// v1 지원 범위: PG13~17. 디렉토리가 없는 메이저는 건너뛴다.
    pub fn from_base_dir(base: &Path) -> Self {
        let mut roots = BTreeMap::new();
        for major in 13..=17u32 {
            let dir = base.join(format!("pg{major}"));
            if dir.is_dir() {
                roots.insert(major, dir);
            }
        }
        Self { roots }
    }

    /// 테스트/명시 구성용: 메이저→디렉토리 맵을 직접 지정.
    pub fn from_roots(roots: BTreeMap<u32, PathBuf>) -> Self {
        Self { roots }
    }

    pub fn select_pg_dump(&self, server_major: u32) -> Result<PathBuf, PgSourceError> {
        self.select_binary(server_major, bin_name("pg_dump"))
    }

    pub fn select_pg_dumpall(&self, server_major: u32) -> Result<PathBuf, PgSourceError> {
        self.select_binary(server_major, bin_name("pg_dumpall"))
    }

    /// 정확 일치가 있으면 그것. 없으면 server_major 이상 중 가장 낮은 메이저. 아니면 하드 실패.
    fn select_binary(&self, server_major: u32, filename: &str) -> Result<PathBuf, PgSourceError> {
        let dir = self
            .roots
            .get(&server_major)
            .or_else(|| self.roots.range(server_major..).next().map(|(_, d)| d))
            .ok_or(PgSourceError::NoBundledClientForServer { server_major })?;
        Ok(dir.join(filename))
    }
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
    fn t6_fallback_to_lowest_ge_server() {
        let m = roots(&[16, 17]); // pg15 없음
        let p = m.select_pg_dump(15).unwrap();
        assert!(p.to_string_lossy().contains("pg16"));
    }

    #[test]
    fn t7_unsupported_major_hard_fail() {
        let m = roots(&[13, 14, 15, 16, 17]);
        let err = m.select_pg_dump(18).unwrap_err();
        assert!(matches!(
            err,
            PgSourceError::NoBundledClientForServer { server_major: 18 }
        ));
    }
}
