use std::path::Path;

use crate::ipc::WindowsSid;

use super::SecretStoreError;

#[derive(Clone, Debug)]
pub struct AclPolicy {
    service_sid: WindowsSid,
    owner_sid: WindowsSid,
}

impl AclPolicy {
    pub fn new(service_sid: WindowsSid, owner_sid: WindowsSid) -> Result<Self, SecretStoreError> {
        if service_sid == owner_sid {
            return Err(SecretStoreError::InputInvalid);
        }
        Ok(Self {
            service_sid,
            owner_sid,
        })
    }

    pub fn apply_directory(&self, path: &Path) -> Result<(), SecretStoreError> {
        let sddl = format!(
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{})(A;OICI;FA;;;{})",
            self.service_sid.as_str(),
            self.owner_sid.as_str()
        );
        apply_sddl(path, &sddl)
    }

    pub fn apply_file(&self, path: &Path) -> Result<(), SecretStoreError> {
        let sddl = format!(
            "D:P(A;;FA;;;SY)(A;;FA;;;{})(A;;FA;;;{})",
            self.service_sid.as_str(),
            self.owner_sid.as_str()
        );
        apply_sddl(path, &sddl)
    }
}

#[cfg(windows)]
fn apply_sddl(path: &Path, sddl: &str) -> Result<(), SecretStoreError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        SetFileSecurityW, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR,
    };

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|_| SecretStoreError::AclFailed)?;
        let result = SetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
        .ok()
        .map_err(|_| SecretStoreError::AclFailed);
        let _ = LocalFree(HLOCAL(descriptor.0));
        result
    }
}

#[cfg(not(windows))]
fn apply_sddl(_path: &Path, _sddl: &str) -> Result<(), SecretStoreError> {
    Err(SecretStoreError::PlatformUnsupported)
}

#[cfg(windows)]
pub fn audit_acl_sddl(path: &Path) -> Result<String, SecretStoreError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{GetLastError, LocalFree, ERROR_INSUFFICIENT_BUFFER, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetFileSecurityW, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    };

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut required = 0_u32;
    unsafe {
        let first = GetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION.0,
            PSECURITY_DESCRIPTOR::default(),
            0,
            &mut required,
        );
        if first.as_bool() || GetLastError() != ERROR_INSUFFICIENT_BUFFER || required == 0 {
            return Err(SecretStoreError::AclFailed);
        }
        let mut descriptor = vec![0_u8; required as usize];
        GetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION.0,
            PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr().cast()),
            required,
            &mut required,
        )
        .ok()
        .map_err(|_| SecretStoreError::AclFailed)?;

        let mut output = PWSTR::null();
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr().cast()),
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut output,
            None,
        )
        .map_err(|_| SecretStoreError::AclFailed)?;
        let value = output.to_string().map_err(|_| SecretStoreError::AclFailed);
        let _ = LocalFree(HLOCAL(output.0.cast()));
        value
    }
}

#[cfg(not(windows))]
pub fn audit_acl_sddl(_path: &Path) -> Result<String, SecretStoreError> {
    Err(SecretStoreError::PlatformUnsupported)
}
