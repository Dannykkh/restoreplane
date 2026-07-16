//! 초기 설정(init) 커맨드 — 인프로세스로 dbbackup-secrets를 직접 호출한다.
//!
//! CLI `init`과 동일한 결과(키스토어 3경로 wrap + 서명 키페어 + 복구코드 1회)를 만들되,
//! 대화형 프롬프트 대신 GUI가 넘긴 마스터 비밀번호를 쓴다. 사이드카가 아닌 인프로세스인 이유:
//! Keystore::init은 순수 라이브러리 호출이라 타입 안전하고 검증이 쉽다(액션 파이프라인만 사이드카).

use dbbackup_secrets::Keystore;
use serde::Serialize;
use zeroize::Zeroizing;

use crate::paths::{config_path, keyfile_path, keystore_path};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeystoreStatus {
    config_path: String,
    config_present: bool,
    keystore_present: bool,
}

#[tauri::command]
pub fn keystore_status() -> KeystoreStatus {
    let cfg = config_path();
    KeystoreStatus {
        config_present: cfg.exists(),
        keystore_present: Keystore::exists(&keystore_path(&cfg)),
        config_path: cfg.display().to_string(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitResult {
    /// 단 한 번만 표시된다. 평문 저장 금지(UI가 사용자에게 보관을 강제).
    recovery_code: String,
    verifying_key_hex: String,
    keystore_path: String,
}

/// 마스터 비밀번호로 키스토어를 생성한다. 이미 있으면 거부(덮어쓰면 기존 백업 DEK 복구 불가).
#[tauri::command]
pub fn init_keystore(master_password: String) -> Result<InitResult, String> {
    let cfg = config_path();
    let ks_path = keystore_path(&cfg);

    if Keystore::exists(&ks_path) {
        return Err(format!(
            "키스토어가 이미 존재합니다({}). 재설정은 rotate-key 경로로 진행하세요.",
            ks_path.display()
        ));
    }
    if master_password.trim().len() < 8 {
        return Err("마스터 비밀번호는 8자 이상이어야 합니다.".to_string());
    }

    let pw = Zeroizing::new(master_password);
    let (keystore, recovery) = Keystore::init(&pw, keyfile_path(&cfg))
        .map_err(|e| format!("키스토어 초기화 실패: {e}"))?;
    keystore
        .save(&ks_path)
        .map_err(|e| format!("키스토어 저장 실패: {e}"))?;

    Ok(InitResult {
        recovery_code: recovery.to_string(),
        verifying_key_hex: keystore.verifying_key_hex.clone(),
        keystore_path: ks_path.display().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_keystore_then_refuses_overwrite() {
        let _env = crate::paths::env_guard();
        let dir = std::env::temp_dir().join(format!("dbconsole-init-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("dbbackup.toml");
        // config_path()가 이 경로를 쓰도록 env 지정(인프로세스 커맨드 경로 검증).
        std::env::set_var("DBBACKUP_CONFIG", &cfg);

        // 미설정 상태.
        assert!(!keystore_status().keystore_present);

        // 최초 init: 키스토어 생성 + 복구코드 반환.
        let res = init_keystore("correct horse battery".to_string()).expect("init 성공");
        assert!(!res.recovery_code.trim().is_empty(), "복구코드 1회 반환");
        assert_eq!(
            res.verifying_key_hex.len(),
            64,
            "ed25519 공개키 32B=hex 64자"
        );
        assert!(keystore_path(&cfg).exists(), "keystore.json 기록");
        assert!(keystore_status().keystore_present);

        // 재init 거부(덮어쓰기 금지).
        assert!(init_keystore("another".to_string()).is_err());

        // 짧은 비번 거부(별도 임시 경로).
        let dir2 = dir.join("short");
        std::fs::create_dir_all(&dir2).unwrap();
        std::env::set_var("DBBACKUP_CONFIG", dir2.join("dbbackup.toml"));
        assert!(init_keystore("short".to_string()).is_err(), "8자 미만 거부");

        std::env::remove_var("DBBACKUP_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
