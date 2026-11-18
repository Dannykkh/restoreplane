//! Explicit CurrentUser DPAPI protection; machine-scoped backup APIs are unchanged.

use crate::SecretError;
use rand::RngCore;
use zeroize::Zeroizing;

const MAX_USER_SECRET_BYTES: usize = 256 * 1024;

/// A fresh purpose-separated caller key, held only in zeroizing memory.
pub fn ephemeral_user_key() -> Result<Zeroizing<[u8; 32]>, SecretError> {
    let mut key = Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng
        .try_fill_bytes(key.as_mut())
        .map_err(|_| SecretError::Io(std::io::Error::other("native entropy unavailable")))?;
    Ok(key)
}

fn entropy(purpose: &str) -> Result<Vec<u8>, SecretError> {
    if purpose.is_empty()
        || purpose.len() > 128
        || !purpose
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(SecretError::Dpapi("invalid current-user purpose".into()));
    }
    Ok(format!("restoreplane.current-user.v1\0{purpose}").into_bytes())
}

pub fn protect_current_user(plaintext: &[u8], purpose: &str) -> Result<Vec<u8>, SecretError> {
    let entropy = entropy(purpose)?;
    if plaintext.is_empty() || plaintext.len() > MAX_USER_SECRET_BYTES {
        return Err(SecretError::WrapFailed);
    }
    #[cfg(windows)]
    {
        windows_dpapi::encrypt_data(plaintext, windows_dpapi::Scope::User, Some(&entropy))
            .map_err(|_| SecretError::WrapFailed)
    }
    #[cfg(not(windows))]
    {
        let _ = (plaintext, entropy);
        Err(SecretError::DpapiUnavailable)
    }
}

pub fn unprotect_current_user(
    blob: &[u8],
    purpose: &str,
) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let entropy = entropy(purpose)?;
    if blob.is_empty() || blob.len() > MAX_USER_SECRET_BYTES + 4096 {
        return Err(SecretError::UnwrapFailed);
    }
    #[cfg(windows)]
    {
        windows_dpapi::decrypt_data(blob, windows_dpapi::Scope::User, Some(&entropy))
            .map(Zeroizing::new)
            .map_err(|_| SecretError::UnwrapFailed)
    }
    #[cfg(not(windows))]
    {
        let _ = (blob, entropy);
        Err(SecretError::DpapiUnavailable)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn current_user_roundtrip_is_purpose_bound_and_not_plaintext() {
        let plaintext = b"workbench-secret-canary-9281";
        let blob = protect_current_user(plaintext, "workbench-credential").unwrap();
        assert!(!blob
            .windows(plaintext.len())
            .any(|window| window == plaintext));
        assert_eq!(
            unprotect_current_user(&blob, "workbench-credential")
                .unwrap()
                .as_slice(),
            plaintext
        );
        assert!(unprotect_current_user(&blob, "saved-query").is_err());
        let mut corrupted = blob;
        let last = corrupted.len() - 1;
        corrupted[last] ^= 0x40;
        assert!(unprotect_current_user(&corrupted, "workbench-credential").is_err());
    }
}
