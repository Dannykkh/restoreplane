//! SQLite file provider.
//!
//! Live databases are copied with SQLite's online backup API. A completed
//! database file is then exposed through the common `BackupSource` stream
//! contract, so encoding, Manifest, Replica, retention, and verification stay
//! shared with server engines.

mod backup;
mod certification;
mod error;
mod probe;
mod restore;
mod source;
mod validate;

pub use backup::{
    online_backup_to_path, online_backup_to_path_observed, BackupCancellationToken, BackupEvidence,
    BackupProgress, OnlineBackupOptions,
};
pub use certification::{sqlite_certification, SQLITE_PROVIDER_LIMITATIONS};
pub use error::SqliteProviderError;
pub use probe::{probe_sqlite_file, SqliteFileProbe};
pub use restore::{
    restore_create_new, restore_create_new_observed, restore_overwrite, restore_overwrite_observed,
    restore_overwrite_with_replacer, AtomicReplacer, SystemAtomicReplacer,
};
pub use source::SqliteSource;
pub use validate::{
    validate_database, validate_database_observed, ScalarOracle, ValidationEvidence,
};
