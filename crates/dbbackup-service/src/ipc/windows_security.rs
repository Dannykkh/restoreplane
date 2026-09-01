use std::ffi::c_void;
use std::mem::{size_of, MaybeUninit};
use std::os::windows::io::AsRawHandle;

use thiserror::Error;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, LocalFree, BOOL, ERROR_INSUFFICIENT_BUFFER, HANDLE, HLOCAL,
};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetKernelObjectSecurity, GetTokenInformation, RevertToSelf, TokenElevation, TokenGroups,
    TokenSessionId, TokenUser, DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES, TOKEN_ELEVATION, TOKEN_GROUPS, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::System::Pipes::{GetNamedPipeClientSessionId, ImpersonateNamedPipeClient};
use windows::Win32::System::SystemServices::{SE_GROUP_ENABLED, SE_GROUP_LOGON_ID};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, OpenProcessToken, OpenThreadToken,
};

use super::{IpcError, TokenIdentity, WindowsSid};

const SYSTEM_SID: &str = "S-1-5-18";
const BUILTIN_ADMINISTRATORS_SID: &str = "S-1-5-32-544";
const MAX_APPROVED_ADMINISTRATORS: usize = 32;

#[derive(Clone, Debug)]
pub struct PipeSecurityPolicy {
    service_sid: WindowsSid,
    owner_sid: WindowsSid,
    approved_administrator_sids: Vec<WindowsSid>,
}

impl PipeSecurityPolicy {
    pub fn new(
        service_sid: WindowsSid,
        owner_sid: WindowsSid,
        mut approved_administrator_sids: Vec<WindowsSid>,
    ) -> Result<Self, PipeSecurityError> {
        if service_sid == owner_sid
            || approved_administrator_sids.len() > MAX_APPROVED_ADMINISTRATORS
            || approved_administrator_sids
                .iter()
                .any(|sid| sid == &service_sid || sid == &owner_sid)
        {
            return Err(PipeSecurityError::PolicyInvalid);
        }
        approved_administrator_sids.sort();
        approved_administrator_sids.dedup();
        Ok(Self {
            service_sid,
            owner_sid,
            approved_administrator_sids,
        })
    }

    fn sddl(&self) -> String {
        let mut sddl = String::from("D:P(A;;GA;;;SY)");
        if self.service_sid.as_str() != SYSTEM_SID {
            sddl.push_str(&format!("(A;;GA;;;{})", self.service_sid.as_str()));
        }
        sddl.push_str(&format!("(A;;GRGW;;;{})", self.owner_sid.as_str()));
        for sid in &self.approved_administrator_sids {
            sddl.push_str(&format!("(A;;GRGW;;;{})", sid.as_str()));
        }
        sddl
    }
}

#[derive(Debug, Error)]
pub enum PipeSecurityError {
    #[error("IPC_PIPE_POLICY_INVALID")]
    PolicyInvalid,
    #[error("IPC_PIPE_CREATE_FAILED")]
    CreateFailed(#[source] std::io::Error),
    #[error("IPC_PIPE_SECURITY_FAILED")]
    SecurityFailed,
    #[error("IPC_PIPE_IDENTITY_FAILED")]
    IdentityFailed,
}

impl PipeSecurityError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PolicyInvalid => "IPC_PIPE_POLICY_INVALID",
            Self::CreateFailed(_) => "IPC_PIPE_CREATE_FAILED",
            Self::SecurityFailed => "IPC_PIPE_SECURITY_FAILED",
            Self::IdentityFailed => "IPC_PIPE_IDENTITY_FAILED",
        }
    }
}

pub fn secure_named_pipe_server(
    pipe_name: &str,
    policy: &PipeSecurityPolicy,
    first_instance: bool,
) -> Result<NamedPipeServer, PipeSecurityError> {
    if !pipe_name.starts_with(r"\\.\pipe\DbBackup.")
        || pipe_name.len() > 256
        || pipe_name.chars().any(|character| character == '\0')
    {
        return Err(PipeSecurityError::PolicyInvalid);
    }

    let mut security = OwnedSecurityAttributes::from_sddl(&policy.sddl())?;
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first_instance)
        .reject_remote_clients(true);
    unsafe {
        options.create_with_security_attributes_raw(
            pipe_name,
            (&mut security.attributes as *mut SECURITY_ATTRIBUTES).cast::<c_void>(),
        )
    }
    .map_err(PipeSecurityError::CreateFailed)
}

