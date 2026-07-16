// crates/dbbackup-secrets/src/secret/mod.rs
//! SecretManager 진입점 + 공개 API(issue_and_wrap / unwrap / sign / verify / rotate / keys status).

pub mod dek;
pub mod error;
pub mod field;
pub mod kdf;
pub mod sign;
pub mod wrap;

pub mod dpapi;
pub mod rotate;

use std::path::PathBuf;

use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub use dek::Dek;
pub use error::SecretError;
pub use kdf::RecoveryVerifier;
pub use rotate::RotateReport;
pub use wrap::{WrapSet, WrapStanza};

/// unwrap 자격증명. 어느 경로로 DEK를 복구할지 선택한다.
pub enum Credential {
    /// 무인 실행 — 로컬 keyfile + DPAPI Machine-scope.
    Dpapi,
    /// 마스터 비밀번호(이식성).
    Password(Zeroizing<String>),
    /// 복구코드(비번 분실 대비).
    Recovery(Zeroizing<String>),
}

/// S02 진입점. 서명 개인키는 서명 시에만 로드(오프라인), 공개키는 config에 핀된다.
pub struct SecretManager {
    signing_key: Option<SigningKey>,
    verifying_key: VerifyingKey,
    keyfile_path: PathBuf,
    recovery_verifier: Option<RecoveryVerifier>,
}

impl SecretManager {
    /// 매니저 구성. signing_key는 서명이 필요한 경로에서만 Some.
    pub fn new(
        keyfile_path: PathBuf,
        signing_key: Option<SigningKey>,
        verifying_key: VerifyingKey,
        recovery_verifier: Option<RecoveryVerifier>,
    ) -> Self {
        Self {
            signing_key,
            verifying_key,
            keyfile_path,
            recovery_verifier,
        }
    }

