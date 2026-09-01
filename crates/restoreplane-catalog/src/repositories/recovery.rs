use restoreplane_domain::{
    ConnectionProfileId, RecoveryPointId, RestoreRunId, VerificationRunId, WorkItemId,
};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use uuid::Uuid;

use crate::OperationalStore;

use super::RepositoryError;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RestorePurpose {
    #[default]
    IncidentRecovery,
    RestoreDrill,
}

impl RestorePurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::IncidentRecovery => "incident_recovery",
            Self::RestoreDrill => "restore_drill",
        }
    }
}

#[derive(Clone, Debug)]
pub struct SafeRestoreSpec {
    restore_run_id: RestoreRunId,
    work_item_id: WorkItemId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: String,
    target_connection_profile_id: ConnectionProfileId,
    target_database_name: String,
    target_fingerprint: String,
    preflight_version: String,
    eligibility_snapshot_json: String,
    purpose: RestorePurpose,
    created_at_ms: i64,
}

impl SafeRestoreSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        restore_run_id: RestoreRunId,
        work_item_id: WorkItemId,
        recovery_point_id: RecoveryPointId,
        source_replica_id: impl Into<String>,
        target_connection_profile_id: ConnectionProfileId,
        target_database_name: impl Into<String>,
        target_fingerprint: impl Into<String>,
        preflight_version: impl Into<String>,
        eligibility_snapshot_json: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            restore_run_id,
            work_item_id,
            recovery_point_id,
            source_replica_id: source_replica_id.into(),
            target_connection_profile_id,
            target_database_name: target_database_name.into(),
            target_fingerprint: target_fingerprint.into(),
            preflight_version: preflight_version.into(),
            eligibility_snapshot_json: eligibility_snapshot_json.into(),
            purpose: RestorePurpose::IncidentRecovery,
            created_at_ms,
        }
    }

    pub fn with_purpose(mut self, purpose: RestorePurpose) -> Self {
        self.purpose = purpose;
        self
    }

    pub fn restore_run_id(&self) -> &RestoreRunId {
        &self.restore_run_id
    }

    pub fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub fn recovery_point_id(&self) -> &RecoveryPointId {
        &self.recovery_point_id
    }

    pub fn source_replica_id(&self) -> &str {
        &self.source_replica_id
    }

    pub fn target_connection_profile_id(&self) -> &ConnectionProfileId {
        &self.target_connection_profile_id
    }

    pub fn target_database_name(&self) -> &str {
        &self.target_database_name
    }

    pub fn target_fingerprint(&self) -> &str {
        &self.target_fingerprint
    }

    pub fn preflight_version(&self) -> &str {
        &self.preflight_version
    }

    pub fn eligibility_snapshot_json(&self) -> &str {
        &self.eligibility_snapshot_json
    }

    pub const fn purpose(&self) -> RestorePurpose {
        self.purpose
    }

    pub fn verification_run_id(&self) -> Option<VerificationRunId> {
        (self.purpose == RestorePurpose::RestoreDrill).then(|| {
            VerificationRunId::from_stable_name(
                Uuid::NAMESPACE_URL,
                format!(
                    "verification:restore-drill:{}",
                    self.restore_run_id.as_str()
                )
                .as_bytes(),
            )
        })
    }

    pub const fn created_at_ms(&self) -> i64 {
        self.created_at_ms
    }

    pub(crate) fn validate(&self) -> Result<(), RepositoryError> {
        if self.source_replica_id.is_empty()
            || self.source_replica_id.len() > 256
            || invalid_text(&self.source_replica_id)
            || self.target_database_name.is_empty()
            || self.target_database_name.len() > 128
            || invalid_text(&self.target_database_name)
            || !is_digest(&self.target_fingerprint)
            || !is_digest(&self.preflight_version)
            || self.eligibility_snapshot_json.len() > 64 * 1024
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let snapshot: serde_json::Value = serde_json::from_str(&self.eligibility_snapshot_json)
            .map_err(|_| RepositoryError::SecurityInputInvalid)?;
        let snapshot_purpose = snapshot
            .get("purpose")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("incident_recovery");
        let valid_binding = snapshot
            .get("eligible")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
            && snapshot.get("mode").and_then(serde_json::Value::as_str) == Some("create_new")
            && snapshot
                .get("preflightVersion")
                .and_then(serde_json::Value::as_str)
                == Some(self.preflight_version.as_str())
            && snapshot
                .get("targetFingerprint")
                .and_then(serde_json::Value::as_str)
                == Some(self.target_fingerprint.as_str())
            && snapshot
                .get("targetConnectionId")
                .and_then(serde_json::Value::as_str)
                == Some(self.target_connection_profile_id.as_str())
            && snapshot
                .get("targetName")
                .and_then(serde_json::Value::as_str)
                == Some(self.target_database_name.as_str())
            && snapshot
                .pointer("/source/snapshotId")
                .and_then(serde_json::Value::as_str)
                == Some(self.recovery_point_id.as_str())
            && snapshot
                .pointer("/source/replicaId")
                .and_then(serde_json::Value::as_str)
                == Some(self.source_replica_id.as_str())
            && snapshot_purpose == self.purpose.as_str()
            && (self.purpose != RestorePurpose::RestoreDrill
                || (snapshot
                    .get("restoreTestTarget")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                    && snapshot
                        .get("targetRole")
                        .and_then(serde_json::Value::as_str)
                        == Some("restore_test")));
        if valid_binding {
            Ok(())
        } else {
            Err(RepositoryError::SecurityInputInvalid)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafeRestoreEnqueueResult {
    restore_run_id: RestoreRunId,
    work_item_id: WorkItemId,
    verification_run_id: Option<VerificationRunId>,
}

impl SafeRestoreEnqueueResult {
    pub fn restore_run_id(&self) -> &RestoreRunId {
        &self.restore_run_id
    }

    pub fn work_item_id(&self) -> &WorkItemId {
        &self.work_item_id
    }

    pub fn verification_run_id(&self) -> Option<&VerificationRunId> {
        self.verification_run_id.as_ref()
    }
}

#[derive(Clone)]
pub struct RecoveryRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn recovery(&self) -> RecoveryRepository {
        RecoveryRepository {
            store: self.clone(),
        }
    }
}

impl RecoveryRepository {
    pub fn enqueue_safe(
        &self,
        spec: SafeRestoreSpec,
    ) -> Result<SafeRestoreEnqueueResult, RepositoryError> {
        spec.validate()?;
        self.store
            .run_write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                insert_safe_restore_in_transaction(&transaction, &spec)?;
                let verification_run_id = spec.verification_run_id();
                transaction.commit()?;
                Ok(SafeRestoreEnqueueResult {
                    restore_run_id: spec.restore_run_id,
                    work_item_id: spec.work_item_id,
                    verification_run_id,
                })
            })
            .map_err(RepositoryError::from)?
    }
}

