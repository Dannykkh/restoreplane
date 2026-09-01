use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use crate::OperationalStore;

use super::RepositoryError;

const MAX_ARCHIVE_ROWS: usize = 5_000;
const MAX_ARCHIVE_MANIFEST_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct TelemetryMaintenanceRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn telemetry_maintenance(&self) -> TelemetryMaintenanceRepository {
        TelemetryMaintenanceRepository {
            store: self.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedActivityEvent {
    seq: i64,
    occurred_at_ms: i64,
    severity: String,
    event_code: String,
    entity_type: String,
    entity_id: String,
    redaction_version: i64,
    message_blake3: String,
    attributes_blake3: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchivedNotificationOutbox {
    id: String,
    state: String,
    deduplication_key_blake3: String,
    payload_blake3: String,
    attempt_count: i64,
    last_error_code: Option<String>,
    delivered_at_ms: Option<i64>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryArchiveBatch {
    cutoff_ms: i64,
    activity: Vec<ArchivedActivityEvent>,
    outbox: Vec<ArchivedNotificationOutbox>,
}

impl TelemetryArchiveBatch {
    pub const fn cutoff_ms(&self) -> i64 {
        self.cutoff_ms
    }

    pub fn activity(&self) -> &[ArchivedActivityEvent] {
        &self.activity
    }

    pub fn outbox(&self) -> &[ArchivedNotificationOutbox] {
        &self.outbox
    }

    pub fn is_empty(&self) -> bool {
        self.activity.is_empty() && self.outbox.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelemetryArchiveCommit {
    activity_count: usize,
    outbox_count: usize,
}

impl TelemetryArchiveCommit {
    pub const fn activity_count(self) -> usize {
        self.activity_count
    }

    pub const fn outbox_count(self) -> usize {
        self.outbox_count
    }
}

impl TelemetryMaintenanceRepository {
    pub fn select_archive_batch(
        &self,
        cutoff_ms: i64,
        max_rows: usize,
    ) -> Result<TelemetryArchiveBatch, RepositoryError> {
        if cutoff_ms < 0 || max_rows == 0 || max_rows > MAX_ARCHIVE_ROWS {
            return Err(RepositoryError::TelemetryArchiveInputInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let mut activity_statement = connection.prepare(
            "SELECT
                 activity.seq,
                 activity.occurred_at_ms,
                 activity.severity,
                 activity.event_code,
                 activity.entity_type,
                 activity.entity_id,
                 activity.redaction_version,
                 activity.message,
                 activity.attributes_json
             FROM activity_events activity
             WHERE activity.occurred_at_ms < ?1
               AND NOT EXISTS (
                   SELECT 1
                   FROM attention_event_links link
                   JOIN attention_states attention
                     ON attention.id = link.attention_state_id
                   WHERE link.activity_event_seq = activity.seq
                     AND attention.state IN ('open', 'acknowledged')
               )
               AND NOT EXISTS (
                   SELECT 1
                   FROM notification_outbox outbox
                   WHERE outbox.activity_event_seq = activity.seq
                     AND outbox.state IN ('pending', 'delivering')
               )
             ORDER BY activity.seq
             LIMIT ?2",
        )?;
        let activity = activity_statement
            .query_map(params![cutoff_ms, max_rows as i64], |row| {
                let message: String = row.get(7)?;
                let attributes: String = row.get(8)?;
                Ok(ArchivedActivityEvent {
                    seq: row.get(0)?,
                    occurred_at_ms: row.get(1)?,
                    severity: row.get(2)?,
                    event_code: row.get(3)?,
                    entity_type: row.get(4)?,
                    entity_id: row.get(5)?,
                    redaction_version: row.get(6)?,
                    message_blake3: blake3::hash(message.as_bytes()).to_hex().to_string(),
                    attributes_blake3: blake3::hash(attributes.as_bytes()).to_hex().to_string(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let remaining = max_rows.saturating_sub(activity.len());
        let mut outbox = Vec::new();
        if remaining > 0 {
            let mut outbox_statement = connection.prepare(
                "SELECT
                     outbox.id,
                     outbox.state,
                     outbox.deduplication_key,
                     outbox.payload_json,
                     outbox.attempt_count,
                     outbox.last_error_code,
                     outbox.delivered_at_ms,
                     outbox.created_at_ms,
                     outbox.updated_at_ms
                 FROM notification_outbox outbox
                 WHERE outbox.updated_at_ms < ?1
                   AND outbox.state IN ('delivered', 'failed_terminal', 'cancelled')
                   AND NOT EXISTS (
                       SELECT 1
                       FROM attention_event_links link
                       JOIN attention_states attention
                         ON attention.id = link.attention_state_id
                       WHERE link.activity_event_seq = outbox.activity_event_seq
                         AND attention.state IN ('open', 'acknowledged')
                   )
                 ORDER BY outbox.updated_at_ms, outbox.id
                 LIMIT ?2",
            )?;
            outbox = outbox_statement
                .query_map(params![cutoff_ms, remaining as i64], |row| {
                    let deduplication_key: String = row.get(2)?;
                    let payload: String = row.get(3)?;
                    Ok(ArchivedNotificationOutbox {
                        id: row.get(0)?,
                        state: row.get(1)?,
                        deduplication_key_blake3: blake3::hash(deduplication_key.as_bytes())
                            .to_hex()
                            .to_string(),
                        payload_blake3: blake3::hash(payload.as_bytes()).to_hex().to_string(),
                        attempt_count: row.get(4)?,
                        last_error_code: row.get(5)?,
                        delivered_at_ms: row.get(6)?,
                        created_at_ms: row.get(7)?,
                        updated_at_ms: row.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
        }
        Ok(TelemetryArchiveBatch {
            cutoff_ms,
            activity,
            outbox,
        })
    }

    pub fn commit_archive(
        &self,
        archive_id: &str,
        manifest_json: &str,
        batch: &TelemetryArchiveBatch,
        committed_at_ms: i64,
    ) -> Result<TelemetryArchiveCommit, RepositoryError> {
        if archive_id.len() != 64
            || !archive_id.bytes().all(|value| value.is_ascii_hexdigit())
            || manifest_json.is_empty()
            || manifest_json.len() > MAX_ARCHIVE_MANIFEST_BYTES
            || serde_json::from_str::<serde_json::Value>(manifest_json).is_err()
            || batch.is_empty()
            || batch.activity.len().saturating_add(batch.outbox.len()) > MAX_ARCHIVE_ROWS
            || batch.cutoff_ms < 0
            || committed_at_ms < batch.cutoff_ms
        {
            return Err(RepositoryError::TelemetryArchiveInputInvalid);
        }
        let archive_id = archive_id.to_ascii_lowercase();
        let key = format!("telemetry_archive.v1.{archive_id}");
        let batch = batch.clone();
        let manifest_json = manifest_json.to_owned();
        self.store
            .run_low_priority_write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let existing: Option<String> = transaction
                    .query_row(
                        "SELECT value_text FROM app_meta WHERE key = ?1",
                        [key.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing) = existing {
                    if existing == manifest_json {
                        transaction.commit()?;
                        return Ok(TelemetryArchiveCommit {
                            activity_count: batch.activity.len(),
                            outbox_count: batch.outbox.len(),
                        });
                    }
                    return Err(RepositoryError::TelemetryArchiveConflict);
                }

                for outbox in &batch.outbox {
                    let eligible: bool = transaction.query_row(
                        "SELECT EXISTS(
                             SELECT 1
                             FROM notification_outbox outbox
                             WHERE outbox.id = ?1
                               AND outbox.state = ?2
                               AND outbox.updated_at_ms = ?3
                               AND outbox.updated_at_ms < ?4
                               AND NOT EXISTS (
                                   SELECT 1
                                   FROM attention_event_links link
                                   JOIN attention_states attention
                                     ON attention.id = link.attention_state_id
                                   WHERE link.activity_event_seq = outbox.activity_event_seq
                                     AND attention.state IN ('open', 'acknowledged')
                               )
                         )",
                        params![
                            outbox.id,
                            outbox.state,
                            outbox.updated_at_ms,
                            batch.cutoff_ms
                        ],
                        |row| row.get(0),
                    )?;
                    if !eligible {
                        return Err(RepositoryError::TelemetryArchiveConflict);
                    }
                    transaction.execute(
                        "DELETE FROM notification_delivery_attempts WHERE outbox_id = ?1",
                        [outbox.id.as_str()],
                    )?;
                    let affected = transaction.execute(
                        "DELETE FROM notification_outbox
                         WHERE id = ?1
                           AND state = ?2
                           AND updated_at_ms = ?3",
                        params![outbox.id, outbox.state, outbox.updated_at_ms],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::TelemetryArchiveConflict);
                    }
                }

                for activity in &batch.activity {
                    transaction.execute(
                        "DELETE FROM attention_event_links
                         WHERE activity_event_seq = ?1
                           AND attention_state_id IN (
                               SELECT id
                               FROM attention_states
                               WHERE state = 'resolved'
                           )",
                        [activity.seq],
                    )?;
                    let affected = transaction.execute(
                        "DELETE FROM activity_events
                         WHERE seq = ?1
                           AND occurred_at_ms = ?2
                           AND occurred_at_ms < ?3
                           AND NOT EXISTS (
                               SELECT 1
                               FROM attention_event_links
                               WHERE activity_event_seq = ?1
                           )
                           AND NOT EXISTS (
                               SELECT 1
                               FROM notification_outbox
                               WHERE activity_event_seq = ?1
                                 AND state IN ('pending', 'delivering')
                           )",
                        params![activity.seq, activity.occurred_at_ms, batch.cutoff_ms],
                    )?;
                    if affected != 1 {
                        return Err(RepositoryError::TelemetryArchiveConflict);
                    }
                }
                transaction.execute(
                    "INSERT INTO app_meta(key, value_text, updated_at_ms)
                     VALUES (?1, ?2, ?3)",
                    params![key, manifest_json, committed_at_ms],
                )?;
                transaction.commit()?;
                connection.execute_batch("PRAGMA incremental_vacuum(64);")?;
                Ok(TelemetryArchiveCommit {
                    activity_count: batch.activity.len(),
                    outbox_count: batch.outbox.len(),
                })
            })
            .map_err(RepositoryError::from)?
    }

    pub fn archive_record_count(&self) -> Result<usize, RepositoryError> {
        let reader = self.store.open_reader()?;
        let count: i64 = reader.connection().query_row(
            "SELECT COUNT(*) FROM app_meta
             WHERE key GLOB 'telemetry_archive.v1.*'",
            [],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| RepositoryError::TelemetryArchiveConflict)
    }
}
