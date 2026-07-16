// crates/dbbackup-secrets/src/secret/dpapi.rs
//! DPAPI Machine-scope wrap + keyfile-entropy 유도(C1, R5).
//!
//! 핵심: entropy를 바이너리에 하드코딩하지 않는다. init 시 머신마다 비커밋 로컬 keyfile을
//! 만들고 그 바이트에서 HKDF로 DPAPI optional entropy를 유도한다. keyfile을 못 읽는
//! 프로세스는 unprotect가 불가능하다(C1 완화).
//!
//! [확인 필요] 해소: windows-dpapi 0.2.0의 실제 API는 섹션 초안과 다르다.
//!   섹션: `windows_dpapi::encrypt(dek, Some(entropy), Scope::Machine)`
//!   실제: `windows_dpapi::encrypt_data(data, Scope::Machine, Some(entropy))` / `decrypt_data(...)`
//! 인자 순서와 함수명을 실제 크레이트에 맞춰 적용했다(보안 속성은 동일: Machine-scope + entropy).

use hkdf::Hkdf;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::path::Path;
use zeroize::{Zeroize, Zeroizing};

use crate::secret::error::SecretError;

const KEYFILE_INFO: &[u8] = b"dbbackup-dpapi-entropy-v1";

/// 비커밋 로컬 keyfile(예: %PROGRAMDATA%\dbbackup\dpapi.key). 없으면 init에서 생성한다.
/// ACL 제한은 S11 배포 단계에서 부여한다(여기서는 파일 생성만).
pub(crate) fn load_or_create_keyfile(path: &Path) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    if !path.exists() {
        let mut kf = [0u8; 48]; // 384bit
        OsRng.fill_bytes(&mut kf);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(path, kf.as_slice())?;
        kf.zeroize();
    }
    Ok(Zeroizing::new(std::fs::read(path)?))
}

/// keyfile bytes → 32B DPAPI entropy(HKDF-SHA256).
pub(crate) fn derive_dpapi_entropy(keyfile: &[u8]) -> Zeroizing<[u8; 32]> {
    let hk = Hkdf::<Sha256>::new(None, keyfile);
    let mut out = Zeroizing::new([0u8; 32]);
    hk.expand(KEYFILE_INFO, out.as_mut_slice())
        .expect("32 <= 255*32 okm limit");
    out
}

/// keyfile 지문(hex 앞 8글자) — keys status 표기용(entropy 자체가 아니다).
pub(crate) fn keyfile_fingerprint(keyfile: &[u8]) -> String {
    let d = Sha256::digest(keyfile);
    d.iter().take(4).map(|b| format!("{:02x}", b)).collect()
}

#[cfg(windows)]
pub(crate) fn dpapi_wrap(
    dek: &crate::secret::dek::Dek,
    entropy: &[u8; 32],
) -> Result<Vec<u8>, SecretError> {
    windows_dpapi::encrypt_data(
        dek.expose().as_slice(),
        windows_dpapi::Scope::Machine,
        Some(entropy.as_slice()),
    )
    .map_err(|e| SecretError::Dpapi(e.to_string()))
}

#[cfg(windows)]
pub(crate) fn dpapi_unwrap(
    blob: &[u8],
    entropy: &[u8; 32],
) -> Result<crate::secret::dek::Dek, SecretError> {
    let pt = Zeroizing::new(
        windows_dpapi::decrypt_data(
            blob,
            windows_dpapi::Scope::Machine,
            Some(entropy.as_slice()),
        )
        .map_err(|e| SecretError::Dpapi(e.to_string()))?,
    );
    let arr: [u8; 32] = pt
        .as_slice()
        .try_into()
        .map_err(|_| SecretError::BadDekLen)?;
    Ok(crate::secret::dek::Dek::from_bytes(arr))
}
