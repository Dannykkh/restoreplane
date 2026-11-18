use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use fs2::FileExt;
use rusqlite::{Connection, OpenFlags};

use crate::session::NativePrincipal;
use crate::WorkbenchError;

pub trait UserRootPolicy: Send + Sync {
    /// Native composition creates/verifies the current user's private directory.
    /// Implementations must reject reparse/symlink traversal and foreign ACLs.
    fn ensure_private_root(&self, root: &Path) -> Result<(), WorkbenchError>;
}

pub struct ClientStore {
    root: PathBuf,
    owner_id: String,
    journal_epoch: String,
    connection: Mutex<Connection>,
    anchor: Mutex<Option<File>>,
    disabled: AtomicBool,
    _process_lock: File,
}

const SCHEMA: &str =
    include_str!("../../../docs/plan/database-management-workspace/schema-client-v1.sql");

#[derive(serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Anchor {
    owner: String,
    journal_epoch: String,
    revision: i64,
    phase: String,
}
fn header_anchor(connection: &Connection) -> Result<Anchor, WorkbenchError> {
    connection
        .query_row(
            "SELECT owner_id,journal_epoch,storage_revision FROM journal_header WHERE singleton=1",
            [],
            |row| {
                Ok(Anchor {
                    owner: row.get(0)?,
                    journal_epoch: row.get(1)?,
                    revision: row.get(2)?,
                    phase: "clean".into(),
                })
            },
        )
        .map_err(|_| WorkbenchError::StoreUnavailable)
}
fn read_anchor(file: &mut File) -> Result<Anchor, WorkbenchError> {
    if file
        .metadata()
        .map_err(|_| WorkbenchError::StoreUnavailable)?
        .len()
        > 1024
    {
        return Err(WorkbenchError::StoreUnavailable);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    let mut bytes = Vec::with_capacity(1024);
    file.take(1025)
        .read_to_end(&mut bytes)
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    serde_json::from_slice(&bytes).map_err(|_| WorkbenchError::StoreUnavailable)
}
fn persist_anchor(file: &mut File, anchor: &Anchor) -> Result<(), WorkbenchError> {
    let bytes = serde_json::to_vec(anchor).map_err(|_| WorkbenchError::StoreUnavailable)?;
    if bytes.len() > 1024 {
        return Err(WorkbenchError::StoreUnavailable);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    file.write_all(&bytes)
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    file.set_len(bytes.len() as u64)
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    file.sync_all()
        .map_err(|_| WorkbenchError::StoreUnavailable)
}
fn anchor_file(path: &Path, create: bool) -> Result<File, WorkbenchError> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(create);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(1);
    }
    let file = options
        .open(path)
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    file.try_lock_exclusive()
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    Ok(file)
}

