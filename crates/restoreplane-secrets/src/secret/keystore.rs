// crates/restoreplane-secrets/src/secret/keystore.rs
//! 로컬 키스토어(gitignore 대상, 예: keystore.json). SecretManager를 재구성하는 데 필요한
//! 상태를 지속한다 — 봉투 최상위 키(KEK)를 3경로로 감싼 WrapSet + KEK로 감싼 서명 seed +
//! 복구코드 검증자(PHC) + 서명 공개키.
//!
//! 보안 규율:
//! - 디스크에 평문 키 자료를 두지 않는다. KEK는 비번(Argon2id)/복구코드/DPAPI로만 열리며,
//!   서명 개인키 seed는 KEK로 감싼 채 저장된다(KEK unlock 전에는 서명 불가).
//! - 서명 공개키(verifying_key)만 평문으로 둔다 — 검증에는 공개 정보라 안전하며, verify가
//!   이 핀된 공개키로 위조 매니페스트를 거부한다.
//! - init에서 표시한 복구코드는 저장하지 않는다(검증자 PHC만 보관, R4).

use std::path::{Path, PathBuf};

use data_encoding::HEXLOWER;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::secret::dek::Dek;
use crate::secret::error::SecretError;
use crate::secret::kdf::{generate_recovery_code, RecoveryVerifier};
use crate::secret::sign::generate_signing_keypair;
use crate::secret::wrap::{self, WrapSet};
use crate::secret::{Credential, SecretManager};

/// 현재 키스토어 스키마 버전.
pub const KEYSTORE_VERSION: u32 = 1;

/// 디스크에 직렬화되는 키스토어. 평문 개인키/DEK/KEK는 절대 담기지 않는다.
#[derive(Clone, Serialize, Deserialize)]
pub struct Keystore {
    pub version: u32,
    /// ed25519 서명 공개키(hex, 32B) — verify가 핀으로 대조(공개 정보라 평문 OK).
    pub verifying_key_hex: String,
    /// 서명 개인키 seed(32B)를 KEK로 감싼 스탠자. KEK unlock 전에는 열 수 없다.
    signing_key_wrap: WrapSet,
    /// KEK(봉투 최상위 키)를 3경로(비번/복구/DPAPI)로 감싼 WrapSet.
    /// argon2 salt+params, 복구 검증 wrap, DPAPI wrap이 모두 이 안의 스탠자에 들어 있다.
    kek_wrapset: WrapSet,
    /// 복구코드 검증자(PHC). 평문 복구코드는 저장하지 않는다.
    recovery_verifier: RecoveryVerifier,
    /// DPAPI keyfile 경로(비커밋). Windows unlock 경로에 필요.
    keyfile_path: PathBuf,
}

impl Keystore {
    /// 키스토어 파일이 이미 존재하는가.
    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// 최초 설정. KEK 발급 → 3경로 wrap, 서명 키페어 생성 → seed를 KEK로 wrap, 복구코드 1회 반환.
    /// DPAPI wrap은 실패해도 경고만 하고 진행한다(비번/복구 경로는 항상 확보).
    pub fn init(
        master_pw: &Zeroizing<String>,
        keyfile_path: PathBuf,
    ) -> Result<(Self, Zeroizing<String>), SecretError> {
        let recovery = generate_recovery_code();
        let recovery_verifier = RecoveryVerifier::from_code(&recovery)?;
        let (sk, vk) = generate_signing_keypair();
        let verifying_key_hex = HEXLOWER.encode(vk.as_bytes());

        // 1) KEK 발급 + 비번/복구 경로 wrap(항상), DPAPI 경로(Windows, 실패 허용).
        let kek = Dek::generate();
        let mut stanzas = vec![
            wrap::wrap_password(&kek, master_pw.as_bytes())?,
            wrap::wrap_recovery(&kek, recovery.as_bytes())?,
        ];
        #[cfg(windows)]
        {
            let mgr = SecretManager::new(keyfile_path.clone(), None, vk, None);
            match mgr.wrap_dpapi(&kek) {
                Ok(s) => stanzas.push(s),
                Err(e) => {
                    eprintln!(
                        "[경고] DPAPI 등록 실패 — 무인 실행 경로가 비활성화됩니다(비번/복구 경로는 정상): {e}"
                    );
                }
            }
        }
        let kek_wrapset = WrapSet {
            dek_wrap_gen: 1,
            stanzas,
        };

        // 2) 서명 seed(32B)를 KEK로 wrap해 저장(디스크 평문 개인키 금지).
        let seed = Dek::from_bytes(sk.to_bytes());
        let signing_key_wrap = WrapSet {
            dek_wrap_gen: 1,
            stanzas: vec![wrap::wrap_with_kek(&kek, &seed)?],
        };

        let ks = Keystore {
            version: KEYSTORE_VERSION,
            verifying_key_hex,
            signing_key_wrap,
            kek_wrapset,
            recovery_verifier,
            keyfile_path,
        };
        Ok((ks, recovery))
    }

