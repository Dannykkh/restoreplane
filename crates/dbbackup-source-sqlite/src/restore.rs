use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::probe::{ensure_safe_parent, sync_directory, sync_file};
use crate::{
    online_backup_to_path_observed, probe_sqlite_file, validate_database_observed,
    BackupCancellationToken, BackupProgress, OnlineBackupOptions, ScalarOracle, SqliteFileProbe,
    SqliteProviderError, ValidationEvidence,
};

pub trait AtomicReplacer {
    fn replace(&self, target: &Path, replacement: &Path) -> Result<(), SqliteProviderError>;
}

pub struct SystemAtomicReplacer;

impl AtomicReplacer for SystemAtomicReplacer {
    fn replace(&self, target: &Path, replacement: &Path) -> Result<(), SqliteProviderError> {
        system_atomic_replace(target, replacement)
    }
}

pub fn restore_create_new(
    artifact: &Path,
    target: &Path,
    oracle: Option<&ScalarOracle>,
) -> Result<ValidationEvidence, SqliteProviderError> {
    let cancellation = BackupCancellationToken::default();
    restore_create_new_observed(
        artifact,
        target,
        oracle,
        &cancellation,
        |_| {},
        Arc::new(|| false),
    )
}

pub fn restore_create_new_observed(
    artifact: &Path,
    target: &Path,
    oracle: Option<&ScalarOracle>,
    cancellation: &BackupCancellationToken,
    observer: impl FnMut(BackupProgress),
    validation_control: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<ValidationEvidence, SqliteProviderError> {
    if target.exists() {
        return Err(SqliteProviderError::CreateNewTargetExists(
            target.to_path_buf(),
        ));
    }
    online_backup_to_path_observed(
        artifact,
        target,
        &OnlineBackupOptions::default(),
        cancellation,
        observer,
    )?;
    validate_database_observed(target, oracle, move || validation_control())
}

pub fn restore_overwrite(
    artifact: &Path,
    target: &Path,
    bound_target: &SqliteFileProbe,
    oracle: Option<&ScalarOracle>,
) -> Result<ValidationEvidence, SqliteProviderError> {
    let cancellation = BackupCancellationToken::default();
    restore_overwrite_with_replacer_observed(
        artifact,
        target,
        bound_target,
        oracle,
        &SystemAtomicReplacer,
        &cancellation,
        |_| {},
        Arc::new(|| false),
    )
}

pub fn restore_overwrite_observed(
    artifact: &Path,
    target: &Path,
    bound_target: &SqliteFileProbe,
    oracle: Option<&ScalarOracle>,
    cancellation: &BackupCancellationToken,
    observer: impl FnMut(BackupProgress),
    validation_control: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<ValidationEvidence, SqliteProviderError> {
    restore_overwrite_with_replacer_observed(
        artifact,
        target,
        bound_target,
        oracle,
        &SystemAtomicReplacer,
        cancellation,
        observer,
        validation_control,
    )
}

pub fn restore_overwrite_with_replacer(
    artifact: &Path,
    target: &Path,
    bound_target: &SqliteFileProbe,
    oracle: Option<&ScalarOracle>,
    replacer: &dyn AtomicReplacer,
) -> Result<ValidationEvidence, SqliteProviderError> {
    let cancellation = BackupCancellationToken::default();
    restore_overwrite_with_replacer_observed(
        artifact,
        target,
        bound_target,
        oracle,
        replacer,
        &cancellation,
        |_| {},
        Arc::new(|| false),
    )
}

#[allow(clippy::too_many_arguments)]
fn restore_overwrite_with_replacer_observed(
    artifact: &Path,
    target: &Path,
    bound_target: &SqliteFileProbe,
    oracle: Option<&ScalarOracle>,
    replacer: &dyn AtomicReplacer,
    cancellation: &BackupCancellationToken,
    observer: impl FnMut(BackupProgress),
    validation_control: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<ValidationEvidence, SqliteProviderError> {
    let artifact_identity = probe_sqlite_file(artifact)?;
    let current_target = probe_sqlite_file(target)?;
    validate_target_binding(bound_target, &current_target)?;
    if current_target.readonly() {
        return Err(SqliteProviderError::TargetReadOnly);
    }
    if artifact_identity.same_file(&current_target) {
        return Err(SqliteProviderError::SameFile);
    }
    let parent = ensure_safe_parent(target)?;
    let stage = parent.join(format!(".dbbackup-restore-{}.sqlite", uuid::Uuid::new_v4()));
    let _guard = StagedFileGuard(stage.clone());
    online_backup_to_path_observed(
        artifact,
        &stage,
        &OnlineBackupOptions::default(),
        cancellation,
        observer,
    )?;
    let stage_control = Arc::clone(&validation_control);
    let validation = validate_database_observed(&stage, oracle, move || stage_control())?;
    sync_file(&stage)?;

    let immediately_before_replace = probe_sqlite_file(target)?;
    validate_target_binding(bound_target, &immediately_before_replace)?;
    replacer.replace(target, &stage)?;
    sync_file(target)?;
    sync_directory(&parent)?;
    let final_validation =
        validate_database_observed(target, oracle, move || validation_control())?;
    if validation != final_validation {
        return Err(SqliteProviderError::IntegrityFailed(
            "post-replace validation differs from staged validation".to_owned(),
        ));
    }
    Ok(final_validation)
}

fn validate_target_binding(
    bound: &SqliteFileProbe,
    observed: &SqliteFileProbe,
) -> Result<(), SqliteProviderError> {
    if bound.same_file(observed) {
        Ok(())
    } else {
        Err(SqliteProviderError::TargetIdentityChanged)
    }
}

struct StagedFileGuard(PathBuf);

impl Drop for StagedFileGuard {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}

#[cfg(windows)]
fn system_atomic_replace(target: &Path, replacement: &Path) -> Result<(), SqliteProviderError> {
    use std::iter::once;
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{ReplaceFileW, REPLACE_FILE_FLAGS};

    let parent = target.parent().ok_or_else(|| {
        SqliteProviderError::PathInvalid(format!("target has no parent: {}", target.display()))
    })?;
    let backup = parent.join(format!(
        ".dbbackup-original-{}.sqlite",
        uuid::Uuid::new_v4()
    ));
    let target_wide = target
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect::<Vec<_>>();
    let replacement_wide = replacement
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect::<Vec<_>>();
    let backup_wide = backup
        .as_os_str()
        .encode_wide()
        .chain(once(0))
        .collect::<Vec<_>>();
    let result = unsafe {
        ReplaceFileW(
            PCWSTR(target_wide.as_ptr()),
            PCWSTR(replacement_wide.as_ptr()),
            PCWSTR(backup_wide.as_ptr()),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    };
    if let Err(error) = result {
        if !target.exists() && backup.exists() {
            std::fs::rename(&backup, target)?;
        }
        return Err(SqliteProviderError::AtomicReplaceFailed(error.to_string()));
    }
    sync_file(target)?;
    if backup.exists() {
        std::fs::remove_file(&backup)?;
    }
    Ok(())
}

#[cfg(not(windows))]
fn system_atomic_replace(target: &Path, replacement: &Path) -> Result<(), SqliteProviderError> {
    let permissions = std::fs::metadata(target)?.permissions();
    std::fs::set_permissions(replacement, permissions)?;
    std::fs::rename(replacement, target)
        .map_err(|error| SqliteProviderError::AtomicReplaceFailed(error.to_string()))
}
