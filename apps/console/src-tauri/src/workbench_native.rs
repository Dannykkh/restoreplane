//! Native identity and private-root boundary. No caller-supplied SID/path is trusted.
use restoreplane_workbench::store::UserRootPolicy;
use restoreplane_workbench::{IntegrityLevel, NativePrincipal, WorkbenchError};
use std::path::{Path, PathBuf};

pub struct NativeEnvironment {
    pub principal: NativePrincipal,
    pub root: PathBuf,
    pub policy: PrivateUserRoot,
}
pub struct PrivateUserRoot {
    root: PathBuf,
    sid: String,
}

#[cfg(windows)]
pub fn detect() -> Result<NativeEnvironment, WorkbenchError> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    let identity = restoreplane_service::ipc::current_process_identity()
        .map_err(|_| WorkbenchError::Forbidden)?;
    let rid = integrity_rid()?;
    if rid < 0x2000 {
        return Err(WorkbenchError::Forbidden);
    }
    let integrity = if identity.elevated_admin() || rid >= 0x3000 {
        IntegrityLevel::Elevated
    } else {
        IntegrityLevel::Standard
    };
    let principal = NativePrincipal::new(
        blake3::hash(identity.user_sid().as_str().as_bytes())
            .to_hex()
            .to_string(),
        identity.session_id(),
        integrity,
    )?;
    // This check precedes KnownFolder resolution, creation, SQLite and DPAPI.
    principal.ensure_standard()?;
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None) }
        .map_err(|_| WorkbenchError::Forbidden)?;
    let decoded = unsafe { raw.to_string() }.map_err(|_| WorkbenchError::Forbidden);
    unsafe { CoTaskMemFree(Some(raw.0.cast())) };
    let local = PathBuf::from(decoded?);
    for parent in local.ancestors() {
        reject_reparse(parent)?;
    }
    let root = local.join("RestorePlane-Workbench");
    Ok(NativeEnvironment {
        principal,
        root: root.clone(),
        policy: PrivateUserRoot {
            root,
            sid: identity.user_sid().as_str().to_owned(),
        },
    })
}

#[cfg(not(windows))]
pub fn detect() -> Result<NativeEnvironment, WorkbenchError> {
    Err(WorkbenchError::Unsupported)
}

impl UserRootPolicy for PrivateUserRoot {
    fn ensure_private_root(&self, path: &Path) -> Result<(), WorkbenchError> {
        if !path.is_absolute()
            || !path.starts_with(&self.root)
            || path
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir))
        {
            return Err(WorkbenchError::Forbidden);
        }
        #[cfg(windows)]
        {
            create_or_verify(&self.root, &self.sid)?;
            let relative = path
                .strip_prefix(&self.root)
                .map_err(|_| WorkbenchError::Forbidden)?;
            let mut current = self.root.clone();
            for component in relative.components() {
                if !matches!(component, std::path::Component::Normal(_)) {
                    return Err(WorkbenchError::Forbidden);
                }
                current.push(component);
                create_or_verify(&current, &self.sid)?;
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = path;
            Err(WorkbenchError::Unsupported)
        }
    }
}

#[cfg(windows)]
fn reject_reparse(path: &Path) -> Result<(), WorkbenchError> {
    use std::os::windows::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path).map_err(|_| WorkbenchError::Forbidden)?;
    if metadata.file_attributes() & 0x400 != 0 || !metadata.is_dir() {
        return Err(WorkbenchError::Forbidden);
    }
    Ok(())
}

#[cfg(windows)]
fn integrity_rid() -> Result<u32, WorkbenchError> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|_| WorkbenchError::Forbidden)?;
    let result = (|| {
        let mut required = 0;
        let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut required) };
        if required == 0 || required > 65536 {
            return Err(WorkbenchError::Forbidden);
        }
        let mut buffer = vec![0usize; (required as usize).div_ceil(std::mem::size_of::<usize>())];
        unsafe {
            GetTokenInformation(
                token,
                TokenIntegrityLevel,
                Some(buffer.as_mut_ptr().cast()),
                required,
                &mut required,
            )
        }
        .map_err(|_| WorkbenchError::Forbidden)?;
        let label = unsafe { &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
        let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) };
        if count == 0 {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(unsafe { *GetSidSubAuthority(label.Label.Sid, u32::from(count) - 1) })
    })();
    let _ = unsafe { CloseHandle(token) };
    result
}

