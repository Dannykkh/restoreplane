use std::str::FromStr;

use restoreplane_domain::{OperationRunId, RecoveryPointId, WorkItemId};
use restoreplane_pipeline::{RetentionDeleteAuthorizationV2, RetentionDispositionV2};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OperationalStore;

use super::ipc::{stage_retention_tombstone_in_transaction, RetentionTombstoneSpec};
use super::retention_planner::prepare_delete_plan_in_transaction;
use super::RepositoryError;

const MAX_MAINTENANCE_CANDIDATES: usize = 4_096;
const MAX_OPERATION_GENERATIONS: u32 = 10_000;
const RETENTION_CURSOR_KEY: &str = "retention.runtime.cursor.v1";
const RETENTION_LAST_SCAN_KEY: &str = "retention.runtime.last_scan.v1";

#[derive(Clone, Debug)]
pub struct RetentionMaintenanceSpec {
    now_ms: i64,
    grace_period_ms: i64,
    max_candidates: usize,
}

impl RetentionMaintenanceSpec {
    pub const fn new(now_ms: i64, grace_period_ms: i64, max_candidates: usize) -> Self {
        Self {
            now_ms,
            grace_period_ms,
            max_candidates,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.now_ms < 0
            || self.grace_period_ms <= 0
            || self.max_candidates == 0
            || self.max_candidates > MAX_MAINTENANCE_CANDIDATES
        {
            return Err(RepositoryError::RetentionAuthorityInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RetentionMaintenanceReport {
    candidates_evaluated: usize,
    preserved: usize,
    waiting_for_grace: usize,
    tombstones_staged: usize,
    deletes_enqueued: usize,
    already_active: usize,
    failures: usize,
}

impl RetentionMaintenanceReport {
    pub const fn candidates_evaluated(self) -> usize {
        self.candidates_evaluated
    }

    pub const fn preserved(self) -> usize {
        self.preserved
    }

    pub const fn waiting_for_grace(self) -> usize {
        self.waiting_for_grace
    }

    pub const fn tombstones_staged(self) -> usize {
        self.tombstones_staged
    }

    pub const fn deletes_enqueued(self) -> usize {
        self.deletes_enqueued
    }

    pub const fn already_active(self) -> usize {
        self.already_active
    }

    pub const fn failures(self) -> usize {
        self.failures
    }
}

#[derive(Clone)]
pub struct RetentionRuntimeRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn retention_runtime(&self) -> RetentionRuntimeRepository {
        RetentionRuntimeRepository {
            store: self.clone(),
        }
    }
}

impl RetentionRuntimeRepository {
    pub fn reconcile(
        &self,
        spec: RetentionMaintenanceSpec,
    ) -> Result<RetentionMaintenanceReport, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let candidates = load_candidates(&transaction, spec.max_candidates)?;
            let next_cursor = candidates.last().map(RetentionScanCursorV1::from);
            let mut report = RetentionMaintenanceReport::default();
            for candidate in candidates {
                report.candidates_evaluated += 1;
                let recovery_point_id =
                    match RecoveryPointId::from_str(&candidate.recovery_point_id) {
                        Ok(value) => value,
                        Err(_) => {
                            report.failures += 1;
                            continue;
                        }
                    };
                let plan = match prepare_delete_plan_in_transaction(
                    &transaction,
                    &recovery_point_id,
                    &candidate.destination_fingerprint,
                    spec.now_ms,
                    spec.grace_period_ms,
                ) {
                    Ok(plan) => plan,
                    Err(RepositoryError::RetentionAuthorityInvalid)
                    | Err(RepositoryError::RetentionTombstoneInvalid) => {
                        report.failures += 1;
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                match plan.decision().disposition {
                    RetentionDispositionV2::Preserve => report.preserved += 1,
                    RetentionDispositionV2::WaitForGrace => report.waiting_for_grace += 1,
                    RetentionDispositionV2::TombstoneRequired => {
                        let Some((operation_run_id, _generation)) = next_operation_identity(
                            &transaction,
                            &recovery_point_id,
                            &candidate.destination_fingerprint,
                            spec.grace_period_ms,
                        )?
                        else {
                            report.failures += 1;
                            continue;
                        };
                        let tombstone = RetentionTombstoneSpec::new(
                            operation_run_id,
                            recovery_point_id,
                            candidate.destination_fingerprint,
                            spec.grace_period_ms,
                            spec.now_ms,
                        );
                        tombstone.validate()?;
                        if stage_retention_tombstone_in_transaction(&transaction, &tombstone)? {
                            report.tombstones_staged += 1;
                        } else {
                            report.already_active += 1;
                        }
                    }
                    RetentionDispositionV2::DeleteEligible => {
                        match enqueue_automatic_delete(
                            &transaction,
                            &recovery_point_id,
                            &candidate.destination_fingerprint,
                            spec.now_ms,
                            spec.grace_period_ms,
                            plan.authorization(),
                        )? {
                            EnqueueDisposition::Created => report.deletes_enqueued += 1,
                            EnqueueDisposition::AlreadyActive => report.already_active += 1,
                            EnqueueDisposition::ManualAuthority => report.preserved += 1,
                        }
                    }
                }
            }
            if let Some(cursor) = next_cursor {
                store_cursor(&transaction, &cursor, spec.now_ms)?;
            }
            store_scan_heartbeat(&transaction, spec.now_ms)?;
            transaction.commit()?;
            Ok(report)
        })
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

#[derive(Clone)]
struct CandidateRow {
    committed_at_ms: i64,
    recovery_point_id: String,
    destination_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetentionScanCursorV1 {
    committed_at_ms: i64,
    destination_fingerprint: String,
    recovery_point_id: String,
    schema_version: u32,
}

impl From<&CandidateRow> for RetentionScanCursorV1 {
    fn from(candidate: &CandidateRow) -> Self {
        Self {
            committed_at_ms: candidate.committed_at_ms,
            destination_fingerprint: candidate.destination_fingerprint.clone(),
            recovery_point_id: candidate.recovery_point_id.clone(),
            schema_version: 1,
        }
    }
}

fn load_candidates(
    transaction: &Transaction<'_>,
    max_candidates: usize,
) -> Result<Vec<CandidateRow>, RepositoryError> {
    let cursor_json: Option<String> = transaction
        .query_row(
            "SELECT value_text FROM app_meta WHERE key = ?1",
            [RETENTION_CURSOR_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let cursor = cursor_json
        .as_deref()
        .map(serde_json::from_str::<RetentionScanCursorV1>)
        .transpose()
        .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    if cursor.as_ref().is_some_and(|value| {
        value.schema_version != 1
            || value.committed_at_ms < 0
            || RecoveryPointId::from_str(&value.recovery_point_id).is_err()
            || value.destination_fingerprint.split_once(':').is_none()
    }) {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    let candidates = load_candidates_after(transaction, cursor.as_ref(), max_candidates)?;
    if candidates.is_empty() && cursor.is_some() {
        return load_candidates_after(transaction, None, max_candidates);
    }
    Ok(candidates)
}

fn load_candidates_after(
    transaction: &Transaction<'_>,
    cursor: Option<&RetentionScanCursorV1>,
    max_candidates: usize,
) -> Result<Vec<CandidateRow>, RepositoryError> {
    let mut statement = transaction.prepare(
        "SELECT DISTINCT recovery.committed_at_ms, recovery.id,
                         replica.destination_fingerprint
         FROM catalog_recovery_points AS recovery
         JOIN backup_runs AS backup
           ON backup.id = recovery.origin_backup_run_id
          AND backup.recovery_point_id = recovery.id
          AND backup.plan_occurrence_id = recovery.origin_occurrence_id
          AND backup.source_fingerprint = recovery.source_target_identity
         JOIN backup_plan_revisions AS revision
           ON revision.id = backup.plan_revision_id
          AND revision.published_at_ms IS NOT NULL
         JOIN catalog_artifact_replicas AS replica
           ON replica.recovery_point_id = recovery.id
         WHERE recovery.state = 'complete'
           AND recovery.signature_status = 'valid'
           AND backup.state IN ('succeeded', 'partially_succeeded')
           AND replica.state IN ('committed', 'degraded')
           AND (
                ?1 IS NULL
                OR recovery.committed_at_ms > ?1
                OR (recovery.committed_at_ms = ?1 AND recovery.id > ?2)
                OR (recovery.committed_at_ms = ?1 AND recovery.id = ?2
                    AND replica.destination_fingerprint > ?3)
           )
         ORDER BY recovery.committed_at_ms, recovery.id, replica.destination_fingerprint
         LIMIT ?4",
    )?;
    let rows = statement.query_map(
        params![
            cursor.map(|value| value.committed_at_ms),
            cursor.map(|value| value.recovery_point_id.as_str()),
            cursor.map(|value| value.destination_fingerprint.as_str()),
            max_candidates as i64
        ],
        |row| {
            Ok(CandidateRow {
                committed_at_ms: row.get(0)?,
                recovery_point_id: row.get(1)?,
                destination_fingerprint: row.get(2)?,
            })
        },
    )?;
    let candidates = rows.collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    Ok(candidates)
}

fn store_cursor(
    transaction: &Transaction<'_>,
    cursor: &RetentionScanCursorV1,
    updated_at_ms: i64,
) -> Result<(), RepositoryError> {
    let value_text =
        serde_json::to_string(cursor).map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    transaction.execute(
        "INSERT INTO app_meta(key, value_text, updated_at_ms)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET
             value_text = excluded.value_text,
             updated_at_ms = excluded.updated_at_ms",
        params![RETENTION_CURSOR_KEY, value_text, updated_at_ms],
    )?;
    Ok(())
}

fn store_scan_heartbeat(
    transaction: &Transaction<'_>,
    updated_at_ms: i64,
) -> Result<(), RepositoryError> {
    transaction.execute(
        "INSERT INTO app_meta(key, value_text, updated_at_ms)
         VALUES (?1, '{\"schema_version\":1}', ?2)
         ON CONFLICT(key) DO UPDATE SET
             value_text = excluded.value_text,
             updated_at_ms = excluded.updated_at_ms",
        params![RETENTION_LAST_SCAN_KEY, updated_at_ms],
    )?;
    Ok(())
}

fn automatic_operation_id(
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
    grace_period_ms: i64,
    generation: u32,
) -> OperationRunId {
    OperationRunId::from_stable_name(
        recovery_point_id.as_uuid(),
        format!(
            "automatic-retention-policy-v1\0{destination_fingerprint}\0{grace_period_ms}\0{generation}"
        )
        .as_bytes(),
    )
}

fn next_operation_identity(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
    grace_period_ms: i64,
) -> Result<Option<(OperationRunId, u32)>, RepositoryError> {
    for generation in 0..MAX_OPERATION_GENERATIONS {
        let operation_run_id = automatic_operation_id(
            recovery_point_id,
            destination_fingerprint,
            grace_period_ms,
            generation,
        );
        let state: Option<String> = transaction
            .query_row(
                "SELECT state FROM operation_runs WHERE id = ?1",
                [operation_run_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        match state.as_deref() {
            None | Some("created") => return Ok(Some((operation_run_id, generation))),
            Some("queued") | Some("running") => return Ok(Some((operation_run_id, generation))),
            Some(_) => {}
        }
    }
    Ok(None)
}

fn find_active_automatic_operation(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
    grace_period_ms: i64,
) -> Result<Option<(OperationRunId, u32, String)>, RepositoryError> {
    for generation in 0..MAX_OPERATION_GENERATIONS {
        let operation_run_id = automatic_operation_id(
            recovery_point_id,
            destination_fingerprint,
            grace_period_ms,
            generation,
        );
        let state: Option<String> = transaction
            .query_row(
                "SELECT state FROM operation_runs WHERE id = ?1",
                [operation_run_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if state
            .as_deref()
            .is_some_and(|value| matches!(value, "created" | "queued" | "running"))
        {
            return Ok(Some((
                operation_run_id,
                generation,
                state.expect("checked"),
            )));
        }
        if state.is_none() {
            return Ok(None);
        }
    }
    Ok(None)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PublishedRetentionPolicySnapshotV1 {
    destination_fingerprint: String,
    plan_revision_id: String,
    published_at_ms: i64,
    retention_policy_json: String,
    schema_version: u32,
    timezone: String,
}

fn load_policy_snapshot(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
) -> Result<PublishedRetentionPolicySnapshotV1, RepositoryError> {
    let (_, destination_id) = destination_fingerprint
        .split_once(':')
        .ok_or(RepositoryError::RetentionAuthorityInvalid)?;
    transaction
        .query_row(
            "SELECT revision.id, revision.published_at_ms, revision.timezone,
                    COALESCE(membership.retention_override_json,
                             revision.retention_policy_json)
             FROM catalog_recovery_points AS recovery
             JOIN backup_runs AS backup
               ON backup.id = recovery.origin_backup_run_id
              AND backup.recovery_point_id = recovery.id
              AND backup.plan_occurrence_id = recovery.origin_occurrence_id
              AND backup.source_fingerprint = recovery.source_target_identity
             JOIN backup_plan_revisions AS revision
               ON revision.id = backup.plan_revision_id
              AND revision.published_at_ms IS NOT NULL
             JOIN plan_revision_destinations AS membership
               ON membership.plan_revision_id = revision.id
              AND membership.destination_id = ?3
             JOIN destinations AS destination
               ON destination.id = membership.destination_id
             WHERE recovery.id = ?1
               AND destination.kind || ':' || destination.id = ?2",
            params![
                recovery_point_id.as_str(),
                destination_fingerprint,
                destination_id
            ],
            |row| {
                Ok(PublishedRetentionPolicySnapshotV1 {
                    destination_fingerprint: destination_fingerprint.to_owned(),
                    plan_revision_id: row.get(0)?,
                    published_at_ms: row.get(1)?,
                    timezone: row.get(2)?,
                    retention_policy_json: row.get(3)?,
                    schema_version: 1,
                })
            },
        )
        .optional()?
        .ok_or(RepositoryError::RetentionAuthorityInvalid)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PolicyAuthorizationDocumentV1 {
    authority_kind: String,
    authorization_digest_blake3: String,
    destination_fingerprint: String,
    grace_period_ms: i64,
    operation_generation: u32,
    operation_run_id: String,
    policy: PublishedRetentionPolicySnapshotV1,
    recovery_point_id: String,
    schema_version: u32,
    tombstoned_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct PolicyAuthorizationEnvelopeV1 {
    authorization_digest_blake3: String,
    authorization_id: String,
    document: PolicyAuthorizationDocumentV1,
    schema_version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AutomaticRetentionDeleteRequestV3 {
    authorization: serde_json::Value,
    authorized_at_ms: i64,
    authority_kind: String,
    delete_secret_ref_id: Option<String>,
    destination_fingerprint: String,
    dry_run_digest_blake3: String,
    policy_authorization: PolicyAuthorizationEnvelopeV1,
    recovery_point_id: String,
    schema_version: u32,
    tombstone: serde_json::Value,
    tombstoned_at_ms: i64,
}

enum EnqueueDisposition {
    Created,
    AlreadyActive,
    ManualAuthority,
}

fn enqueue_automatic_delete(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
    now_ms: i64,
    grace_period_ms: i64,
    authorization: &RetentionDeleteAuthorizationV2,
) -> Result<EnqueueDisposition, RepositoryError> {
    let Some((operation_run_id, generation, state)) = find_active_automatic_operation(
        transaction,
        recovery_point_id,
        destination_fingerprint,
        grace_period_ms,
    )?
    else {
        return Ok(EnqueueDisposition::ManualAuthority);
    };
    if matches!(state.as_str(), "queued" | "running") {
        return Ok(EnqueueDisposition::AlreadyActive);
    }
    let tombstoned_at_ms = authorization
        .tombstoned_at_ms()
        .ok_or(RepositoryError::RetentionAuthorityInvalid)?;
    let tombstone = RetentionTombstoneSpec::new(
        operation_run_id.clone(),
        recovery_point_id.clone(),
        destination_fingerprint,
        grace_period_ms,
        tombstoned_at_ms,
    );
    tombstone.validate()?;
    if stage_retention_tombstone_in_transaction(transaction, &tombstone)? {
        return Err(RepositoryError::RetentionTombstoneInvalid);
    }
    let staged_tombstone_json: String = transaction.query_row(
        "SELECT request_json FROM operation_runs WHERE id = ?1 AND state = 'created'",
        [operation_run_id.as_str()],
        |row| row.get(0),
    )?;
    let staged_tombstone: serde_json::Value = serde_json::from_str(&staged_tombstone_json)
        .map_err(|_| RepositoryError::RetentionTombstoneInvalid)?;
    let authorization_json = authorization
        .to_json()
        .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    let authorization_value: serde_json::Value = serde_json::from_str(&authorization_json)
        .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    let policy = load_policy_snapshot(transaction, recovery_point_id, destination_fingerprint)?;
    let document = PolicyAuthorizationDocumentV1 {
        authority_kind: "published_retention_policy".to_owned(),
        authorization_digest_blake3: authorization.digest_blake3().to_owned(),
        destination_fingerprint: destination_fingerprint.to_owned(),
        grace_period_ms,
        operation_generation: generation,
        operation_run_id: operation_run_id.as_str().to_owned(),
        policy,
        recovery_point_id: recovery_point_id.as_str().to_owned(),
        schema_version: 1,
        tombstoned_at_ms,
    };
    let canonical =
        serde_json::to_vec(&document).map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    let document_digest = blake3::hash(&canonical).to_hex().to_string();
    let authorization_id =
        Uuid::new_v5(&operation_run_id.as_uuid(), document_digest.as_bytes()).to_string();
    let policy_authorization = PolicyAuthorizationEnvelopeV1 {
        authorization_digest_blake3: document_digest,
        authorization_id,
        document,
        schema_version: 1,
    };
    let request_json = serde_json::to_string(&serde_json::json!({
        "authorization": authorization_value,
        "authorized_at_ms": now_ms,
        "authority_kind": "published_retention_policy",
        "delete_secret_ref_id": authorization.delete_secret_ref_id(),
        "destination_fingerprint": destination_fingerprint,
        "dry_run_digest_blake3": authorization.digest_blake3(),
        "policy_authorization": policy_authorization,
        "recovery_point_id": recovery_point_id.as_str(),
        "schema_version": 3,
        "tombstone": staged_tombstone,
        "tombstoned_at_ms": tombstoned_at_ms,
    }))
    .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    let updated = transaction.execute(
        "UPDATE operation_runs
         SET request_json = ?2, state = 'queued', updated_at_ms = ?3
         WHERE id = ?1 AND state = 'created' AND request_json = ?4",
        params![
            operation_run_id.as_str(),
            request_json,
            now_ms,
            staged_tombstone_json
        ],
    )?;
    if updated != 1 {
        return Err(RepositoryError::RetentionTombstoneInvalid);
    }
    let work_item_id = WorkItemId::from_stable_name(
        operation_run_id.as_uuid(),
        b"automatic-retention-policy-v1\0work",
    );
    transaction.execute(
        "INSERT INTO work_items(
             id, kind, operation_run_id, priority, state,
             available_at_ms, attempt_limit, created_at_ms, updated_at_ms
         ) VALUES (?1, 'operation', ?2, 500, 'queued', ?3, 3, ?3, ?3)",
        params![work_item_id.as_str(), operation_run_id.as_str(), now_ms],
    )?;
    Ok(EnqueueDisposition::Created)
}

pub(super) fn validate_automatic_policy_authorization_in_transaction(
    transaction: &Transaction<'_>,
    operation_run_id: &str,
    subject_id: &str,
    request_json: &str,
    reevaluate_at_ms: Option<i64>,
) -> Result<RetentionDeleteAuthorizationV2, RepositoryError> {
    let request: AutomaticRetentionDeleteRequestV3 =
        serde_json::from_str(request_json).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let authorization_json = serde_json::to_string(&request.authorization)
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let stored_authorization = RetentionDeleteAuthorizationV2::from_json(&authorization_json)
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let policy = &request.policy_authorization;
    let canonical =
        serde_json::to_vec(&policy.document).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let expected_document_digest = blake3::hash(&canonical).to_hex().to_string();
    let expected_authorization_id = Uuid::new_v5(
        &Uuid::parse_str(operation_run_id).map_err(|_| RepositoryError::RuntimeStateInvalid)?,
        expected_document_digest.as_bytes(),
    )
    .to_string();
    let recovery_point_id =
        RecoveryPointId::from_str(subject_id).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let document = &policy.document;
    if request.schema_version != 3
        || request.authority_kind != "published_retention_policy"
        || request.authorized_at_ms != stored_authorization.evaluated_at_ms()
        || request.delete_secret_ref_id.as_deref() != stored_authorization.delete_secret_ref_id()
        || request.destination_fingerprint != stored_authorization.destination_fingerprint()
        || request.dry_run_digest_blake3 != stored_authorization.digest_blake3()
        || request.recovery_point_id != subject_id
        || request.tombstoned_at_ms != stored_authorization.tombstoned_at_ms().unwrap_or(-1)
        || policy.schema_version != 1
        || policy.authorization_digest_blake3 != expected_document_digest
        || policy.authorization_id != expected_authorization_id
        || document.schema_version != 1
        || document.authority_kind != "published_retention_policy"
        || document.authorization_digest_blake3 != stored_authorization.digest_blake3()
        || document.destination_fingerprint != request.destination_fingerprint
        || document.grace_period_ms != stored_authorization.grace_period_ms()
        || document.operation_run_id != operation_run_id
        || document.recovery_point_id != subject_id
        || document.tombstoned_at_ms != request.tombstoned_at_ms
        || automatic_operation_id(
            &recovery_point_id,
            &request.destination_fingerprint,
            document.grace_period_ms,
            document.operation_generation,
        )
        .as_str()
            != operation_run_id
        || load_policy_snapshot(
            transaction,
            &recovery_point_id,
            &request.destination_fingerprint,
        )? != document.policy
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let tombstone_recovery = request
        .tombstone
        .get("recovery_point_id")
        .and_then(serde_json::Value::as_str);
    let tombstone_destination = request
        .tombstone
        .get("destination_fingerprint")
        .and_then(serde_json::Value::as_str);
    let tombstone_at = request
        .tombstone
        .get("tombstoned_at_ms")
        .and_then(serde_json::Value::as_i64);
    let tombstone_grace = request
        .tombstone
        .get("grace_period_ms")
        .and_then(serde_json::Value::as_i64);
    if tombstone_recovery != Some(subject_id)
        || tombstone_destination != Some(request.destination_fingerprint.as_str())
        || tombstone_at != Some(request.tombstoned_at_ms)
        || tombstone_grace != Some(document.grace_period_ms)
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    if let Some(observed_at_ms) = reevaluate_at_ms {
        let current = prepare_delete_plan_in_transaction(
            transaction,
            &recovery_point_id,
            &request.destination_fingerprint,
            observed_at_ms,
            document.grace_period_ms,
        )?;
        if current.decision().disposition != RetentionDispositionV2::DeleteEligible
            || !stored_authorization
                .has_same_delete_scope(current.authorization())
                .map_err(|_| RepositoryError::RuntimeStateInvalid)?
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        return Ok(current.authorization().clone());
    }
    Ok(stored_authorization)
}
