// crates/dbbackup-secrets/src/secret/dek.rs
//! Dek — 백업 1건당 새로 발급되는 데이터 암호화 키(R1, C2) + payload STREAM primitive.
//!
//! payload 계층 분리(P0-5, R6): S02는 payload를 직접 암호화하지 않고 DEK로부터 STREAM
//! Encryptor/Decryptor만 만들어 S05에 넘긴다. wrap 계층은 wrap.rs가 담당한다.

use chacha20poly1305::aead::generic_array::GenericArray;
use chacha20poly1305::aead::stream::{DecryptorBE32, EncryptorBE32};
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit};
use rand::rngs::OsRng;
use rand::RngCore;
use zeroize::Zeroizing;

/// 백업 1건당 새로 발급되는 32B 데이터 암호화 키. Drop 시 메모리에서 소거된다.
/// Debug를 파생하지 않는다(R11: 키 자료 로그 노출 금지).
#[derive(Clone)]
pub struct Dek(Zeroizing<[u8; 32]>);

impl Dek {
    /// CSPRNG(OsRng)로 새 DEK 발급 — 백업마다 호출(R1).
    pub fn generate() -> Self {
        let mut k = [0u8; 32];
        OsRng.fill_bytes(&mut k);
        Dek(Zeroizing::new(k))
    }

    /// wrap/payload 계층 전용 접근자(내부 크레이트 가시성).
    pub(crate) fn expose(&self) -> &[u8; 32] {
        &self.0
    }

    pub(crate) fn from_bytes(b: [u8; 32]) -> Self {
        Dek(Zeroizing::new(b))
    }

    /// payload 암호화용 STREAM Encryptor(S05가 zstd 출력에 감아 사용).
    /// nonce_prefix는 DEK가 백업마다 새로우므로 상수여도 재사용 위험이 없다.
    pub fn payload_encryptor(&self, nonce_prefix: &[u8; 7]) -> EncryptorBE32<ChaCha20Poly1305> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&*self.0));
        EncryptorBE32::from_aead(cipher, GenericArray::from_slice(nonce_prefix))
    }

    /// payload 복호용 STREAM Decryptor.
    pub fn payload_decryptor(&self, nonce_prefix: &[u8; 7]) -> DecryptorBE32<ChaCha20Poly1305> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&*self.0));
        DecryptorBE32::from_aead(cipher, GenericArray::from_slice(nonce_prefix))
    }
}
