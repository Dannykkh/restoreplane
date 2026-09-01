use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{bounded, select_biased, unbounded, Sender, TrySendError};
use rusqlite::{Connection, DatabaseName, OpenFlags};
use thiserror::Error;

use crate::{apply_operational_store_migrations, MigrationError};

type WriteJob = Box<dyn FnOnce(&mut Connection) + Send>;
const LOW_PRIORITY_QUEUE_CAPACITY: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionDiagnostics {
    pub journal_mode: String,
    pub synchronous: i64,
    pub foreign_keys: i64,
    pub auto_vacuum: i64,
    pub busy_timeout_ms: i64,
    pub query_only: i64,
    pub user_version: i64,
    pub application_table_count: i64,
}

#[derive(Debug, Error)]
pub enum OperationalStoreError {
    #[error("OPERATIONAL_STORE_SQLITE")]
    Sqlite(#[source] rusqlite::Error),
    #[error("OPERATIONAL_STORE_MIGRATION")]
    Migration(#[source] MigrationError),
    #[error("OPERATIONAL_STORE_WRITER_UNAVAILABLE")]
    WriterUnavailable,
    #[error("OPERATIONAL_STORE_THREAD_START_FAILED")]
    ThreadStart(#[source] std::io::Error),
    #[error("OPERATIONAL_STORE_LOW_PRIORITY_QUEUE_FULL")]
    LowPriorityQueueFull,
}

impl OperationalStoreError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Sqlite(_) => "OPERATIONAL_STORE_SQLITE",
            Self::Migration(error) => error.code(),
            Self::WriterUnavailable => "OPERATIONAL_STORE_WRITER_UNAVAILABLE",
            Self::ThreadStart(_) => "OPERATIONAL_STORE_THREAD_START_FAILED",
            Self::LowPriorityQueueFull => "OPERATIONAL_STORE_LOW_PRIORITY_QUEUE_FULL",
        }
    }
}

impl From<rusqlite::Error> for OperationalStoreError {
    fn from(source: rusqlite::Error) -> Self {
        Self::Sqlite(source)
    }
}

impl From<MigrationError> for OperationalStoreError {
    fn from(source: MigrationError) -> Self {
        Self::Migration(source)
    }
}

enum WriterMessage {
    Diagnostics(mpsc::Sender<Result<ConnectionDiagnostics, OperationalStoreError>>),
    Checkpoint(mpsc::Sender<Result<(), OperationalStoreError>>),
    Execute(WriteJob),
}

#[derive(Clone)]
pub struct OperationalStore {
    db_path: PathBuf,
    high_writer: Sender<WriterMessage>,
    low_writer: Sender<WriterMessage>,
}

impl OperationalStore {
    pub fn open(db_path: PathBuf) -> Result<Self, OperationalStoreError> {
        let (high_writer, high_receiver) = unbounded::<WriterMessage>();
        let (low_writer, low_receiver) = bounded::<WriterMessage>(LOW_PRIORITY_QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let writer_path = db_path.clone();
        thread::Builder::new()
            .name("operational-store-writer".to_owned())
            .spawn(move || {
                let mut connection = match open_writer_connection(&writer_path) {
                    Ok(connection) => {
                        let _ = ready_tx.send(Ok(()));
                        connection
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                loop {
                    select_biased! {
                        recv(high_receiver) -> message => {
                            match message {
                                Ok(message) => handle_writer_message(message, &mut connection),
                                Err(_) => return,
                            }
                        }
                        recv(low_receiver) -> message => {
                            match message {
                                Ok(message) => handle_writer_message(message, &mut connection),
                                Err(_) => return,
                            }
                        }
                    }
                }
            })
            .map_err(OperationalStoreError::ThreadStart)?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                db_path,
                high_writer,
                low_writer,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(OperationalStoreError::WriterUnavailable),
        }
    }

    pub fn writer_diagnostics(&self) -> Result<ConnectionDiagnostics, OperationalStoreError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.high_writer
            .send(WriterMessage::Diagnostics(reply_tx))
            .map_err(|_| OperationalStoreError::WriterUnavailable)?;
        reply_rx
            .recv()
            .map_err(|_| OperationalStoreError::WriterUnavailable)?
    }

    pub fn open_reader(&self) -> Result<OperationalReader, OperationalStoreError> {
        OperationalReader::open(&self.db_path)
    }

    pub fn checkpoint_wal(&self) -> Result<(), OperationalStoreError> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.high_writer
            .send(WriterMessage::Checkpoint(reply_tx))
            .map_err(|_| OperationalStoreError::WriterUnavailable)?;
        reply_rx
            .recv()
            .map_err(|_| OperationalStoreError::WriterUnavailable)?
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.db_path
    }

