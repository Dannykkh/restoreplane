// crates/dbbackup-secrets/src/secret/wrap.rs
//! wrap 계층 — chacha20poly1305 원샷 AEAD로 32B DEK를 감싼다(R2, R6).
//! payload 계층(dek.rs의 STREAM)과 물리적으로 분리된다(P0-5).

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::secret::dek::Dek;
use crate::secret::error::SecretError;
use crate::secret::kdf::{derive_kek, M_COST, P_COST, T_COST};

/// DEK 1개를 감싼 self-describing 스탠자. 각 경로는 독립적으로 DEK를 복구할 수 있다(R2).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind")]
pub enum WrapStanza {
    /// (a) 마스터 비밀번호 Argon2id-KEK로 wrap.
    Argon2id {
        salt: [u8; 16],
        m_cost: u32,
        t_cost: u32,
        p_cost: u32,
        nonce: [u8; 12],
        ct: Vec<u8>,
    },
    /// (b) 복구코드 Argon2id-KEK로 wrap(고엔트로피).
    RecoveryCode {
        salt: [u8; 16],
        m_cost: u32,
        t_cost: u32,
        p_cost: u32,
        nonce: [u8; 12],
        ct: Vec<u8>,
    },
    /// (c) DPAPI Machine-scope로 wrap(머신+keyfile 바인딩).
    Dpapi {
        machine_id: String,
        keyfile_id: String,
        blob: Vec<u8>,
    },
    /// (d) 봉투 최상위 키(KEK)로 32B 키(per-backup DEK 또는 서명 seed)를 직접 wrap.
    /// 키스토어의 KEK를 unlock한 뒤에만 unwrap 가능 — 매니페스트 동봉 WrapSet과
    /// 키스토어의 서명키 보관에 쓴다(자격증명 파라미터가 없어 KEK 없이는 열 수 없다).
    Kek { nonce: [u8; 12], ct: Vec<u8> },
}

/// 한 백업의 DEK를 감싼 다경로 스탠자 묶음. 매니페스트에 동봉되어 서명된다.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WrapSet {
    /// rotate-key 세대(R8). unwrap은 최고 세대를 우선한다.
    pub dek_wrap_gen: u32,
    /// 보통 경로별 1개(비번/복구/DPAPI). register-dpapi로 추가될 수 있다.
    pub stanzas: Vec<WrapStanza>,
}

/// KEK로 DEK를 원샷 AEAD wrap. (nonce, ct=32B DEK+16B tag) 반환.
fn aead_wrap(kek: &[u8; 32], dek: &Dek) -> Result<([u8; 12], Vec<u8>), SecretError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(kek));
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), dek.expose().as_slice())
        .map_err(|_| SecretError::WrapFailed)?;
    Ok((nonce, ct))
}

/// KEK로 원샷 AEAD unwrap. 태그 검증 실패(잘못된 비번 등)면 UnwrapFailed.
fn aead_unwrap(kek: &[u8; 32], nonce: &[u8; 12], ct: &[u8]) -> Result<Dek, SecretError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(kek));
    let pt = Zeroizing::new(
        cipher
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| SecretError::UnwrapFailed)?,
    );
    let arr: [u8; 32] = pt
        .as_slice()
        .try_into()
        .map_err(|_| SecretError::BadDekLen)?;
    Ok(Dek::from_bytes(arr))
}

/// (d) KEK로 32B 키(DEK/서명 seed)를 직접 wrap. KDF 없이 KEK 바이트를 그대로 AEAD 키로 쓴다.
pub(crate) fn wrap_with_kek(kek: &Dek, target: &Dek) -> Result<WrapStanza, SecretError> {
    let (nonce, ct) = aead_wrap(kek.expose(), target)?;
    Ok(WrapStanza::Kek { nonce, ct })
}

/// (d) KEK로 wrap된 스탠자를 unwrap. Kek 스탠자가 아니면 WrongStanza.
pub(crate) fn unwrap_with_kek(kek: &Dek, st: &WrapStanza) -> Result<Dek, SecretError> {
    if let WrapStanza::Kek { nonce, ct } = st {
        aead_unwrap(kek.expose(), nonce, ct)
    } else {
        Err(SecretError::WrongStanza)
    }
}

pub fn wrap_dek_with_kek(kek: &Dek, dek: &Dek, generation: u32) -> Result<WrapSet, SecretError> {
    if generation == 0 {
        return Err(SecretError::WrapFailed);
    }
    Ok(WrapSet {
        dek_wrap_gen: generation,
        stanzas: vec![wrap_with_kek(kek, dek)?],
    })
}

pub fn unwrap_dek_with_kek(kek: &Dek, set: &WrapSet) -> Result<Dek, SecretError> {
    for stanza in &set.stanzas {
        if let Ok(dek) = unwrap_with_kek(kek, stanza) {
            return Ok(dek);
        }
    }
    Err(SecretError::NoUsableStanza)
}

/// (a) 비밀번호 경로 wrap.
pub(crate) fn wrap_password(dek: &Dek, password: &[u8]) -> Result<WrapStanza, SecretError> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let kek = derive_kek(password, &salt, M_COST, T_COST, P_COST)?;
    let (nonce, ct) = aead_wrap(&kek, dek)?;
    Ok(WrapStanza::Argon2id {
        salt,
        m_cost: M_COST,
        t_cost: T_COST,
        p_cost: P_COST,
        nonce,
        ct,
    })
}

/// (b) 복구코드 경로 wrap.
pub(crate) fn wrap_recovery(dek: &Dek, code: &[u8]) -> Result<WrapStanza, SecretError> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let kek = derive_kek(code, &salt, M_COST, T_COST, P_COST)?;
    let (nonce, ct) = aead_wrap(&kek, dek)?;
    Ok(WrapStanza::RecoveryCode {
        salt,
        m_cost: M_COST,
        t_cost: T_COST,
        p_cost: P_COST,
        nonce,
        ct,
    })
}

/// (a) 비밀번호 경로 unwrap — 스탠자에 저장된 params로 재유도(T15).
pub(crate) fn unwrap_password(st: &WrapStanza, password: &[u8]) -> Result<Dek, SecretError> {
    if let WrapStanza::Argon2id {
        salt,
        m_cost,
        t_cost,
        p_cost,
        nonce,
        ct,
    } = st
    {
        let kek = derive_kek(password, salt, *m_cost, *t_cost, *p_cost)?;
        aead_unwrap(&kek, nonce, ct)
    } else {
        Err(SecretError::WrongStanza)
    }
}

/// (b) 복구코드 경로 unwrap.
pub(crate) fn unwrap_recovery(st: &WrapStanza, code: &[u8]) -> Result<Dek, SecretError> {
    if let WrapStanza::RecoveryCode {
        salt,
        m_cost,
        t_cost,
        p_cost,
        nonce,
        ct,
    } = st
    {
        let kek = derive_kek(code, salt, *m_cost, *t_cost, *p_cost)?;
        aead_unwrap(&kek, nonce, ct)
    } else {
        Err(SecretError::WrongStanza)
    }
}
