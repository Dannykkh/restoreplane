use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::probe::{ensure_safe_parent, sync_directory, sync_file};
use crate::{probe_sqlite_file, validate_database, SqliteProviderError};

#[derive(Clone, Debug)]
pub struct OnlineBackupOptions {
    pub pages_per_step: i32,
    pub pause_between_steps: Duration,
    pub busy_timeout: Duration,
    pub max_transient_retries: u32,
    pub min_free_space_bytes: u64,
    pub max_output_bytes: Option<u64>,
}

impl Default for OnlineBackupOptions {
    fn default() -> Self {
        Self {
            pages_per_step: 128,
            pause_between_steps: Duration::from_millis(10),
            busy_timeout: Duration::from_secs(2),
            max_transient_retries: 100,
            min_free_space_bytes: 16 * 1024 * 1024,
            max_output_bytes: None,
        }
    }
}

#[derive(Clone, Default)]
pub struct BackupCancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl BackupCancellationToken {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEvidence {
    journal_mode: String,
    source_object_id: String,
    bytes: u64,
    blake3: String,
    steps: u64,
    transient_lock_events: u64,
    elapsed_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BackupProgress {
    pub steps: u64,
    pub remaining_pages: i32,
    pub total_pages: i32,
}

impl BackupEvidence {
    pub fn journal_mode(&self) -> &str {
        &self.journal_mode
    }

    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn blake3(&self) -> &str {
        &self.blake3
    }

    pub const fn steps(&self) -> u64 {
        self.steps
    }

    pub const fn transient_lock_events(&self) -> u64 {
        self.transient_lock_events
    }
}

pub fn online_backup_to_path(
    source_path: &Path,
    output_path: &Path,
    options: &OnlineBackupOptions,
    cancellation: &BackupCancellationToken,
) -> Result<BackupEvidence, SqliteProviderError> {
    online_backup_to_path_observed(source_path, output_path, options, cancellation, |_| {})
}

pub fn online_backup_to_path_observed(
    source_path: &Path,
    output_path: &Path,
    options: &OnlineBackupOptions,
    cancellation: &BackupCancellationToken,
    mut observer: impl FnMut(BackupProgress),
) -> Result<BackupEvidence, SqliteProviderError> {
    if options.pages_per_step <= 0 || options.max_transient_retries > 1_000_000 {
        return Err(SqliteProviderError::PathInvalid(
            "invalid online backup policy".to_owned(),
        ));
    }
    if cancellation.is_cancelled() {
        return Err(SqliteProviderError::Cancelled);
    }
    if output_path.exists() {
        return Err(SqliteProviderError::OutputExists(output_path.to_path_buf()));
    }
    let source_before = probe_sqlite_file(source_path)?;
    let expected_bytes = source_before
        .page_count()
        .saturating_mul(source_before.page_size());
    if options
        .max_output_bytes
        .is_some_and(|maximum| expected_bytes > maximum)
    {
        return Err(SqliteProviderError::SpaceInsufficient);
    }
    let parent = ensure_safe_parent(output_path)?;
    let required = expected_bytes.saturating_add(options.min_free_space_bytes);
    if fs2::available_space(&parent)? < required {
        return Err(SqliteProviderError::SpaceInsufficient);
    }
    let stage = tempfile::Builder::new()
        .prefix(".dbbackup-sqlite-")
        .suffix(".partial")
        .tempfile_in(&parent)?
        .into_temp_path();
    let stage_path: &Path = stage.as_ref();
    let start = Instant::now();
    let source = Connection::open_with_flags(
        source_before.canonical_path(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    source.busy_timeout(options.busy_timeout)?;
    let mut destination = Connection::open_with_flags(
        stage_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    destination.busy_timeout(options.busy_timeout)?;
    destination.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;")?;
    let mut steps = 0_u64;
    let mut transient_lock_events = 0_u64;
    let mut consecutive_transient = 0_u32;
    {
        let backup = Backup::new(&source, &mut destination)?;
        loop {
            if cancellation.is_cancelled() {
                return Err(SqliteProviderError::Cancelled);
            }
            steps = steps.saturating_add(1);
            let step_result = backup.step(options.pages_per_step)?;
            let progress = backup.progress();
            observer(BackupProgress {
                steps,
                remaining_pages: progress.remaining,
                total_pages: progress.pagecount,
            });
            if cancellation.is_cancelled() {
                return Err(SqliteProviderError::Cancelled);
            }
            match step_result {
                StepResult::Done => break,
                StepResult::More => consecutive_transient = 0,
                StepResult::Busy | StepResult::Locked => {
                    transient_lock_events = transient_lock_events.saturating_add(1);
                    consecutive_transient = consecutive_transient.saturating_add(1);
                    if consecutive_transient > options.max_transient_retries {
                        return Err(SqliteProviderError::BusyTimeout);
                    }
                }
                _ => return Err(SqliteProviderError::Sqlite(rusqlite::Error::InvalidQuery)),
            }
            thread::sleep(options.pause_between_steps);
        }
    }
    drop(destination);
    validate_database(stage_path, None)?;
    sync_file(stage_path)?;
    let source_after = probe_sqlite_file(source_path)?;
    if !source_before.same_file(&source_after) {
        return Err(SqliteProviderError::SourceIdentityChanged);
    }
    let bytes = std::fs::metadata(stage_path)?.len();
    let blake3 = hash_file(stage_path)?;
    persist_new_file(stage, output_path)?;
    sync_directory(&parent)?;
    Ok(BackupEvidence {
        journal_mode: source_before.journal_mode().to_owned(),
        source_object_id: source_before.object_id().to_owned(),
        bytes,
        blake3,
        steps,
        transient_lock_events,
        elapsed_ms: start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    })
}

#[cfg(windows)]
fn persist_new_file(
    stage: tempfile::TempPath,
    output_path: &Path,
) -> Result<(), SqliteProviderError> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let stage_path = stage
        .keep()
        .map_err(|error| SqliteProviderError::Io(error.error))?;
    let stage_wide = stage_path
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect::<Vec<_>>();
    let output_wide = output_path
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        MoveFileExW(
            PCWSTR(stage_wide.as_ptr()),
            PCWSTR(output_wide.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if let Err(error) = result {
        let _ = std::fs::remove_file(&stage_path);
        return Err(SqliteProviderError::Io(std::io::Error::from_raw_os_error(
            error.code().0,
        )));
    }
    Ok(())
}

#[cfg(not(windows))]
fn persist_new_file(
    stage: tempfile::TempPath,
    output_path: &Path,
) -> Result<(), SqliteProviderError> {
    stage
        .persist_noclobber(output_path)
        .map_err(|error| SqliteProviderError::Io(error.error))?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, SqliteProviderError> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
