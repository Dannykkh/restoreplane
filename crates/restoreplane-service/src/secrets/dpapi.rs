use zeroize::{Zeroize, Zeroizing};

use super::SecretStoreError;

#[cfg(windows)]
pub fn protect(plaintext: &[u8], entropy: &[u8]) -> Result<Vec<u8>, SecretStoreError> {
    use windows::core::PCWSTR;
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = input_blob(plaintext)?;
    let entropy = input_blob(entropy)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(
            &input,
            PCWSTR::null(),
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| SecretStoreError::CryptoFailed)?;
    }
    LocalBlob::new(output)?.copy()
}

#[cfg(windows)]
pub fn unprotect(
    ciphertext: &[u8],
    entropy: &[u8],
) -> Result<Zeroizing<Vec<u8>>, SecretStoreError> {
    use windows::Win32::Security::Cryptography::{
        CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    let input = input_blob(ciphertext)?;
    let entropy = input_blob(entropy)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(
            &input,
            None,
            Some(&entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
        .map_err(|_| SecretStoreError::CryptoFailed)?;
    }
    LocalBlob::new(output)?.copy_zeroizing()
}

#[cfg(windows)]
fn input_blob(
    bytes: &[u8],
) -> Result<windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB, SecretStoreError> {
    use windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB;

    Ok(CRYPT_INTEGER_BLOB {
        cbData: bytes
            .len()
            .try_into()
            .map_err(|_| SecretStoreError::InputInvalid)?,
        pbData: bytes.as_ptr().cast_mut(),
    })
}

#[cfg(windows)]
struct LocalBlob {
    blob: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
}

#[cfg(windows)]
impl LocalBlob {
    fn new(
        blob: windows::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB,
    ) -> Result<Self, SecretStoreError> {
        if blob.pbData.is_null() || blob.cbData == 0 {
            return Err(SecretStoreError::CryptoFailed);
        }
        Ok(Self { blob })
    }

    fn copy(&self) -> Result<Vec<u8>, SecretStoreError> {
        let bytes =
            unsafe { std::slice::from_raw_parts(self.blob.pbData, self.blob.cbData as usize) };
        Ok(bytes.to_vec())
    }

    fn copy_zeroizing(&self) -> Result<Zeroizing<Vec<u8>>, SecretStoreError> {
        self.copy().map(Zeroizing::new)
    }
}

#[cfg(windows)]
impl Drop for LocalBlob {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LocalFree, HLOCAL};

        if self.blob.pbData.is_null() {
            return;
        }
        unsafe {
            std::slice::from_raw_parts_mut(self.blob.pbData, self.blob.cbData as usize).zeroize();
            let _ = LocalFree(HLOCAL(self.blob.pbData.cast()));
        }
        self.blob.pbData = std::ptr::null_mut();
        self.blob.cbData = 0;
    }
}

#[cfg(not(windows))]
pub fn protect(_plaintext: &[u8], _entropy: &[u8]) -> Result<Vec<u8>, SecretStoreError> {
    Err(SecretStoreError::PlatformUnsupported)
}

#[cfg(not(windows))]
pub fn unprotect(
    _ciphertext: &[u8],
    _entropy: &[u8],
) -> Result<Zeroizing<Vec<u8>>, SecretStoreError> {
    Err(SecretStoreError::PlatformUnsupported)
}