    /// 핀된 공개키 접근자.
    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.verifying_key
    }

    pub(crate) fn has_signing_key(&self) -> bool {
        self.signing_key.is_some()
    }

    /// 공개키 지문(hex 앞 8글자) — keys status 표기용.
    pub(crate) fn verifying_fingerprint(&self) -> String {
        let d = Sha256::digest(self.verifying_key.as_bytes());
        d.iter().take(4).map(|b| format!("{:02x}", b)).collect()
    }

    /// 백업 실행 시: 새 DEK 발급 + 사용 가능한 경로로 wrap → WrapSet(매니페스트 동봉).
    /// Windows에서는 DPAPI 스탠자까지 3경로, 그 외에는 비번+복구 2경로.
    pub fn issue_and_wrap(
        &self,
        master_pw: &Zeroizing<String>,
        recovery_code: &Zeroizing<String>,
        gen: u32,
    ) -> Result<(Dek, WrapSet), SecretError> {
        let dek = Dek::generate(); // R1
        let mut stanzas = Vec::with_capacity(3);
        stanzas.push(wrap::wrap_password(&dek, master_pw.as_bytes())?); // (a)
        stanzas.push(wrap::wrap_recovery(&dek, recovery_code.as_bytes())?); // (b)
        #[cfg(windows)]
        {
            stanzas.push(self.wrap_dpapi(&dek)?); // (c)
        }
        Ok((
            dek,
            WrapSet {
                dek_wrap_gen: gen,
                stanzas,
            },
        ))
    }

    /// DPAPI 스탠자 생성(Windows 전용). keyfile 유도 entropy로 wrap한다(C1/R5).
    #[cfg(windows)]
    pub(crate) fn wrap_dpapi(&self, dek: &Dek) -> Result<WrapStanza, SecretError> {
        let kf = dpapi::load_or_create_keyfile(&self.keyfile_path)?;
        let entropy = dpapi::derive_dpapi_entropy(&kf);
        let blob = dpapi::dpapi_wrap(dek, &entropy)?;
        Ok(WrapStanza::Dpapi {
            machine_id: machine_id(),
            keyfile_id: dpapi::keyfile_fingerprint(&kf),
            blob,
        })
    }

    /// 복원/rotate 시: 자격증명에 맞는 스탠자로 DEK를 복구한다.
    pub fn unwrap(&self, set: &WrapSet, cred: &Credential) -> Result<Dek, SecretError> {
        for st in &set.stanzas {
            match (st, cred) {
                #[cfg(windows)]
                (WrapStanza::Dpapi { blob, .. }, Credential::Dpapi) => {
                    return self.unwrap_dpapi(blob);
                }
                (WrapStanza::Argon2id { .. }, Credential::Password(pw)) => {
                    return wrap::unwrap_password(st, pw.as_bytes());
                }
                (WrapStanza::RecoveryCode { .. }, Credential::Recovery(code)) => {
                    if let Some(v) = &self.recovery_verifier {
                        if !v.verify(code) {
                            return Err(SecretError::RecoveryRejected);
                        }
                    }
                    return wrap::unwrap_recovery(st, code.as_bytes());
                }
                _ => continue,
            }
        }
        Err(SecretError::NoUsableStanza)
    }

    /// DPAPI 스탠자 unwrap(Windows 전용).
    #[cfg(windows)]
    pub(crate) fn unwrap_dpapi(&self, blob: &[u8]) -> Result<Dek, SecretError> {
        let kf = dpapi::load_or_create_keyfile(&self.keyfile_path)?;
        let entropy = dpapi::derive_dpapi_entropy(&kf);
        dpapi::dpapi_unwrap(blob, &entropy)
    }

    /// 매니페스트 서명(개인키 로드 시에만). canonical 직렬화는 호출자(S08) 책임.
    pub fn sign(&self, canonical_bytes: &[u8]) -> Result<Signature, SecretError> {
        let sk = self
            .signing_key
            .as_ref()
            .ok_or(SecretError::SigningKeyMissing)?;
        Ok(sign::sign_manifest(sk, canonical_bytes))
    }

    /// 매니페스트 검증. 실패/미서명이면 HARD FAIL(우회 플래그 없음, C3).
    pub fn verify(&self, canonical_bytes: &[u8], sig: &Signature) -> Result<(), SecretError> {
        sign::verify_manifest(&self.verifying_key, canonical_bytes, sig)
    }

    /// rotate-key: 덤프 재암호화 없이 DEK만 새 세대로 재-wrap(R8).
    pub fn rotate(
        &self,
        set: &WrapSet,
        old: &Credential,
        new_pw: &Zeroizing<String>,
        new_recovery: &Zeroizing<String>,
        next_gen: u32,
    ) -> Result<WrapSet, SecretError> {
        rotate::rotate_wrapset(self, set, old, new_pw, new_recovery, next_gen)
    }

    /// 새 PC: 비번/복구코드로 DEK unlock(무인 DPAPI 경로 없이, R10).
    pub fn unlock(&self, set: &WrapSet, cred: &Credential) -> Result<Dek, SecretError> {
        self.unwrap(set, cred)
    }

    /// 새 PC에서 무인 실행 등록: unlock 후 현재 세대에 DPAPI 스탠자를 추가한다(R10, Windows 전용).
    #[cfg(windows)]
    pub fn register_dpapi(
        &self,
        set: &mut WrapSet,
        unlock_cred: &Credential,
    ) -> Result<(), SecretError> {
        let dek = self.unlock(set, unlock_cred)?;
        let stanza = self.wrap_dpapi(&dek)?;
        set.stanzas.push(stanza);
        Ok(())
    }

    /// keys status(R9) 요약 출력.
    pub fn keys_status(&self, set: &WrapSet) -> String {
        rotate::keys_status(self, set)
    }
}

/// 등록 머신 식별자(keys status 표기용). 실패 시 "unknown".
#[cfg(windows)]
fn machine_id() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "unknown".to_string())
}
