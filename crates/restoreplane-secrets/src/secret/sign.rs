// crates/restoreplane-secrets/src/secret/sign.rs
//! ed25519 오프라인 서명/검증(C3, R7). 대칭 AEAD 무결성만으로는 위조를 막을 수 없으므로
//! 매니페스트는 비대칭 서명으로 출처를 인증한다.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand::rngs::OsRng;

use crate::secret::error::SecretError;

/// init에서 오프라인 키페어 생성. 개인키는 오프라인/비커밋, 공개키는 config에 핀한다.
pub fn generate_signing_keypair() -> (SigningKey, VerifyingKey) {
    let sk = SigningKey::generate(&mut OsRng);
    let vk = sk.verifying_key();
    (sk, vk)
}

/// 매니페스트는 결정적(canonical) 직렬화 후 서명한다(S08과 공유하는 규칙).
pub fn sign_manifest(sk: &SigningKey, canonical_bytes: &[u8]) -> Signature {
    sk.sign(canonical_bytes)
}

/// 복원 시 핀된 공개키로 검증. 실패/미서명이면 복원 거부(Origin Authentication).
pub fn verify_manifest(
    vk: &VerifyingKey,
    canonical_bytes: &[u8],
    sig: &Signature,
) -> Result<(), SecretError> {
    vk.verify(canonical_bytes, sig)
        .map_err(|_| SecretError::SignatureRejected)
}
