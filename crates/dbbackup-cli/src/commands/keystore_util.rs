// crates/dbbackup-cli/src/commands/keystore_util.rs
//! init/run/verify/restore가 공유하는 키스토어 경로 해석 + 자격증명 수집 헬퍼.
//! 테스트가 비대화(무인)로 구동할 수 있도록 마스터 비밀번호는 env(DBBACKUP_MASTER_PASSWORD)로도 받는다.

use std::path::{Path, PathBuf};

use dbbackup_secrets::Credential;
use zeroize::Zeroizing;

/// 무인 실행/테스트용 마스터 비밀번호 env 변수.
pub const MASTER_PW_ENV: &str = "DBBACKUP_MASTER_PASSWORD";

/// config 옆의 키스토어 경로(예: dbbackup.toml → 같은 폴더의 keystore.json).
pub fn keystore_path(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("keystore.json"),
        _ => PathBuf::from("keystore.json"),
    }
}

/// DPAPI keyfile 경로(키스토어 옆, 비커밋 *.keyfile).
pub fn keyfile_path(config_path: &Path) -> PathBuf {
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("dpapi.keyfile"),
        _ => PathBuf::from("dpapi.keyfile"),
    }
}

/// env에 마스터 비밀번호가 있으면 반환(무인/테스트 경로).
pub fn master_pw_from_env() -> Option<Zeroizing<String>> {
    std::env::var(MASTER_PW_ENV).ok().map(Zeroizing::new)
}

/// 마스터 비밀번호를 env → (대화형 프롬프트) 순으로 얻는다. prompt=false면 env만.
pub fn resolve_master_pw(prompt_label: &str, allow_prompt: bool) -> anyhow::Result<Zeroizing<String>> {
    if let Some(pw) = master_pw_from_env() {
        return Ok(pw);
    }
    if allow_prompt {
        let pw = rpassword::prompt_password(prompt_label)?;
        return Ok(Zeroizing::new(pw));
    }
    anyhow::bail!(
        "마스터 비밀번호가 필요합니다 — 무인 실행은 {MASTER_PW_ENV} 환경변수로 제공하세요"
    )
}

/// unlock 자격증명 결정: --with-recovery-code면 복구코드 프롬프트, 아니면 비번(env/프롬프트).
/// use_dpapi가 true이고 다른 자격증명이 없으면 DPAPI(무인) 경로.
pub fn resolve_unlock_credential(
    with_recovery_code: bool,
    allow_prompt: bool,
) -> anyhow::Result<Credential> {
    if with_recovery_code {
        let code = if allow_prompt {
            rpassword::prompt_password("복구코드: ")?
        } else {
            std::env::var("DBBACKUP_RECOVERY_CODE").map_err(|_| {
                anyhow::anyhow!("복구코드가 필요합니다 — DBBACKUP_RECOVERY_CODE 환경변수로 제공하세요")
            })?
        };
        return Ok(Credential::Recovery(Zeroizing::new(code)));
    }
    // env 비번이 있으면 비번 경로, 없고 프롬프트 불가면 DPAPI(무인) 시도.
    if let Some(pw) = master_pw_from_env() {
        return Ok(Credential::Password(pw));
    }
    if allow_prompt {
        let pw = rpassword::prompt_password("마스터 비밀번호: ")?;
        return Ok(Credential::Password(Zeroizing::new(pw)));
    }
    Ok(Credential::Dpapi)
}
