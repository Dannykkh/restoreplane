// crates/dbbackup-secrets/src/secret/kdf.rs
//! Argon2id KEK 유도(R3) + 복구코드 생성/검증(R4).
//!
//! 파라미터(m=64MiB, t=3, p=1, v0x13)는 스탠자에 저장되어 재유도 가능해야 한다(T15 gotcha 방지).

use argon2::{Algorithm, Argon2, Params, Version};
use data_encoding::BASE32_NOPAD;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::secret::error::SecretError;

/// Argon2id memory cost — 단위 KiB(65536 = 64MiB).
pub const M_COST: u32 = 65536;
/// Argon2id time cost(패스 수).
pub const T_COST: u32 = 3;
/// Argon2id parallelism.
pub const P_COST: u32 = 1;

/// 마스터 비번/복구코드 → 32B KEK. 파라미터는 호출자가 스탠자에 저장한다.
pub(crate) fn derive_kek(
    secret: &[u8],
    salt: &[u8],
    m: u32,
    t: u32,
    p: u32,
) -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let params = Params::new(m, t, p, Some(32)).map_err(|e| SecretError::Kdf(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut kek = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(secret, salt, kek.as_mut_slice())
        .map_err(|e| SecretError::Kdf(e.to_string()))?;
    Ok(kek)
}

/// 128bit 이상 엔트로피 복구코드. 20B(160bit) 랜덤 → Crockford 계열 base32 → 그룹 표시.
/// init 시 1회만 표시하고 평문 저장하지 않는다(R4). 반환값은 Drop 시 소거된다.
pub fn generate_recovery_code() -> Zeroizing<String> {
    let mut raw = [0u8; 20];
    OsRng.fill_bytes(&mut raw);
    let code = BASE32_NOPAD.encode(&raw);
    raw.zeroize();
    Zeroizing::new(group_dashes(&code))
}

/// XXXXX-XXXXX-... 형태로 5글자마다 하이픈을 넣어 오타를 줄인다.
fn group_dashes(s: &str) -> String {
    s.as_bytes()
        .chunks(5)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect::<Vec<_>>()
        .join("-")
}

/// 복구코드 검증자 — 평문이 아니라 Argon2 PHC 해시만 보관한다(R4).
/// init 시 코드로부터 생성하고, unlock 시 입력 코드를 검증한다.
#[derive(Clone, Serialize, Deserialize)]
pub struct RecoveryVerifier {
    phc: String,
}

impl RecoveryVerifier {
    /// 복구코드로부터 검증자 생성(평문은 저장하지 않음).
    pub fn from_code(code: &str) -> Result<Self, SecretError> {
        use argon2::password_hash::rand_core::OsRng as PhOsRng;
        use argon2::password_hash::{PasswordHasher, SaltString};
        let salt = SaltString::generate(&mut PhOsRng);
        let hash = Argon2::default()
            .hash_password(code.as_bytes(), &salt)
            .map_err(|e| SecretError::Kdf(e.to_string()))?
            .to_string();
        Ok(Self { phc: hash })
    }

    /// 입력 코드가 등록된 검증자와 일치하는지 상수시간 검증.
    pub fn verify(&self, code: &str) -> bool {
        use argon2::password_hash::{PasswordHash, PasswordVerifier};
        match PasswordHash::new(&self.phc) {
            Ok(parsed) => Argon2::default()
                .verify_password(code.as_bytes(), &parsed)
                .is_ok(),
            Err(_) => false,
        }
    }
}
