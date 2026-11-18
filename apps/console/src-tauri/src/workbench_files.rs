//! Windows native file dialogs. Caller paths and file bodies are never accepted.
use restoreplane_domain::workbench_movement::MovementFormat;
use restoreplane_workbench::WorkbenchError;
use std::path::PathBuf;

/// The caller holds its one-dialog permit and invoking native window until this
/// returns. Every dialog uses a fresh STA; it never changes OS trust or the cwd.
pub fn pick(
    owner: isize,
    destination: bool,
    format: MovementFormat,
) -> Result<Option<PathBuf>, WorkbenchError> {
    if owner == 0 {
        return Err(WorkbenchError::Forbidden);
    }
    std::thread::Builder::new()
        .name("restoreplane-file-dialog".into())
        .spawn(move || pick_sta(owner, destination, format))
        .map_err(|_| WorkbenchError::Io)?
        .join()
        .map_err(|_| WorkbenchError::Io)?
}
#[cfg(windows)]
fn pick_sta(
    owner: isize,
    destination: bool,
    format: MovementFormat,
) -> Result<Option<PathBuf>, WorkbenchError> {
    use windows::core::{w, Interface};
    use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE,
    };
    use windows::Win32::UI::Shell::{
        FileOpenDialog, FileSaveDialog, IFileDialog, IFileOpenDialog, IFileSaveDialog,
        FOS_DONTADDTORECENT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR,
        FOS_NODEREFERENCELINKS, FOS_NOTESTFILECREATE, FOS_PATHMUSTEXIST, SIGDN_FILESYSPATH,
    };
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() }
        }
    }
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE)
            .ok()
            .map_err(|_| WorkbenchError::Unsupported)?;
        let _apartment = Apartment;
        let dialog: IFileDialog = if destination {
            let selected: IFileSaveDialog =
                CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)
                    .map_err(|_| WorkbenchError::Io)?;
            selected.cast().map_err(|_| WorkbenchError::Io)?
        } else {
            let selected: IFileOpenDialog =
                CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
                    .map_err(|_| WorkbenchError::Io)?;
            selected.cast().map_err(|_| WorkbenchError::Io)?
        };
        let mut flags = FOS_FORCEFILESYSTEM
            | FOS_PATHMUSTEXIST
            | FOS_DONTADDTORECENT
            | FOS_NOCHANGEDIR
            | FOS_NODEREFERENCELINKS;
        if destination {
            // Dialog selection itself must not create even a test file before
            // the admitted job's initial metadata is durable.
            flags |= FOS_NOTESTFILECREATE;
            dialog
                .SetTitle(w!("내보낼 파일 선택"))
                .map_err(|_| WorkbenchError::Io)?;
            dialog
                .SetDefaultExtension(match format {
                    MovementFormat::Csv => w!("csv"),
                    MovementFormat::Tsv => w!("tsv"),
                    MovementFormat::Json => w!("json"),
                })
                .map_err(|_| WorkbenchError::Io)?;
        } else {
            flags |= FOS_FILEMUSTEXIST;
            dialog
                .SetTitle(w!("가져올 파일 선택"))
                .map_err(|_| WorkbenchError::Io)?;
        }
        dialog.SetOptions(flags).map_err(|_| WorkbenchError::Io)?;
        if let Err(error) = dialog.Show(HWND(owner as *mut std::ffi::c_void)) {
            if error.code() == windows::core::HRESULT::from_win32(ERROR_CANCELLED.0) {
                return Ok(None);
            }
            return Err(WorkbenchError::Io);
        }
        let item = dialog.GetResult().map_err(|_| WorkbenchError::Io)?;
        let raw = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(|_| WorkbenchError::Io)?;
        let decoded = raw.to_string();
        CoTaskMemFree(Some(raw.0.cast()));
        let path = PathBuf::from(decoded.map_err(|_| WorkbenchError::Io)?);
        if !path.is_absolute() {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(Some(path))
    }
}
#[cfg(not(windows))]
fn pick_sta(_: isize, _: bool, _: MovementFormat) -> Result<Option<PathBuf>, WorkbenchError> {
    Err(WorkbenchError::Unsupported)
}