pub(super) fn insert_safe_restore_in_transaction(
    transaction: &Transaction<'_>,
    restore: &SafeRestoreSpec,
) -> Result<(), RepositoryError> {
    ensure_restore_source_target_in_transaction(
        transaction,
        &restore.recovery_point_id,
        &restore.source_replica_id,
        &restore.target_connection_profile_id,
    )?;
    transaction.execute(
        "INSERT INTO restore_runs(
             id, recovery_point_id, source_replica_id,
             target_connection_profile_id, target_database_name, target_fingerprint,
             purpose, conflict_policy, eligibility_snapshot_json, state,
             created_at_ms, updated_at_ms
         ) VALUES (
             ?1, ?2, ?3,
             ?4, ?5, ?6,
             ?7, 'create_new', ?8, 'queued',
             ?9, ?9
         )",
        params![
            restore.restore_run_id.as_str(),
            restore.recovery_point_id.as_str(),
            restore.source_replica_id,
            restore.target_connection_profile_id.as_str(),
            restore.target_database_name,
            restore.target_fingerprint,
            restore.purpose.as_str(),
            restore.eligibility_snapshot_json,
            restore.created_at_ms
        ],
    )?;
    transaction.execute(
        "INSERT INTO work_items(
             id, kind, restore_run_id, priority, state, available_at_ms,
             attempt_limit, created_at_ms, updated_at_ms
         ) VALUES (?1, 'restore', ?2, 500, 'queued', ?3, 3, ?3, ?3)",
        params![
            restore.work_item_id.as_str(),
            restore.restore_run_id.as_str(),
            restore.created_at_ms
        ],
    )?;
    if let Some(verification_run_id) = restore.verification_run_id() {
        transaction.execute(
            "INSERT INTO verification_runs(
                 id, recovery_point_id, source_replica_id, requested_level,
                 trigger_kind, state, restore_run_id, created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, 3,
                 'restore_drill', 'queued', ?4, ?5, ?5
             )",
            params![
                verification_run_id.as_str(),
                restore.recovery_point_id.as_str(),
                restore.source_replica_id,
                restore.restore_run_id.as_str(),
                restore.created_at_ms
            ],
        )?;
    }
    Ok(())
}

pub(super) fn ensure_restore_source_target_in_transaction(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: &str,
    target_connection_profile_id: &ConnectionProfileId,
) -> Result<(), RepositoryError> {
    let source: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT recovery.state, recovery.signature_status,
                    snapshot.restore_eligibility
             FROM catalog_recovery_points AS recovery
             JOIN catalog_snapshot_projection AS snapshot
               ON snapshot.recovery_point_id = recovery.id
             WHERE recovery.id = ?1",
            [recovery_point_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if !matches!(
        source.as_ref(),
        Some((state, signature, eligibility))
            if state == "complete"
                && signature == "valid"
                && matches!(eligibility.as_str(), "eligible" | "conditional")
    ) {
        return Err(RepositoryError::SecurityInputInvalid);
    }
    let replica_eligible: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM catalog_artifact_replicas
             WHERE id = ?1
               AND recovery_point_id = ?2
               AND requirement = 'required'
               AND state = 'committed'
               AND reader_status = 'eligible'
         )",
        params![source_replica_id, recovery_point_id.as_str()],
        |row| row.get(0),
    )?;
    let engine_match: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM connection_profiles AS target
             JOIN catalog_recovery_points AS source
               ON source.id = ?2
             WHERE target.id = ?1
               AND target.deleted_at_ms IS NULL
               AND target.state = 'ready'
               AND target.engine = source.engine
         )",
        params![
            target_connection_profile_id.as_str(),
            recovery_point_id.as_str()
        ],
        |row| row.get(0),
    )?;
    if !replica_eligible || !engine_match {
        return Err(RepositoryError::SecurityInputInvalid);
    }
    Ok(())
}

fn invalid_text(value: &str) -> bool {
    value.contains(['\0', '\r', '\n'])
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