#[cfg(windows)]
fn create_or_verify(path: &Path, sid: &str) -> Result<(), WorkbenchError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSecurityDescriptorToStringSecurityDescriptorW,
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{
        GetFileSecurityW, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
    };
    use windows::Win32::Storage::FileSystem::CreateDirectoryW;
    let sddl = format!("O:{sid}D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{sid})");
    let wide_sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut expected = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide_sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut expected,
            None,
        )
    }
    .map_err(|_| WorkbenchError::Forbidden)?;
    let result = (|| {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let attrs = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: expected.0,
                    bInheritHandle: false.into(),
                };
                unsafe { CreateDirectoryW(PCWSTR(wide_path.as_ptr()), Some(&attrs)) }
                    .map_err(|_| WorkbenchError::Forbidden)?;
            }
            Err(_) => return Err(WorkbenchError::Forbidden),
            Ok(_) => {}
        }
        reject_reparse(path)?;
        let information = OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION;
        let mut size = 0;
        let _ = unsafe {
            GetFileSecurityW(
                PCWSTR(wide_path.as_ptr()),
                information.0,
                PSECURITY_DESCRIPTOR::default(),
                0,
                &mut size,
            )
        };
        if size == 0 || size > 65536 {
            return Err(WorkbenchError::Forbidden);
        }
        let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        unsafe {
            GetFileSecurityW(
                PCWSTR(wide_path.as_ptr()),
                information.0,
                PSECURITY_DESCRIPTOR(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            )
        }
        .ok()
        .map_err(|_| WorkbenchError::Forbidden)?;
        let render = |descriptor| -> Result<String, WorkbenchError> {
            let mut text = PWSTR::null();
            unsafe {
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    SDDL_REVISION_1,
                    information,
                    &mut text,
                    None,
                )
            }
            .map_err(|_| WorkbenchError::Forbidden)?;
            let result = unsafe { text.to_string() }.map_err(|_| WorkbenchError::Forbidden);
            let _ = unsafe { LocalFree(HLOCAL(text.0.cast())) };
            result
        };
        if render(expected)? != render(PSECURITY_DESCRIPTOR(buffer.as_mut_ptr().cast()))? {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(())
    })();
    let _ = unsafe { LocalFree(HLOCAL(expected.0)) };
    result
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn current_process_integrity_enforces_native_boundary() {
        let identity = restoreplane_service::ipc::current_process_identity().unwrap();
        let rid = integrity_rid().unwrap();
        eprintln!(
            "native_integrity_rid={rid}; elevated_admin={}",
            identity.elevated_admin()
        );
        match detect() {
            Err(WorkbenchError::Elevated) => assert!(identity.elevated_admin() || rid >= 0x3000),
            Err(WorkbenchError::Forbidden) if rid < 0x2000 => {}
            Ok(environment) if (0x2000..0x3000).contains(&rid) && !identity.elevated_admin() => {
                environment.principal.ensure_standard().unwrap();
            }
            _ => panic!("native integrity and Workbench admission must agree"),
        }
    }

    #[test]
    fn private_root_helper_accepts_only_explicit_user_and_system_acl() {
        // This tests ACL construction directly; it does not bypass detect() in
        // the production initialization path or claim a medium-token GUI test.
        let identity = restoreplane_service::ipc::current_process_identity().unwrap();
        let root =
            std::env::temp_dir().join(format!("restoreplane-wb-acl-{}", uuid::Uuid::new_v4()));
        assert!(!root.exists());
        let policy = PrivateUserRoot {
            root: root.clone(),
            sid: identity.user_sid().as_str().to_owned(),
        };
        policy.ensure_private_root(&root).unwrap();
        let child = root.join("nested");
        policy.ensure_private_root(&child).unwrap();
        policy.ensure_private_root(&child).unwrap();
        let inherited = root.join("inherited");
        std::fs::create_dir(&inherited).unwrap();
        assert!(policy.ensure_private_root(&inherited).is_err());
        assert!(policy
            .ensure_private_root(&root.join("..").join("foreign"))
            .is_err());
        std::fs::remove_dir(&inherited).unwrap();
        std::fs::remove_dir(&child).unwrap();
        std::fs::remove_dir(&root).unwrap();
    }
}