    pub(crate) fn run_write<F, T, E>(&self, job: F) -> Result<Result<T, E>, OperationalStoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.high_writer
            .send(WriterMessage::Execute(Box::new(move |connection| {
                let _ = reply_tx.send(job(connection));
            })))
            .map_err(|_| OperationalStoreError::WriterUnavailable)?;
        reply_rx
            .recv()
            .map_err(|_| OperationalStoreError::WriterUnavailable)
    }

    pub(crate) fn run_low_priority_write<F, T, E>(
        &self,
        job: F,
    ) -> Result<Result<T, E>, OperationalStoreError>
    where
        F: FnOnce(&mut Connection) -> Result<T, E> + Send + 'static,
        T: Send + 'static,
        E: Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.low_writer
            .try_send(WriterMessage::Execute(Box::new(move |connection| {
                let _ = reply_tx.send(job(connection));
            })))
            .map_err(|error| match error {
                TrySendError::Full(_) => OperationalStoreError::LowPriorityQueueFull,
                TrySendError::Disconnected(_) => OperationalStoreError::WriterUnavailable,
            })?;
        reply_rx
            .recv()
            .map_err(|_| OperationalStoreError::WriterUnavailable)
    }
}

pub struct OperationalReader {
    connection: Connection,
}

impl OperationalReader {
    fn open(db_path: &Path) -> Result<Self, OperationalStoreError> {
        let connection = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(Duration::from_millis(5000))?;
        connection.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA query_only=ON;",
        )?;
        Ok(Self { connection })
    }

    pub fn diagnostics(&self) -> Result<ConnectionDiagnostics, OperationalStoreError> {
        connection_diagnostics(&self.connection)
    }

    pub fn is_read_only(&self) -> Result<bool, OperationalStoreError> {
        self.connection
            .is_readonly(DatabaseName::Main)
            .map_err(OperationalStoreError::from)
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}

fn open_writer_connection(db_path: &Path) -> Result<Connection, OperationalStoreError> {
    let connection = Connection::open(db_path)?;
    connection.busy_timeout(Duration::from_millis(5000))?;
    connection.execute_batch(
        "PRAGMA auto_vacuum=INCREMENTAL;
         PRAGMA foreign_keys=ON;
         PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;",
    )?;
    apply_operational_store_migrations(&connection)?;
    Ok(connection)
}

fn handle_writer_message(message: WriterMessage, connection: &mut Connection) {
    match message {
        WriterMessage::Diagnostics(reply) => {
            let _ = reply.send(connection_diagnostics(connection));
        }
        WriterMessage::Checkpoint(reply) => {
            let result = connection
                .query_row("PRAGMA wal_checkpoint(FULL)", [], |_| Ok(()))
                .map_err(OperationalStoreError::from);
            let _ = reply.send(result);
        }
        WriterMessage::Execute(job) => job(connection),
    }
}

