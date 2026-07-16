// crates/dbbackup-secrets/src/secret/field.rs
//! dbbackup-core의 SecretEncryptor 경계 구현. config의 평문 필드를 현재 DEK로 암호화해
//! `enc:<wrap_gen>:<base64>` (EncField)로 변환한다. wrap_gen을 carry해 rotate-key 대상 식별을
//! 결정적으로 만든다(core::EncField 주석의 P2-1).

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use dbbackup_core::{EncField, SecretEncryptError, SecretEncryptor};
use rand::rngs::OsRng;
use rand::RngCore;
use zeroize::Zeroizing;

use crate::secret::dek::Dek;

/// 현재 DEK와 wrap 세대를 들고 config 필드를 암호화하는 어댑터.
/// ciphertext 레이아웃 = 12B nonce || AEAD(ct+tag).
pub struct DekEncryptor {
    dek: Dek,
    wrap_gen: u32,
}

impl DekEncryptor {
    pub fn new(dek: Dek, wrap_gen: u32) -> Self {
        Self { dek, wrap_gen }
    }

    /// 라운드트립 검증/복호용. nonce를 앞에서 떼어내고 복호한다.
    pub fn unwrap_field(&self, field: &EncField) -> Result<Zeroizing<String>, SecretEncryptError> {
        if field.ciphertext.len() < 12 {
            return Err(SecretEncryptError::Unavailable("enc field too short"));
        }
        let (nonce, ct) = field.ciphertext.split_at(12);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(self.dek.expose()));
        let pt = cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| SecretEncryptError::Unavailable("aead decrypt failed"))?;
        let s = String::from_utf8(pt)
            .map_err(|_| SecretEncryptError::Unavailable("plaintext not utf8"))?;
        Ok(Zeroizing::new(s))
    }
}

impl SecretEncryptor for DekEncryptor {
    fn wrap(&self, plaintext: &str) -> Result<EncField, SecretEncryptError> {
        let cipher = ChaCha20Poly1305::new(Key::from_slice(self.dek.expose()));
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_bytes())
            .map_err(|_| SecretEncryptError::Unavailable("aead encrypt failed"))?;
        let mut buf = nonce.to_vec();
        buf.extend_from_slice(&ct);
        Ok(EncField::new(self.wrap_gen, buf))
    }
}