    /// 디스크에서 로드.
    pub fn load(path: &Path) -> Result<Self, SecretError> {
        let bytes = std::fs::read(path)?;
        let ks: Keystore = serde_json::from_slice(&bytes)
            .map_err(|e| SecretError::Keystore(format!("파싱 실패: {e}")))?;
        if ks.version > KEYSTORE_VERSION {
            return Err(SecretError::Keystore(format!(
                "지원하지 않는 키스토어 버전 {}(최대 {KEYSTORE_VERSION})",
                ks.version
            )));
        }
        Ok(ks)
    }

    /// 디스크에 저장(pretty JSON).
    pub fn save(&self, path: &Path) -> Result<(), SecretError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| SecretError::Keystore(format!("직렬화 실패: {e}")))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, json)?;
        Ok(())
    }

    /// 핀된 서명 공개키.
    pub fn verifying_key(&self) -> Result<VerifyingKey, SecretError> {
        let raw = HEXLOWER
            .decode(self.verifying_key_hex.as_bytes())
            .map_err(|e| SecretError::Keystore(format!("공개키 hex 디코드 실패: {e}")))?;
        let arr: [u8; 32] = raw
            .as_slice()
            .try_into()
            .map_err(|_| SecretError::Keystore("공개키 길이가 32B가 아님".to_string()))?;
        VerifyingKey::from_bytes(&arr)
            .map_err(|e| SecretError::Keystore(format!("공개키 파싱 실패: {e}")))
    }

    /// 자격증명으로 KEK를 unlock(비번/복구/DPAPI). 복구코드 경로는 검증자(PHC)로 사전 검증한다.
    pub fn unlock_kek(&self, cred: &Credential) -> Result<Dek, SecretError> {
        self.verify_manager()?.unwrap(&self.kek_wrapset, cred)
    }

    /// KEK로 서명 개인키를 복구한다(서명 직전에만 호출).
    pub fn signing_key(&self, kek: &Dek) -> Result<SigningKey, SecretError> {
        let st = self
            .signing_key_wrap
            .stanzas
            .first()
            .ok_or_else(|| SecretError::Keystore("서명키 스탠자 없음".to_string()))?;
        let seed = wrap::unwrap_with_kek(kek, st)?;
        Ok(SigningKey::from_bytes(seed.expose()))
    }

    /// per-backup DEK를 KEK로 감싼 WrapSet 생성(매니페스트 동봉용).
    pub fn wrap_dek(&self, kek: &Dek, dek: &Dek, gen: u32) -> Result<WrapSet, SecretError> {
        Ok(WrapSet {
            dek_wrap_gen: gen,
            stanzas: vec![wrap::wrap_with_kek(kek, dek)?],
        })
    }

    /// 매니페스트 WrapSet(KEK로 감싼 DEK)에서 DEK를 복구한다.
    pub fn unwrap_dek(&self, kek: &Dek, set: &WrapSet) -> Result<Dek, SecretError> {
        for st in &set.stanzas {
            if let Ok(d) = wrap::unwrap_with_kek(kek, st) {
                return Ok(d);
            }
        }
        Err(SecretError::NoUsableStanza)
    }

    /// 검증/unlock 전용 SecretManager(서명 개인키 없음). 복구코드 검증자를 포함해 recovery 경로 지원.
    pub fn verify_manager(&self) -> Result<SecretManager, SecretError> {
        Ok(SecretManager::new(
            self.keyfile_path.clone(),
            None,
            self.verifying_key()?,
            Some(self.recovery_verifier.clone()),
        ))
    }
}
