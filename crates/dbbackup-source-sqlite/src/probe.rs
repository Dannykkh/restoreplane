use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::SqliteProviderError;

const SQLITE_HEADER: &[u8; 16] = b"SQLite format 3\0";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SqliteFileProbe {
    canonical_path: PathBuf,
    object_id: String,
    length_bytes: u64,
    modified_at_ms: u64,
    readonly: bool,
    sqlite_version: String,
    journal_mode: String,
    page_size: u64,
    page_count: u64,
    schema_object_count: u64,
}

impl SqliteFileProbe {
    pub fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub fn object_id(&self) -> &str {
        &self.object_id
    }

    pub const fn length_bytes(&self) -> u64 {
        self.length_bytes
    }

    pub const fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn sqlite_version(&self) -> &str {
        &self.sqlite_version
    }

    pub fn journal_mode(&self) -> &str {
        &self.journal_mode
    }

    pub const fn page_size(&self) -> u64 {
        self.page_size
    }

    pub const fn page_count(&self) -> u64 {
        self.page_count
    }

    pub const fn schema_object_count(&self) -> u64 {
        self.schema_object_count
    }

    pub fn same_file(&self, other: &Self) -> bool {
        self.object_id == other.object_id && self.canonical_path == other.canonical_path
    }
}

pub fn probe_sqlite_file(path: &Path) -> Result<SqliteFileProbe, SqliteProviderError> {
    ensure_no_links_in_existing_path(path)?;
    let canonical_path = std::fs::canonicalize(path)?;
    let mut file = File::open(&canonical_path)?;
    let mut header = [0_u8; 16];
    file.read_exact(&mut header)?;
    if &header != SQLITE_HEADER {
        return Err(SqliteProviderError::HeaderInvalid(canonical_path));
    }
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(SqliteProviderError::PathInvalid(path.display().to_string()));
    }
    let modified_at_ms = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0);
    let object_id = file_object_id(&file, &metadata)?;
    let connection = Connection::open_with_flags(
        &canonical_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.busy_timeout(Duration::from_millis(250))?;
    let journal_mode = connection
        .pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))?
        .to_ascii_lowercase();
    let page_size = connection.pragma_query_value(None, "page_size", |row| row.get::<_, u64>(0))?;
    let page_count =
        connection.pragma_query_value(None, "page_count", |row| row.get::<_, u64>(0))?;
    let schema_object_count = connection.query_row(
        "SELECT COUNT(*)
         FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get::<_, u64>(0),
    )?;
    Ok(SqliteFileProbe {
        canonical_path,
        object_id,
        length_bytes: metadata.len(),
        modified_at_ms,
        readonly: metadata.permissions().readonly(),
        sqlite_version: rusqlite::version().to_owned(),
        journal_mode,
        page_size,
        page_count,
        schema_object_count,
    })
}

pub(crate) fn ensure_safe_parent(path: &Path) -> Result<PathBuf, SqliteProviderError> {
    let parent = path.parent().ok_or_else(|| {
        SqliteProviderError::PathInvalid(format!("path has no parent: {}", path.display()))
    })?;
    ensure_no_links_in_existing_path(parent)?;
    Ok(std::fs::canonicalize(parent)?)
}

fn ensure_no_links_in_existing_path(path: &Path) -> Result<(), SqliteProviderError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        current.push(component.as_os_str());
        if !current.exists() {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(SqliteProviderError::ReparsePoint(current));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT_VALUE: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT_VALUE != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
fn file_object_id(
    file: &File,
    _metadata: &std::fs::Metadata,
) -> Result<String, SqliteProviderError> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), information.as_mut_ptr())
            .map_err(|error| std::io::Error::from_raw_os_error(error.code().0))?;
        let information = information.assume_init();
        Ok(format!(
            "{:08x}:{:08x}{:08x}",
            information.dwVolumeSerialNumber, information.nFileIndexHigh, information.nFileIndexLow
        ))
    }
}

#[cfg(unix)]
fn file_object_id(
    _file: &File,
    metadata: &std::fs::Metadata,
) -> Result<String, SqliteProviderError> {
    use std::os::unix::fs::MetadataExt;
    Ok(format!("{:016x}:{:016x}", metadata.dev(), metadata.ino()))
}

#[cfg(not(any(unix, windows)))]
fn file_object_id(
    _file: &File,
    metadata: &std::fs::Metadata,
) -> Result<String, SqliteProviderError> {
    Ok(format!("fallback:{:016x}", metadata.len()))
}

pub(crate) fn sync_file(path: &Path) -> Result<(), SqliteProviderError> {
    File::options()
        .read(true)
        .write(true)
        .open(path)?
        .sync_all()?;
    Ok(())
}

pub(crate) fn sync_directory(path: &Path) -> Result<(), SqliteProviderError> {
    #[cfg(windows)]
    {
        // Windows has no supported directory fsync equivalent for this path.
        // Create-new uses MoveFileExW(MOVEFILE_WRITE_THROUGH), and overwrite
        // uses ReplaceFileW after flushing the staged file.
        let _ = path;
    }
    #[cfg(not(windows))]
    {
        File::open(path)?.sync_all()?;
    }
    Ok(())
}
