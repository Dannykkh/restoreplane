// S08 8.3/8.8 — 서명 게이트. 목적지에서 읽은 매니페스트는 사용 전 반드시 이 게이트를 통과한다.
// 파이프라인의 SignedManifest(ed25519)를 재사용하며(병렬 매니페스트 금지), 검증은 config에
// 핀된 공개키(pinned VerifyingKey)로 수행한다. 미서명/위조는 즉시 Err — 경고 후 통과 금지(H5/C3).

use dbbackup_pipeline::{Manifest, SignedManifest};
use ed25519_dalek::VerifyingKey;
use thiserror::Error;

use crate::source::ManifestSource;

#[derive(Debug, Error)]
pub enum GateError {
    #[error("매니페스트가 서명 봉투 형식이 아님(미서명) — 신뢰 불가, 거부")]
    Unsigned,
    #[error("서명 검증 실패 — 위조/치환 의심, 거부")]
    TamperedOrForged,
    #[error("목적지 I/O 실패: {0}")]
    Io(String),
}

/// 서명 게이트의 핵심 판단. 파싱 실패(봉투 아님) = Unsigned, 서명 불일치 = TamperedOrForged.
pub trait ManifestVerifier: Send + Sync {
    fn verify_signed(&self, signed: &SignedManifest) -> Result<(), GateError>;
}

/// config에 핀된 ed25519 공개키로 검증하는 정규 구현.
pub struct PinnedKeyVerifier {
    pub key: VerifyingKey,
}

impl PinnedKeyVerifier {
    pub fn new(key: VerifyingKey) -> Self {
        Self { key }
    }
}

impl ManifestVerifier for PinnedKeyVerifier {
    fn verify_signed(&self, signed: &SignedManifest) -> Result<(), GateError> {
        signed
            .verify(&self.key)
            .map_err(|_| GateError::TamperedOrForged)
    }
}

/// 매니페스트에 동봉된 공개키로 검증하는 TOFU(trust-on-first-use) 구현. config에 서명 공개키가
/// 아직 핀되기 전(슬라이스 1) CLI 배선용이다. 본문 변조는 잡지만, 공격자가 자기 키로 재서명하면서
/// 동봉 공개키까지 바꾼 위조는 잡지 못한다 — 정식 방어는 PinnedKeyVerifier(config 핀 공개키)다.
pub struct EmbeddedKeyVerifier;

impl ManifestVerifier for EmbeddedKeyVerifier {
    fn verify_signed(&self, signed: &SignedManifest) -> Result<(), GateError> {
        let raw = hex_decode_32(&signed.verifying_key).ok_or(GateError::TamperedOrForged)?;
        let vk = VerifyingKey::from_bytes(&raw).map_err(|_| GateError::TamperedOrForged)?;
        signed.verify(&vk).map_err(|_| GateError::TamperedOrForged)
    }
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, chunk) in bytes.chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// 바이트 → (파싱 + 서명 검증) → 신뢰된 Manifest. 서명 봉투가 아니면 Unsigned.
pub fn verify_bytes(bytes: &[u8], verifier: &dyn ManifestVerifier) -> Result<Manifest, GateError> {
    // 봉투(SignedManifest)로 파싱되지 않으면 = 서명이 없는 것으로 간주(미서명 거부).
    let signed: SignedManifest = serde_json::from_slice(bytes).map_err(|_| GateError::Unsigned)?;
    verifier.verify_signed(&signed)?;
    Ok(signed.manifest)
}

/// 복원/검증 직전 호출(S09 소비). 카탈로그 값이 아니라 목적지에서 새로 읽은 바이트만 신뢰한다(H5).
pub fn load_and_verify_manifest_for_restore(
    source: &dyn ManifestSource,
    verifier: &dyn ManifestVerifier,
    manifest_key: &str,
) -> Result<Manifest, GateError> {
    let bytes = source
        .read_bytes(manifest_key)
        .map_err(|e| GateError::Io(e.to_string()))?;
    verify_bytes(&bytes, verifier)
}