pub fn current_process_identity() -> Result<TokenIdentity, PipeSecurityError> {
    let mut handle = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle) }
        .map_err(|_| PipeSecurityError::IdentityFailed)?;
    let token = OwnedHandle(handle);
    token_identity(&token, None)
}

pub fn identity_from_connected_pipe(
    server: &NamedPipeServer,
) -> Result<TokenIdentity, PipeSecurityError> {
    let pipe = HANDLE(server.as_raw_handle());
    let mut pipe_session_id = 0_u32;
    unsafe { GetNamedPipeClientSessionId(pipe, &mut pipe_session_id) }
        .map_err(|_| PipeSecurityError::IdentityFailed)?;
    unsafe { ImpersonateNamedPipeClient(pipe) }.map_err(|_| PipeSecurityError::IdentityFailed)?;
    let mut impersonation = ImpersonationGuard { active: true };

    let identity_result = (|| {
        let mut handle = HANDLE::default();
        unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, BOOL(1), &mut handle) }
            .map_err(|_| PipeSecurityError::IdentityFailed)?;
        let token = OwnedHandle(handle);
        token_identity(&token, Some(pipe_session_id))
    })();
    impersonation.revert()?;
    identity_result
}

pub fn audit_pipe_dacl_sddl(server: &NamedPipeServer) -> Result<String, PipeSecurityError> {
    let handle = HANDLE(server.as_raw_handle());
    let mut required = 0_u32;
    let first = unsafe {
        GetKernelObjectSecurity(
            handle,
            DACL_SECURITY_INFORMATION.0,
            PSECURITY_DESCRIPTOR::default(),
            0,
            &mut required,
        )
    };
    if first.is_ok() || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(PipeSecurityError::SecurityFailed);
    }

    let mut descriptor = AlignedBuffer::new(required as usize);
    unsafe {
        GetKernelObjectSecurity(
            handle,
            DACL_SECURITY_INFORMATION.0,
            PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr()),
            descriptor.byte_len(),
            &mut required,
        )
    }
    .map_err(|_| PipeSecurityError::SecurityFailed)?;

    let mut output = PWSTR::null();
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            PSECURITY_DESCRIPTOR(descriptor.as_mut_ptr()),
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut output,
            None,
        )
    }
    .map_err(|_| PipeSecurityError::SecurityFailed)?;
    let result = unsafe { output.to_string() }.map_err(|_| PipeSecurityError::SecurityFailed);
    let _ = unsafe { LocalFree(HLOCAL(output.0.cast())) };
    result
}

fn token_identity(
    token: &OwnedHandle,
    required_session_id: Option<u32>,
) -> Result<TokenIdentity, PipeSecurityError> {
    let user_buffer = token_information(token.0, TokenUser)?;
    let user = unsafe { &*(user_buffer.as_ptr().cast::<TOKEN_USER>()) };
    let user_sid = sid_to_windows_sid(user.User.Sid)?;

    let groups_buffer = token_information(token.0, TokenGroups)?;
    let groups = unsafe { &*(groups_buffer.as_ptr().cast::<TOKEN_GROUPS>()) };
    let group_slice =
        unsafe { std::slice::from_raw_parts(groups.Groups.as_ptr(), groups.GroupCount as usize) };
    let logon_sid = group_slice
        .iter()
        .find(|group| group.Attributes & (SE_GROUP_LOGON_ID as u32) == SE_GROUP_LOGON_ID as u32)
        .ok_or(PipeSecurityError::IdentityFailed)
        .and_then(|group| sid_to_windows_sid(group.Sid))?;
    let enabled_administrator = group_slice.iter().any(is_enabled_administrator);

    let session_id = token_value::<u32>(token.0, TokenSessionId)?;
    if required_session_id.is_some_and(|required| required != session_id) {
        return Err(PipeSecurityError::IdentityFailed);
    }
    let elevation = token_value::<TOKEN_ELEVATION>(token.0, TokenElevation)?;

    Ok(TokenIdentity::new(
        user_sid,
        logon_sid,
        session_id,
        elevation.TokenIsElevated != 0 && enabled_administrator,
        true,
    ))
}

