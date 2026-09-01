use std::path::PathBuf;

use rusqlite::ffi::ErrorCode;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum SqliteProviderError {
    #[error("SQLite path is invalid: {0}")]
    PathInvalid(String),
    #[error("SQLite path contains a symlink or reparse point: {0}")]
    ReparsePoint(PathBuf),
    #[error("SQLite file header is invalid: {0}")]
    HeaderInvalid(PathBuf),
    #[error("SQLite file I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[source] rusqlite::Error),
    #[error("SQLite backup could not acquire a lock within the bounded retry policy")]
    BusyTimeout,
    #[error("SQLite backup was cancelled")]
    Cancelled,
    #[error("SQLite backup output already exists: {0}")]
    OutputExists(PathBuf),
    #[error("SQLite backup output space is insufficient")]
    SpaceInsufficient,
    #[error("SQLite backup source path changed identity during the operation")]
    SourceIdentityChanged,
    #[error("SQLite integrity_check failed: {0}")]
    IntegrityFailed(String),
    #[error("SQLite foreign_key_check reported violations")]
    ForeignKeyFailed,
    #[error("SQLite application oracle failed: {0}")]
    OracleFailed(String),
    #[error("SQLite create-new target already exists: {0}")]
    CreateNewTargetExists(PathBuf),
    #[error("SQLite overwrite target identity changed")]
    TargetIdentityChanged,
    #[error("SQLite source and restore target refer to the same file")]
    SameFile,
    #[error("SQLite restore target is read-only")]
    TargetReadOnly,
    #[error("SQLite atomic replacement failed: {0}")]
    AtomicReplaceFailed(String),
}

impl SqliteProviderError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PathInvalid(_) => "SQLITE_PATH_INVALID",
            Self::ReparsePoint(_) => "SQLITE_PATH_REPARSE_POINT",
            Self::HeaderInvalid(_) => "SQLITE_HEADER_INVALID",
            Self::Io(_) | Self::Sqlite(_) => "SQLITE_PROVIDER_IO",
            Self::BusyTimeout => "SQLITE_BACKUP_BUSY_TIMEOUT",
            Self::Cancelled => "SQLITE_BACKUP_CANCELLED",
            Self::OutputExists(_) => "SQLITE_BACKUP_OUTPUT_EXISTS",
            Self::SpaceInsufficient => "SQLITE_BACKUP_SPACE_INSUFFICIENT",
            Self::SourceIdentityChanged => "SQLITE_BACKUP_SOURCE_IDENTITY_CHANGED",
            Self::IntegrityFailed(_) => "SQLITE_VALIDATION_INTEGRITY_FAILED",
            Self::ForeignKeyFailed => "SQLITE_VALIDATION_FOREIGN_KEY_FAILED",
            Self::OracleFailed(_) => "SQLITE_VALIDATION_ORACLE_FAILED",
            Self::CreateNewTargetExists(_) => "SQLITE_RESTORE_CREATE_NEW_TARGET_EXISTS",
            Self::TargetIdentityChanged => "SQLITE_RESTORE_TARGET_IDENTITY_CHANGED",
            Self::SameFile => "SQLITE_RESTORE_SAME_FILE",
            Self::TargetReadOnly => "SQLITE_RESTORE_TARGET_READ_ONLY",
            Self::AtomicReplaceFailed(_) => "SQLITE_RESTORE_ATOMIC_REPLACE_FAILED",
        }
    }
}

impl From<rusqlite::Error> for SqliteProviderError {
    fn from(source: rusqlite::Error) -> Self {
        match &source {
            rusqlite::Error::SqliteFailure(error, _)
                if matches!(
                    error.code,
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
                ) =>
            {
                Self::BusyTimeout
            }
            rusqlite::Error::SqliteFailure(error, _) if error.code == ErrorCode::DiskFull => {
                Self::SpaceInsufficient
            }
            _ => Self::Sqlite(source),
        }
    }
}