fn connection_diagnostics(
    connection: &Connection,
) -> Result<ConnectionDiagnostics, OperationalStoreError> {
    Ok(ConnectionDiagnostics {
        journal_mode: connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?,
        synchronous: connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?,
        foreign_keys: connection.query_row("PRAGMA foreign_keys", [], |row| row.get(0))?,
        auto_vacuum: connection.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?,
        busy_timeout_ms: connection.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?,
        query_only: connection.query_row("PRAGMA query_only", [], |row| row.get(0))?,
        user_version: connection.query_row("PRAGMA user_version", [], |row| row.get(0))?,
        application_table_count: connection.query_row(
            "SELECT COUNT(*)
             FROM pragma_table_list
             WHERE schema = 'main'
               AND type = 'table'
               AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use super::*;

    #[test]
    fn high_priority_control_write_overtakes_queued_activity_write() {
        let temp_dir = tempfile::TempDir::new().expect("temp directory");
        let store =
            OperationalStore::open(temp_dir.path().join("operational.db")).expect("open store");
        let order = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_order = Arc::clone(&order);
        store
            .low_writer
            .try_send(WriterMessage::Execute(Box::new(move |_| {
                started_tx.send(()).expect("signal first low start");
                release_rx.recv().expect("release first low");
                first_order.lock().expect("order lock").push("low-first");
            })))
            .expect("queue first low write");
        started_rx.recv().expect("first low started");

        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second_order = Arc::clone(&order);
        store
            .low_writer
            .try_send(WriterMessage::Execute(Box::new(move |_| {
                second_order.lock().expect("order lock").push("low-second");
                second_done_tx.send(()).expect("signal second low");
            })))
            .expect("queue second low write");

        let (high_done_tx, high_done_rx) = mpsc::channel();
        let high_order = Arc::clone(&order);
        store
            .high_writer
            .send(WriterMessage::Execute(Box::new(move |_| {
                high_order.lock().expect("order lock").push("high");
                high_done_tx.send(()).expect("signal high");
            })))
            .expect("queue high write");

        release_tx.send(()).expect("release first low");
        high_done_rx.recv().expect("high write completed");
        second_done_rx.recv().expect("second low write completed");
        assert_eq!(
            *order.lock().expect("order lock"),
            vec!["low-first", "high", "low-second"]
        );
    }

    #[test]
    fn hundred_thousand_activity_submissions_are_bounded_without_starving_control() {
        const SUBMISSIONS: usize = 100_000;
        const CONTROL_BUDGET: Duration = Duration::from_secs(1);

        let temp_dir = tempfile::TempDir::new().expect("temp directory");
        let store =
            OperationalStore::open(temp_dir.path().join("operational.db")).expect("open store");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        store
            .low_writer
            .try_send(WriterMessage::Execute(Box::new(move |_| {
                started_tx.send(()).expect("signal blocked low write");
                release_rx.recv().expect("release blocked low write");
            })))
            .expect("start one low write");
        started_rx.recv().expect("low writer is blocked");

        let low_executed = Arc::new(AtomicUsize::new(0));
        let mut accepted = 0usize;
        let mut rejected = 0usize;
        for _ in 0..SUBMISSIONS {
            let low_executed = Arc::clone(&low_executed);
            match store
                .low_writer
                .try_send(WriterMessage::Execute(Box::new(move |_| {
                    low_executed.fetch_add(1, Ordering::SeqCst);
                }))) {
                Ok(()) => accepted += 1,
                Err(TrySendError::Full(_)) => rejected += 1,
                Err(TrySendError::Disconnected(_)) => panic!("writer disconnected"),
            }
        }
        assert_eq!(accepted, LOW_PRIORITY_QUEUE_CAPACITY);
        assert_eq!(rejected, SUBMISSIONS - LOW_PRIORITY_QUEUE_CAPACITY);

        let (high_done_tx, high_done_rx) = mpsc::channel();
        let observed_low = Arc::clone(&low_executed);
        store
            .high_writer
            .send(WriterMessage::Execute(Box::new(move |_| {
                high_done_tx
                    .send(observed_low.load(Ordering::SeqCst))
                    .expect("signal control write");
            })))
            .expect("queue control write");
        let released_at = Instant::now();
        release_tx.send(()).expect("release blocked activity");
        let low_before_control = high_done_rx
            .recv_timeout(CONTROL_BUDGET)
            .expect("control write meets starvation budget");
        assert_eq!(
            low_before_control, 0,
            "biased control lane must run before queued activity"
        );
        assert!(released_at.elapsed() < CONTROL_BUDGET);
    }
}