fn is_enabled_administrator(group: &SID_AND_ATTRIBUTES) -> bool {
    group.Attributes & (SE_GROUP_ENABLED as u32) != 0
        && sid_to_windows_sid(group.Sid).is_ok_and(|sid| sid.as_str() == BUILTIN_ADMINISTRATORS_SID)
}

fn token_information(
    token: HANDLE,
    information_class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<AlignedBuffer, PipeSecurityError> {
    let mut required = 0_u32;
    let first = unsafe { GetTokenInformation(token, information_class, None, 0, &mut required) };
    if first.is_ok() || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(PipeSecurityError::IdentityFailed);
    }
    let mut buffer = AlignedBuffer::new(required as usize);
    unsafe {
        GetTokenInformation(
            token,
            information_class,
            Some(buffer.as_mut_ptr()),
            buffer.byte_len(),
            &mut required,
        )
    }
    .map_err(|_| PipeSecurityError::IdentityFailed)?;
    Ok(buffer)
}

fn token_value<T: Copy>(
    token: HANDLE,
    information_class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<T, PipeSecurityError> {
    let mut value = MaybeUninit::<T>::zeroed();
    let mut required = 0_u32;
    unsafe {
        GetTokenInformation(
            token,
            information_class,
            Some(value.as_mut_ptr().cast()),
            size_of::<T>() as u32,
            &mut required,
        )
    }
    .map_err(|_| PipeSecurityError::IdentityFailed)?;
    if required as usize != size_of::<T>() {
        return Err(PipeSecurityError::IdentityFailed);
    }
    Ok(unsafe { value.assume_init() })
}

fn sid_to_windows_sid(sid: PSID) -> Result<WindowsSid, PipeSecurityError> {
    if sid.is_invalid() {
        return Err(PipeSecurityError::IdentityFailed);
    }
    let mut output = PWSTR::null();
    unsafe { ConvertSidToStringSidW(sid, &mut output) }
        .map_err(|_| PipeSecurityError::IdentityFailed)?;
    let value = unsafe { output.to_string() }.map_err(|_| PipeSecurityError::IdentityFailed);
    let _ = unsafe { LocalFree(HLOCAL(output.0.cast())) };
    WindowsSid::parse(&value?).map_err(map_identity_error)
}

fn map_identity_error(_error: IpcError) -> PipeSecurityError {
    PipeSecurityError::IdentityFailed
}

struct OwnedSecurityAttributes {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl OwnedSecurityAttributes {
    fn from_sddl(sddl: &str) -> Result<Self, PipeSecurityError> {
        let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(wide.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .map_err(|_| PipeSecurityError::SecurityFailed)?;
        Ok(Self {
            descriptor,
            attributes: SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor.0,
                bInheritHandle: BOOL(0),
            },
        })
    }
}

impl Drop for OwnedSecurityAttributes {
    fn drop(&mut self) {
        let _ = unsafe { LocalFree(HLOCAL(self.descriptor.0)) };
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct ImpersonationGuard {
    active: bool,
}

impl ImpersonationGuard {
    fn revert(&mut self) -> Result<(), PipeSecurityError> {
        unsafe { RevertToSelf() }.map_err(|_| PipeSecurityError::IdentityFailed)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for ImpersonationGuard {
    fn drop(&mut self) {
        if self.active && unsafe { RevertToSelf() }.is_err() {
            std::process::abort();
        }
    }
}

struct AlignedBuffer {
    words: Vec<usize>,
    requested_bytes: usize,
}

impl AlignedBuffer {
    fn new(requested_bytes: usize) -> Self {
        let words = requested_bytes.div_ceil(size_of::<usize>());
        Self {
            words: vec![0; words],
            requested_bytes,
        }
    }

    fn as_ptr(&self) -> *const c_void {
        self.words.as_ptr().cast()
    }

    fn as_mut_ptr(&mut self) -> *mut c_void {
        self.words.as_mut_ptr().cast()
    }

    fn byte_len(&self) -> u32 {
        self.requested_bytes as u32
    }
}
