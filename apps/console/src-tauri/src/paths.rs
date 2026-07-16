//! config/keystore/keyfile 경로 해석 — CLI(keystore_util)와 동일 규칙을 재현한다.
//! config 파일 위치는 환경변수 DBBACKUP_CONFIG 우선, 없으면 작업 디렉토리의 dbbackup.toml.

use std::path::{Path, PathBuf};

pub fn config_path() -> PathBuf {
    std::env::var_os("DBBACKUP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("dbbackup.toml"))
}

fn sibling(cfg_path: &Path, name: &str) -> PathBuf {
    match cfg_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
        _ => PathBuf::from(name),
    }
}

pub fn keystore_path(cfg_path: &Path) -> PathBuf {
    sibling(cfg_path, "keystore.json")
}

pub fn keyfile_path(cfg_path: &Path) -> PathBuf {
    sibling(cfg_path, "dpapi.keyfile")
}
