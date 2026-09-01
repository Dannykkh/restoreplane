use rusqlite::{params, OptionalExtension, TransactionBehavior};
use uuid::Uuid;

use crate::OperationalStore;

use super::RepositoryError;

#[derive(Clone, Debug)]
pub struct ActivityEventSpec {
    id: String,
    occurred_at_ms: i64,
    severity: &'static str,
    event_code: String,
    entity_type: String,
    entity_id: String,
    message: String,
}

impl ActivityEventSpec {
    pub fn info(
        id: impl Into<String>,
        occurred_at_ms: i64,
        event_code: impl Into<String>,
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            occurred_at_ms,
            "info",
            event_code,
            entity_type,
            entity_id,
            message,
        )
    }

    pub fn error(
        id: impl Into<String>,
        occurred_at_ms: i64,
        event_code: impl Into<String>,
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self::new(
            id,
            occurred_at_ms,
            "error",
            event_code,
            entity_type,
            entity_id,
            message,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        id: impl Into<String>,
        occurred_at_ms: i64,
        severity: &'static str,
        event_code: impl Into<String>,
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            occurred_at_ms,
            severity,
            event_code: event_code.into(),
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            message: message.into(),
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.id.is_empty()
            || self.id.len() > 128
            || self.occurred_at_ms < 0
            || self.event_code.is_empty()
            || self.event_code.len() > 128
            || self.entity_type.is_empty()
            || self.entity_type.len() > 128
            || self.entity_id.is_empty()
            || self.entity_id.len() > 512
            || self.message.is_empty()
            || self.message.len() > 16 * 1024
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityEventRecord {
    seq: i64,
    id: String,
    occurred_at_ms: i64,
}

impl ActivityEventRecord {
    pub const fn seq(&self) -> i64 {
        self.seq
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn occurred_at_ms(&self) -> i64 {
        self.occurred_at_ms
    }
}

#[derive(Clone, Debug)]
pub struct AttentionObservation {
    fingerprint: String,
    entity_type: String,
    entity_id: String,
    summary: String,
    activity_event_seq: i64,
    observed_at_ms: i64,
}

impl AttentionObservation {
    pub fn run_failure(
        fingerprint: impl Into<String>,
        entity_type: impl Into<String>,
        entity_id: impl Into<String>,
        summary: impl Into<String>,
        activity_event_seq: i64,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            fingerprint: fingerprint.into(),
            entity_type: entity_type.into(),
            entity_id: entity_id.into(),
            summary: summary.into(),
            activity_event_seq,
            observed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.fingerprint.is_empty()
            || self.fingerprint.len() > 512
            || self.entity_type.is_empty()
            || self.entity_type.len() > 128
            || self.entity_id.is_empty()
            || self.entity_id.len() > 512
            || self.summary.is_empty()
            || self.summary.len() > 4 * 1024
            || self.activity_event_seq <= 0
            || self.observed_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttentionRecord {
    id: String,
    state: String,
    occurrence_count: i64,
}

impl AttentionRecord {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub const fn occurrence_count(&self) -> i64 {
        self.occurrence_count
    }
}

#[derive(Clone)]
pub struct ActivityRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn activity(&self) -> ActivityRepository {
        ActivityRepository {
            store: self.clone(),
        }
    }
}

impl ActivityRepository {
    pub fn append_batch(
        &self,
        events: Vec<ActivityEventSpec>,
    ) -> Result<Vec<i64>, RepositoryError> {
        if events.is_empty() || events.len() > 128 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        for event in &events {
            event.validate()?;
        }
        self.store
            .run_low_priority_write(move |connection| {
                let transaction = connection.transaction()?;
                let mut sequences = Vec::with_capacity(events.len());
                for event in events {
                    transaction.execute(
                        "INSERT INTO activity_events(
                             id, occurred_at_ms, severity, event_code,
                             entity_type, entity_id, message, attributes_json,
                             redaction_version, created_at_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '{}', 1, ?2)",
                        params![
                            event.id,
                            event.occurred_at_ms,
                            event.severity,
                            event.event_code,
                            event.entity_type,
                            event.entity_id,
                            event.message
                        ],
                    )?;
                    sequences.push(transaction.last_insert_rowid());
                }
                transaction.commit()?;
                Ok(sequences)
            })
            .map_err(RepositoryError::from)?
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<ActivityEventRecord>, RepositoryError> {
        if limit == 0 || limit > 500 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let mut statement = reader.connection().prepare(
            "SELECT seq, id, occurred_at_ms
             FROM activity_events
             ORDER BY occurred_at_ms DESC, seq DESC
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit as i64], |row| {
            Ok(ActivityEventRecord {
                seq: row.get(0)?,
                id: row.get(1)?,
                occurred_at_ms: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::from)
    }

    pub fn observe_attention(
        &self,
        observation: AttentionObservation,
    ) -> Result<AttentionRecord, RepositoryError> {
        observation.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let active: Option<(String, i64)> = transaction
                .query_row(
                    "SELECT id, occurrence_count
                     FROM attention_states
                     WHERE fingerprint = ?1
                       AND state IN ('open', 'acknowledged')",
                    [observation.fingerprint.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (id, occurrence_count) = if let Some((id, current_count)) = active {
                let affected = transaction.execute(
                    "UPDATE attention_states
                     SET last_seen_at_ms = ?2, occurrence_count = occurrence_count + 1,
                         summary = ?3, updated_at_ms = ?2
                     WHERE id = ?1
                       AND state IN ('open', 'acknowledged')
                       AND occurrence_count = ?4",
                    params![
                        id,
                        observation.observed_at_ms,
                        observation.summary,
                        current_count
                    ],
                )?;
                if affected != 1 {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                (id, current_count + 1)
            } else {
                let id = Uuid::new_v4().to_string();
                transaction.execute(
                    "INSERT INTO attention_states(
                         id, fingerprint, category, severity, entity_type, entity_id,
                         state, summary, first_seen_at_ms, last_seen_at_ms,
                         occurrence_count, created_at_ms, updated_at_ms
                     ) VALUES (
                         ?1, ?2, 'run_failure', 'error', ?3, ?4,
                         'open', ?5, ?6, ?6,
                         1, ?6, ?6
                     )",
                    params![
                        id,
                        observation.fingerprint,
                        observation.entity_type,
                        observation.entity_id,
                        observation.summary,
                        observation.observed_at_ms
                    ],
                )?;
                (id, 1)
            };
            transaction.execute(
                "INSERT INTO attention_event_links(
                     attention_state_id, activity_event_seq, created_at_ms
                 ) VALUES (?1, ?2, ?3)
                 ON CONFLICT DO NOTHING",
                params![
                    id,
                    observation.activity_event_seq,
                    observation.observed_at_ms
                ],
            )?;
            let state: String = transaction.query_row(
                "SELECT state FROM attention_states WHERE id = ?1",
                [id.as_str()],
                |row| row.get(0),
            )?;
            transaction.commit()?;
            Ok(AttentionRecord {
                id,
                state,
                occurrence_count,
            })
        })
    }

    pub fn acknowledge_attention(
        &self,
        attention_id: &str,
        acknowledged_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if attention_id.is_empty() || acknowledged_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let attention_id = attention_id.to_owned();
        self.write(move |connection| {
            let affected = connection.execute(
                "UPDATE attention_states
                 SET state = 'acknowledged', acknowledged_at_ms = ?2, updated_at_ms = ?2
                 WHERE id = ?1 AND state = 'open'",
                params![attention_id, acknowledged_at_ms],
            )?;
            if affected != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            Ok(())
        })
    }

    pub fn resolve_attention(
        &self,
        attention_id: &str,
        resolution_code: &str,
        resolved_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if attention_id.is_empty()
            || resolution_code.is_empty()
            || resolution_code.len() > 128
            || resolved_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let attention_id = attention_id.to_owned();
        let resolution_code = resolution_code.to_owned();
        self.write(move |connection| {
            let affected = connection.execute(
                "UPDATE attention_states
                 SET state = 'resolved', resolved_at_ms = ?2,
                     resolution_code = ?3, updated_at_ms = ?2
                 WHERE id = ?1 AND state IN ('open', 'acknowledged')",
                params![attention_id, resolved_at_ms, resolution_code],
            )?;
            if affected != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            Ok(())
        })
    }

    pub fn attention_state(&self, attention_id: &str) -> Result<String, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT state FROM attention_states WHERE id = ?1",
                [attention_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}