impl ClientStore {
    pub fn open(
        root: &Path,
        principal: &NativePrincipal,
        policy: &dyn UserRootPolicy,
    ) -> Result<Self, WorkbenchError> {
        principal.ensure_standard()?;
        policy.ensure_private_root(root)?;
        if std::fs::symlink_metadata(root)
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .file_type()
            .is_symlink()
        {
            return Err(WorkbenchError::Forbidden);
        }
        let lock_path = root.join("client-state.lock");
        reject_symlink(&lock_path)?;
        let process_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        process_lock
            .try_lock_exclusive()
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        let database_path = root.join("client-state.sqlite3");
        let anchor_path = root.join("client-state.anchor");
        reject_symlink(&database_path)?;
        reject_symlink(&anchor_path)?;
        let existing = database_path.exists();
        // Never recreate a missing half of a previously initialized pair.
        if !existing && anchor_path.exists() {
            return Err(WorkbenchError::StoreUnavailable);
        }
        let mut anchor = if existing {
            anchor_file(&anchor_path, false).ok()
        } else {
            None
        };
        let mut disabled = false;
        if existing {
            let inspection =
                Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|_| WorkbenchError::StoreUnavailable)?;
            validate_header(&inspection, principal.owner_id())?;
            let expected = header_anchor(&inspection)?;
            disabled = anchor
                .as_mut()
                .is_none_or(|file| read_anchor(file).map_or(true, |actual| actual != expected));
        }
        let mut connection = if disabled {
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        } else {
            Connection::open(&database_path)
        }
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(2))
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        let journal_epoch = if existing {
            connection
                .pragma_update(None, "foreign_keys", true)
                .map_err(|_| WorkbenchError::StoreUnavailable)?;
            connection
                .pragma_update(None, "trusted_schema", false)
                .map_err(|_| WorkbenchError::StoreUnavailable)?;
            if !disabled {
                connection
                    .pragma_update(None, "journal_mode", "WAL")
                    .map_err(|_| WorkbenchError::StoreUnavailable)?;
            }
            connection
                .pragma_update(None, "synchronous", "FULL")
                .map_err(|_| WorkbenchError::StoreUnavailable)?;
            validate_header(&connection, principal.owner_id())?
        } else {
            connection
                .execute_batch(SCHEMA)
                .map_err(|_| WorkbenchError::StoreUnavailable)?;
            let epoch = uuid::Uuid::new_v4().to_string();
            let transaction = connection
                .transaction()
                .map_err(|_| WorkbenchError::StoreUnavailable)?;
            transaction.execute("INSERT INTO journal_header(singleton,schema_version,owner_id,journal_epoch,created_at_ms,updated_at_ms) VALUES(1,1,?1,?2,0,0)", rusqlite::params![principal.owner_id(), epoch]).map_err(|_| WorkbenchError::StoreUnavailable)?;
            transaction
                .commit()
                .map_err(|_| WorkbenchError::StoreUnavailable)?;
            let mut created = anchor_file(&anchor_path, true)?;
            persist_anchor(&mut created, &header_anchor(&connection)?)?;
            anchor = Some(created);
            epoch
        };
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        let synchronous: i64 = connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        let foreign_keys: i64 = connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        if mode != "wal" || synchronous != 2 || foreign_keys != 1 {
            return Err(WorkbenchError::StoreUnavailable);
        }
        Ok(Self {
            root: root.into(),
            owner_id: principal.owner_id().into(),
            journal_epoch,
            connection: Mutex::new(connection),
            anchor: Mutex::new(anchor),
            disabled: AtomicBool::new(disabled),
            _process_lock: process_lock,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }
    pub fn journal_epoch(&self) -> &str {
        &self.journal_epoch
    }
    pub fn write_enabled(&self) -> bool {
        !self.disabled.load(Ordering::SeqCst)
    }

    /// Local single-writer hook consumed by the operation-journal module.
    pub fn read<T>(
        &self,
        work: impl FnOnce(&Connection) -> Result<T, WorkbenchError>,
    ) -> Result<T, WorkbenchError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        work(&connection)
    }

    /// Local single-writer hook consumed by the operation-journal module.
    pub fn write<T>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, WorkbenchError>,
    ) -> Result<T, WorkbenchError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        if !self.write_enabled() {
            return Err(WorkbenchError::StoreUnavailable);
        }
        let mut file = self
            .anchor
            .lock()
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        let guarded = (|| {
            let file = file.as_mut().ok_or(WorkbenchError::StoreUnavailable)?;
            let mut before = header_anchor(&connection)?;
            if read_anchor(file)? != before || before.revision == i64::MAX {
                return Err(WorkbenchError::StoreUnavailable);
            }
            before.phase = "dirty".into();
            persist_anchor(file, &before)?;
            let outcome = work(&mut connection);
            if matches!(
                outcome,
                Err(WorkbenchError::StoreUnavailable | WorkbenchError::Io)
            ) || !connection.is_autocommit()
            {
                return Err(WorkbenchError::StoreUnavailable);
            }
            // A business error may have committed a cancellation/quarantine row.
            // Always seal its resulting state before returning that same error.
            let mut after = header_anchor(&connection)?;
            if after.owner != before.owner
                || after.revision != before.revision
                || uuid::Uuid::parse_str(&after.journal_epoch).is_err()
            {
                return Err(WorkbenchError::StoreUnavailable);
            }
            let changed=connection.execute("UPDATE journal_header SET storage_revision=?1 WHERE singleton=1 AND storage_revision=?2",rusqlite::params![before.revision+1,before.revision]).map_err(|_|WorkbenchError::StoreUnavailable)?;
            if changed != 1 {
                return Err(WorkbenchError::StoreUnavailable);
            }
            after.revision += 1;
            persist_anchor(file, &after)?;
            Ok(outcome)
        })();
        match guarded {
            Ok(outcome) => outcome,
            Err(error) => {
                self.disabled.store(true, Ordering::SeqCst);
                Err(error)
            }
        }
    }
}

fn validate_header(connection: &Connection, owner: &str) -> Result<String, WorkbenchError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    if version != 1 {
        return Err(WorkbenchError::StoreUnavailable);
    }
    let (stored_owner, epoch): (String, String) = connection.query_row("SELECT owner_id,journal_epoch FROM journal_header WHERE singleton=1 AND schema_version=1", [], |row| Ok((row.get(0)?, row.get(1)?))).map_err(|_| WorkbenchError::StoreUnavailable)?;
    if stored_owner != owner || uuid::Uuid::parse_str(&epoch).is_err() {
        return Err(WorkbenchError::Forbidden);
    }
    let tables: i64 = connection.query_row("SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name IN ('journal_header','target_bindings','operations','statements','saved_queries')", [], |row| row.get(0)).map_err(|_| WorkbenchError::StoreUnavailable)?;
    if tables != 5
        || connection
            .prepare("PRAGMA foreign_key_check")
            .map_err(|_| WorkbenchError::StoreUnavailable)?
            .exists([])
            .map_err(|_| WorkbenchError::StoreUnavailable)?
    {
        return Err(WorkbenchError::StoreUnavailable);
    }
    // Experimental schema-v1 files must not be accepted merely because their
    // version number matches: changed transaction links/constraints need review.
    let reference = Connection::open_in_memory().map_err(|_| WorkbenchError::StoreUnavailable)?;
    reference
        .execute_batch(SCHEMA)
        .map_err(|_| WorkbenchError::StoreUnavailable)?;
    for table in [
        "journal_header",
        "target_bindings",
        "operations",
        "statements",
        "saved_queries",
    ] {
        let expected: String = reference
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        let actual: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type='table' AND name=?1",
                [table],
                |row| row.get(0),
            )
            .map_err(|_| WorkbenchError::StoreUnavailable)?;
        if actual != expected {
            return Err(WorkbenchError::StoreUnavailable);
        }
    }
    Ok(epoch)
}

pub(crate) fn reject_symlink(path: &Path) -> Result<(), WorkbenchError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(WorkbenchError::Forbidden),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(WorkbenchError::Io),
    }
}
