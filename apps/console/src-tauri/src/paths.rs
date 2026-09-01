//! config/keystore/keyfile 경로 해석 — CLI(keystore_util)와 동일 규칙을 재현한다.
//! config 파일 위치는 환경변수 RESTOREPLANE_CONFIG 우선, 없으면 작업 디렉토리의 restoreplane.toml.

use std::path::{Path, PathBuf};

pub fn config_path() -> PathBuf {
    std::env::var_os("RESTOREPLANE_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("restoreplane.toml"))
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

/// 테스트 직렬화용 잠금 — RESTOREPLANE_CONFIG(프로세스 전역 env)를 만지는 테스트들이
/// 병렬 실행 시 서로의 경로를 덮어쓰지 않도록 공유한다. poison은 무시(테스트 실패 신호는 별개).
#[cfg(test)]
pub(crate) fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}
