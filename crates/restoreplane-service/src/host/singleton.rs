use super::HostError;

const MUTEX_PREFIX: &str = "Global\\RestorePlane.Service.";

#[cfg(not(windows))]
use std::collections::HashSet;
#[cfg(not(windows))]
use std::sync::{Mutex, OnceLock};

#[cfg(not(windows))]
static OWNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

pub(crate) struct SingletonAuthority {
    #[cfg(windows)]
    handle: windows::Win32::Foundation::HANDLE,
    #[cfg(not(windows))]
    key: String,
}

pub struct MaintenanceAuthority {
    _guard: SingletonAuthority,
}

impl MaintenanceAuthority {
    pub fn acquire(instance_key: &str) -> Result<Self, HostError> {
        if instance_key.trim().is_empty() || instance_key.len() > 256 {
            return Err(HostError::InvalidConfig);
        }
        Ok(Self {
            _guard: SingletonAuthority::acquire(instance_key)?,
        })
    }
}

impl std::fmt::Debug for MaintenanceAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MaintenanceAuthority")
            .finish_non_exhaustive()
    }
}

impl SingletonAuthority {
    pub(crate) fn acquire(instance_key: &str) -> Result<Self, HostError> {
        let digest = blake3::hash(instance_key.as_bytes()).to_hex();
        let name = format!("{MUTEX_PREFIX}{}", &digest.as_str()[..32]);
        acquire_platform(name)
    }
}

#[cfg(windows)]
fn acquire_platform(name: String) -> Result<SingletonAuthority, HostError> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, BOOL, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;

    let wide_name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe { CreateMutexW(None, BOOL(1), PCWSTR(wide_name.as_ptr())) }
        .map_err(|_| HostError::OsApiFailed("CreateMutexW"))?;
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        let _ = unsafe { CloseHandle(handle) };
        return Err(HostError::SingletonAlreadyRunning);
    }
    Ok(SingletonAuthority { handle })
}

#[cfg(windows)]
impl Drop for SingletonAuthority {
    fn drop(&mut self) {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::ReleaseMutex;

        let _ = unsafe { ReleaseMutex(self.handle) };
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(not(windows))]
fn acquire_platform(name: String) -> Result<SingletonAuthority, HostError> {
    let mut owned = OWNED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map_err(|_| HostError::InternalState)?;
    if !owned.insert(name.clone()) {
        return Err(HostError::SingletonAlreadyRunning);
    }
    Ok(SingletonAuthority { key: name })
}

#[cfg(not(windows))]
impl Drop for SingletonAuthority {
    fn drop(&mut self) {
        if let Ok(mut owned) = OWNED.get_or_init(|| Mutex::new(HashSet::new())).lock() {
            owned.remove(&self.key);
        }
    }
}
