use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::backup::Backup;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::{operational_schema_v4_checksum, OperationalStore};

use super::RepositoryError;

#[derive(Clone, Debug)]
pub struct ControlBackupSpec {
    id: String,
    backup_path: PathBuf,
    rehearsal_path: PathBuf,
    started_at_ms: i64,
}

impl ControlBackupSpec {
    pub fn new(
        id: impl Into<String>,
        backup_path: PathBuf,
        rehearsal_path: PathBuf,
        started_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            backup_path,
            rehearsal_path,
            started_at_ms,
        }
    }

    fn validate(&self, source_path: &Path) -> Result<(), RepositoryError> {
        if self.id.is_empty()
            || self.id.len() > 128
            || self.started_at_ms < 0
            || !self.backup_path.is_absolute()
            || !self.rehearsal_path.is_absolute()
            || self.backup_path == self.rehearsal_path
            || self.backup_path == source_path
            || self.rehearsal_path == source_path
            || self.backup_path.exists()
            || self.rehearsal_path.exists()
        {
            return Err(RepositoryError::ControlStoreBackupInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlBackupReport {
    checksum_blake3: String,
    integrity_result: &'static str,
    rehearsal_result: &'static str,
    schema_version: i64,
    application_table_count: i64,
}

impl ControlBackupReport {
    pub fn checksum_blake3(&self) -> &str {
        &self.checksum_blake3
    }

    pub const fn integrity_result(&self) -> &'static str {
        self.integrity_result
    }

    pub const fn rehearsal_result(&self) -> &'static str {
        self.rehearsal_result
    }

    pub const fn schema_version(&self) -> i64 {
        self.schema_version
    }

    pub const fn application_table_count(&self) -> i64 {
        self.application_table_count
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Validation {
    schema_version: i64,
    application_table_count: i64,
}

#[derive(Clone)]
pub struct ControlStoreRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn control_store(&self) -> ControlStoreRepository {
        ControlStoreRepository {
            store: self.clone(),
        }
    }
}

impl ControlStoreRepository {
    pub fn backup_and_rehearse(
        &self,
        spec: ControlBackupSpec,
    ) -> Result<ControlBackupReport, RepositoryError> {
        spec.validate(self.store.database_path())?;
        self.insert_running(&spec)?;

        let mut integrity_result = "not_run";
        let mut rehearsal_result = "not_run";
        let result = (|| {
            let source = self.store.open_reader()?;
            backup_connection(source.connection(), &spec.backup_path)?;
            let backup_validation = match validate_database(&spec.backup_path) {
                Ok(validation) => {
                    integrity_result = "passed";
                    validation
                }
                Err(error) => {
                    integrity_result = "failed";
                    return Err(error);
                }
            };

            let backup_source = open_read_only(&spec.backup_path)?;
            if let Err(error) = backup_connection(&backup_source, &spec.rehearsal_path) {
                rehearsal_result = "failed";
                return Err(error);
            }
            let rehearsal_validation = match validate_database(&spec.rehearsal_path) {
                Ok(validation) => {
                    rehearsal_result = "passed";
                    validation
                }
                Err(error) => {
                    rehearsal_result = "failed";
                    return Err(error);
                }
            };
            if backup_validation != rehearsal_validation {
                rehearsal_result = "failed";
                return Err(RepositoryError::ControlStoreBackupInvalid);
            }
            let checksum_blake3 = checksum_file(&spec.backup_path)?;
            Ok((backup_validation, checksum_blake3))
        })();

        let completed_at_ms = current_time_ms().max(spec.started_at_ms);
        match result {
            Ok((validation, checksum_blake3)) => {
                self.mark_succeeded(
                    &spec.id,
                    &checksum_blake3,
                    completed_at_ms,
                    integrity_result,
                    rehearsal_result,
                )?;
                Ok(ControlBackupReport {
                    checksum_blake3,
                    integrity_result,
                    rehearsal_result,
                    schema_version: validation.schema_version,
                    application_table_count: validation.application_table_count,
                })
            }
            Err(error) => {
                self.mark_failed(
                    &spec.id,
                    completed_at_ms,
                    integrity_result,
                    rehearsal_result,
                    error.code(),
                )?;
                Err(error)
            }
        }
    }

    pub fn backup_status(&self, backup_id: &str) -> Result<String, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT status FROM control_store_backups WHERE id = ?1",
                [backup_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    pub fn successful_backup_count(&self) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM control_store_backups WHERE status = 'succeeded'",
                [],
                |row| row.get(0),
            )
            .map_err(RepositoryError::from)
    }

    fn insert_running(&self, spec: &ControlBackupSpec) -> Result<(), RepositoryError> {
        let id = spec.id.clone();
        let locator = spec.backup_path.to_string_lossy().into_owned();
        let started_at_ms = spec.started_at_ms;
        self.write(move |connection| {
            connection.execute(
                "INSERT INTO control_store_backups(
                     id, storage_kind, locator, schema_version, started_at_ms,
                     status, integrity_result, rehearsal_result, created_at_ms
                 ) VALUES (?1, 'local', ?2, 2, ?3, 'running', 'not_run', 'not_run', ?3)",
                params![id, locator, started_at_ms],
            )?;
            Ok(())
        })
    }

    fn mark_succeeded(
        &self,
        backup_id: &str,
        checksum_blake3: &str,
        completed_at_ms: i64,
        integrity_result: &str,
        rehearsal_result: &str,
    ) -> Result<(), RepositoryError> {
        let backup_id = backup_id.to_owned();
        let checksum_blake3 = checksum_blake3.to_owned();
        let integrity_result = integrity_result.to_owned();
        let rehearsal_result = rehearsal_result.to_owned();
        self.write(move |connection| {
            let affected = connection.execute(
                "UPDATE control_store_backups
                 SET checksum_blake3 = ?2, completed_at_ms = ?3, status = 'succeeded',
                     integrity_result = ?4, rehearsal_result = ?5, rehearsal_at_ms = ?3
                 WHERE id = ?1 AND status = 'running'",
                params![
                    backup_id,
                    checksum_blake3,
                    completed_at_ms,
                    integrity_result,
                    rehearsal_result
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            Ok(())
        })
    }

    fn mark_failed(
        &self,
        backup_id: &str,
        completed_at_ms: i64,
        integrity_result: &str,
        rehearsal_result: &str,
        error_code: &str,
    ) -> Result<(), RepositoryError> {
        let backup_id = backup_id.to_owned();
        let integrity_result = integrity_result.to_owned();
        let rehearsal_result = rehearsal_result.to_owned();
        let error_code = error_code.to_owned();
        self.write(move |connection| {
            let affected = connection.execute(
                "UPDATE control_store_backups
                 SET completed_at_ms = ?2, status = 'failed',
                     integrity_result = ?3, rehearsal_result = ?4,
                     rehearsal_at_ms = CASE WHEN ?4 <> 'not_run' THEN ?2 ELSE NULL END,
                     error_code = ?5
                 WHERE id = ?1 AND status = 'running'",
                params![
                    backup_id,
                    completed_at_ms,
                    integrity_result,
                    rehearsal_result,
                    error_code
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            Ok(())
        })
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

fn backup_connection(source: &Connection, destination_path: &Path) -> Result<(), RepositoryError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination_path)
        .map_err(|_| RepositoryError::ControlStoreBackupIo)?;
    let mut destination =
        Connection::open(destination_path).map_err(|_| RepositoryError::ControlStoreBackupIo)?;
    {
        let backup = Backup::new(source, &mut destination)
            .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
        backup
            .run_to_completion(64, Duration::from_millis(2), None)
            .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
    }
    Ok(())
}

fn open_read_only(path: &Path) -> Result<Connection, RepositoryError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| RepositoryError::ControlStoreBackupIo)
}

fn validate_database(path: &Path) -> Result<Validation, RepositoryError> {
    let connection = open_read_only(path)?;
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
    if integrity != "ok" {
        return Err(RepositoryError::ControlStoreBackupInvalid);
    }
    let mut foreign_key_check = connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
    let mut violations = foreign_key_check
        .query([])
        .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
    if violations
        .next()
        .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?
        .is_some()
    {
        return Err(RepositoryError::ControlStoreBackupInvalid);
    }
    drop(violations);
    drop(foreign_key_check);

    let schema_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
    let application_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*)
             FROM pragma_table_list
             WHERE schema = 'main'
               AND type = 'table'
               AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
    let migration_checksum: String = connection
        .query_row(
            "SELECT checksum_blake3 FROM schema_migrations WHERE version = 4",
            [],
            |row| row.get(0),
        )
        .map_err(|_| RepositoryError::ControlStoreBackupInvalid)?;
    if schema_version != 4
        || application_table_count != 67
        || migration_checksum != operational_schema_v4_checksum()
    {
        return Err(RepositoryError::ControlStoreBackupInvalid);
    }
    Ok(Validation {
        schema_version,
        application_table_count,
    })
}

fn checksum_file(path: &Path) -> Result<String, RepositoryError> {
    let mut file = File::open(path).map_err(|_| RepositoryError::ControlStoreBackupIo)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| RepositoryError::ControlStoreBackupIo)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
