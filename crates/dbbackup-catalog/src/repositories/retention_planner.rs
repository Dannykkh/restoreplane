use std::collections::HashSet;
use std::str::FromStr;

use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use dbbackup_domain::{DestinationId, RecoveryPointId};
use dbbackup_pipeline::{
    ImmutabilityKind, RetentionCandidateV2, RetentionDeletePlanV2, RetentionObjectRoleV2,
    RetentionObjectV2,
};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::OperationalStore;

use super::RepositoryError;

const MAX_POLICY_GENERATIONS: u32 = 10_000;

#[derive(Clone, Debug)]
pub struct RetentionSafetyControlSpec {
    recovery_point_id: RecoveryPointId,
    destination_fingerprint: String,
    pinned: bool,
    legal_hold: bool,
    updated_at_ms: i64,
}

impl RetentionSafetyControlSpec {
    pub fn new(
        recovery_point_id: RecoveryPointId,
        destination_fingerprint: impl Into<String>,
        pinned: bool,
        legal_hold: bool,
        updated_at_ms: i64,
    ) -> Self {
        Self {
            recovery_point_id,
            destination_fingerprint: destination_fingerprint.into(),
            pinned,
            legal_hold,
            updated_at_ms,
        }
    }

    pub(super) fn validate(&self) -> Result<(), RepositoryError> {
        parse_destination_fingerprint(&self.destination_fingerprint)?;
        if self.updated_at_ms < 0 {
            return Err(RepositoryError::RetentionAuthorityInvalid);
        }
        Ok(())
    }

    pub(super) const fn updated_at_ms(&self) -> i64 {
        self.updated_at_ms
    }

    pub(super) const fn recovery_point_id(&self) -> &RecoveryPointId {
        &self.recovery_point_id
    }
}

#[derive(Clone)]
pub struct RetentionPlannerRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn retention_planner(&self) -> RetentionPlannerRepository {
        RetentionPlannerRepository {
            store: self.clone(),
        }
    }
}

impl RetentionPlannerRepository {
    pub fn set_safety_control(
        &self,
        spec: RetentionSafetyControlSpec,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            apply_safety_control(&transaction, &spec)?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn prepare_delete_plan(
        &self,
        recovery_point_id: &RecoveryPointId,
        destination_fingerprint: &str,
        now_ms: i64,
        grace_period_ms: i64,
    ) -> Result<RetentionDeletePlanV2, RepositoryError> {
        if now_ms < 0 || grace_period_ms <= 0 {
            return Err(RepositoryError::RetentionAuthorityInvalid);
        }
        parse_destination_fingerprint(destination_fingerprint)?;
        let recovery_point_id = recovery_point_id.clone();
        let destination_fingerprint = destination_fingerprint.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let plan = prepare_delete_plan_in_transaction(
                &transaction,
                &recovery_point_id,
                &destination_fingerprint,
                now_ms,
                grace_period_ms,
            )?;
            transaction.commit()?;
            Ok(plan)
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

pub(super) fn prepare_delete_plan_in_transaction(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
    now_ms: i64,
    grace_period_ms: i64,
) -> Result<RetentionDeletePlanV2, RepositoryError> {
    if now_ms < 0 || grace_period_ms <= 0 {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    parse_destination_fingerprint(destination_fingerprint)?;
    let authority = load_candidate_authority(
        transaction,
        recovery_point_id,
        destination_fingerprint,
        now_ms,
    )?;
    let objects = load_catalog_objects(transaction, recovery_point_id, destination_fingerprint)?;
    let policy_expired = policy_expired(transaction, &authority, recovery_point_id, now_ms)?;
    let controls = load_safety_control(transaction, recovery_point_id, destination_fingerprint)?;
    let last_compliant = is_last_compliant(transaction, &authority, recovery_point_id, now_ms)?;
    let active_restore_source =
        is_active_restore_source(transaction, recovery_point_id, &authority.replica_id)?;
    let active_repair_source =
        is_active_repair_source(transaction, recovery_point_id, destination_fingerprint)?;
    let tombstoned_at_ms = load_tombstone(
        transaction,
        recovery_point_id,
        destination_fingerprint,
        authority.committed_at_ms,
        grace_period_ms,
        &objects,
    )?;
    let delete_secret_ref_id = delete_secret_ref(
        transaction,
        authority.destination_kind.as_str(),
        authority.delete_secret_ref_id.as_deref(),
    )?;
    let candidate = RetentionCandidateV2::new(
        recovery_point_id.as_uuid(),
        destination_fingerprint,
        authority.committed_at_ms,
        policy_expired,
    )
    .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?
    .with_pinned(controls.pinned)
    .with_legal_hold(controls.legal_hold)
    .with_immutability(authority.immutability)
    .with_retention_until_ms(authority.retention_until_ms)
    .with_last_compliant(last_compliant)
    .with_active_restore_source(active_restore_source)
    .with_active_repair_source(active_repair_source)
    .with_tombstoned_at_ms(tombstoned_at_ms);
    let pipeline_objects = objects
        .into_iter()
        .map(CatalogObject::into_pipeline)
        .collect::<Result<Vec<_>, _>>()?;
    RetentionDeletePlanV2::prepare_with_delete_secret_ref(
        &candidate,
        pipeline_objects,
        now_ms,
        grace_period_ms,
        delete_secret_ref_id.as_deref(),
    )
    .map_err(|_| RepositoryError::RetentionAuthorityInvalid)
}

pub(super) fn apply_safety_control(
    transaction: &Transaction<'_>,
    spec: &RetentionSafetyControlSpec,
) -> Result<(), RepositoryError> {
    let key = retention_control_key(&spec.recovery_point_id, &spec.destination_fingerprint);
    let document = RetentionSafetyControlDocumentV1 {
        destination_fingerprint: spec.destination_fingerprint.clone(),
        legal_hold: spec.legal_hold,
        pinned: spec.pinned,
        recovery_point_id: spec.recovery_point_id.as_str().to_owned(),
        schema_version: 1,
        updated_at_ms: spec.updated_at_ms,
    };
    let value_text =
        serde_json::to_string(&document).map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    let replica_exists: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM catalog_artifact_replicas
             WHERE recovery_point_id = ?1
               AND destination_fingerprint = ?2
         )",
        params![
            spec.recovery_point_id.as_str(),
            spec.destination_fingerprint
        ],
        |row| row.get(0),
    )?;
    if !replica_exists {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    let existing: Option<(String, i64)> = transaction
        .query_row(
            "SELECT value_text, updated_at_ms
             FROM app_meta
             WHERE key = ?1",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((existing_value, existing_updated_at_ms)) = existing {
        if existing_updated_at_ms > spec.updated_at_ms
            || (existing_updated_at_ms == spec.updated_at_ms && existing_value != value_text)
        {
            return Err(RepositoryError::RetentionAuthorityInvalid);
        }
        if existing_value == value_text {
            return Ok(());
        }
    }
    transaction.execute(
        "INSERT INTO app_meta(key, value_text, updated_at_ms)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET
             value_text = excluded.value_text,
             updated_at_ms = excluded.updated_at_ms",
        params![key, value_text, spec.updated_at_ms],
    )?;
    Ok(())
}

struct CandidateAuthority {
    committed_at_ms: i64,
    source_target_identity: String,
    plan_revision_id: String,
    database_target_id: String,
    timezone: String,
    retention_policy_json: String,
    replica_id: String,
    destination_fingerprint: String,
    immutability: ImmutabilityKind,
    retention_until_ms: Option<i64>,
    destination_kind: String,
    delete_secret_ref_id: Option<String>,
    protection_compliance: String,
}

fn load_candidate_authority(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
    now_ms: i64,
) -> Result<CandidateAuthority, RepositoryError> {
    let (expected_kind, destination_id) = parse_destination_fingerprint(destination_fingerprint)?;
    type CandidateAuthorityRow = (
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Option<i64>,
        String,
        String,
        Option<i64>,
        Option<String>,
        String,
    );
    let row: CandidateAuthorityRow = transaction
        .query_row(
            "SELECT
                 recovery.committed_at_ms,
                 recovery.source_target_identity,
                 backup.plan_revision_id,
                 backup.database_target_id,
                 revision.timezone,
                 COALESCE(membership.retention_override_json,
                          revision.retention_policy_json),
                 replica.id,
                 replica.destination_kind,
                 replica.immutability_capability,
                 replica.retention_until_ms,
                 destination.kind,
                 destination.state,
                 destination.deleted_at_ms,
                 delete_binding.secret_ref_id,
                 snapshot.protection_compliance
             FROM catalog_recovery_points AS recovery
             JOIN catalog_artifact_replicas AS replica
               ON replica.recovery_point_id = recovery.id
             JOIN backup_runs AS backup
               ON backup.id = recovery.origin_backup_run_id
              AND backup.recovery_point_id = recovery.id
              AND backup.plan_occurrence_id = recovery.origin_occurrence_id
              AND backup.source_fingerprint = recovery.source_target_identity
             JOIN backup_plan_revisions AS revision
               ON revision.id = backup.plan_revision_id
             JOIN plan_revision_destinations AS membership
               ON membership.plan_revision_id = revision.id
              AND membership.destination_id = ?3
             JOIN destinations AS destination
               ON destination.id = membership.destination_id
             LEFT JOIN destination_secret_bindings AS delete_binding
               ON delete_binding.destination_id = destination.id
              AND delete_binding.secret_role = 'delete_operator'
             JOIN catalog_snapshot_projection AS snapshot
               ON snapshot.recovery_point_id = recovery.id
             WHERE recovery.id = ?1
               AND replica.destination_fingerprint = ?2
               AND recovery.state = 'complete'
               AND recovery.signature_status = 'valid'
               AND backup.state IN ('succeeded', 'partially_succeeded')
               AND revision.published_at_ms IS NOT NULL
               AND replica.state IN ('committed', 'degraded')",
            params![
                recovery_point_id.as_str(),
                destination_fingerprint,
                destination_id.as_str()
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                    row.get(13)?,
                    row.get(14)?,
                ))
            },
        )
        .optional()?
        .ok_or(RepositoryError::RetentionAuthorityInvalid)?;
    if row.0 > now_ms
        || row.7 != expected_kind
        || row.10 != expected_kind
        || !matches!(row.11.as_str(), "ready" | "attention" | "disabled")
        || row.12.is_some()
        || !matches!(row.14.as_str(), "compliant" | "non_compliant" | "unknown")
    {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    Ok(CandidateAuthority {
        committed_at_ms: row.0,
        source_target_identity: row.1,
        plan_revision_id: row.2,
        database_target_id: row.3,
        timezone: row.4,
        retention_policy_json: row.5,
        replica_id: row.6,
        destination_fingerprint: destination_fingerprint.to_owned(),
        immutability: parse_immutability(&row.8)?,
        retention_until_ms: row.9,
        destination_kind: row.10,
        delete_secret_ref_id: row.13,
        protection_compliance: row.14,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GfsRetentionPolicyV1 {
    daily: u32,
    weekly: u32,
    monthly: u32,
}

fn policy_expired(
    transaction: &Transaction<'_>,
    authority: &CandidateAuthority,
    recovery_point_id: &RecoveryPointId,
    now_ms: i64,
) -> Result<bool, RepositoryError> {
    let policy: GfsRetentionPolicyV1 = serde_json::from_str(&authority.retention_policy_json)
        .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    if policy.daily > MAX_POLICY_GENERATIONS
        || policy.weekly > MAX_POLICY_GENERATIONS
        || policy.monthly > MAX_POLICY_GENERATIONS
        || policy.daily + policy.weekly + policy.monthly == 0
    {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    let timezone = Tz::from_str(&authority.timezone)
        .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    let mut statement = transaction.prepare(
        "SELECT recovery.id, recovery.committed_at_ms
         FROM catalog_recovery_points AS recovery
         JOIN backup_runs AS backup
           ON backup.id = recovery.origin_backup_run_id
          AND backup.recovery_point_id = recovery.id
          AND backup.plan_occurrence_id = recovery.origin_occurrence_id
          AND backup.source_fingerprint = recovery.source_target_identity
         JOIN catalog_artifact_replicas AS replica
           ON replica.recovery_point_id = recovery.id
         WHERE backup.plan_revision_id = ?1
           AND backup.database_target_id = ?2
           AND recovery.source_target_identity = ?3
           AND replica.destination_fingerprint = ?4
           AND recovery.state = 'complete'
           AND recovery.signature_status = 'valid'
           AND backup.state IN ('succeeded', 'partially_succeeded')
           AND replica.state IN ('committed', 'degraded')
           AND recovery.committed_at_ms <= ?5
         ORDER BY recovery.committed_at_ms DESC, recovery.id DESC",
    )?;
    let rows = statement.query_map(
        params![
            authority.plan_revision_id,
            authority.database_target_id,
            authority.source_target_identity,
            authority.destination_fingerprint,
            now_ms
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let candidates = rows.collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if !candidates
        .iter()
        .any(|(id, _)| id == recovery_point_id.as_str())
    {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    let retained = retained_gfs_generations(&candidates, timezone, &policy)?;
    Ok(!retained.contains(recovery_point_id.as_str()))
}

fn retained_gfs_generations(
    candidates: &[(String, i64)],
    timezone: Tz,
    policy: &GfsRetentionPolicyV1,
) -> Result<HashSet<String>, RepositoryError> {
    let mut retained = HashSet::new();
    let mut daily = HashSet::new();
    let mut weekly = HashSet::new();
    let mut monthly = HashSet::new();
    for (recovery_point_id, committed_at_ms) in candidates {
        let utc = DateTime::<Utc>::from_timestamp_millis(*committed_at_ms)
            .ok_or(RepositoryError::RetentionAuthorityInvalid)?;
        let local = utc.with_timezone(&timezone);
        let daily_key = (local.year(), local.ordinal());
        if daily.insert(daily_key) && daily.len() <= policy.daily as usize {
            retained.insert(recovery_point_id.clone());
        }
        let iso_week = local.iso_week();
        let weekly_key = (iso_week.year(), iso_week.week());
        if weekly.insert(weekly_key) && weekly.len() <= policy.weekly as usize {
            retained.insert(recovery_point_id.clone());
        }
        let monthly_key = (local.year(), local.month());
        if monthly.insert(monthly_key) && monthly.len() <= policy.monthly as usize {
            retained.insert(recovery_point_id.clone());
        }
    }
    Ok(retained)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct CatalogObject {
    key: String,
    role: String,
    version: Option<String>,
}

impl CatalogObject {
    fn into_pipeline(self) -> Result<RetentionObjectV2, RepositoryError> {
        let role = match self.role.as_str() {
            "payload" => RetentionObjectRoleV2::Payload,
            "part_manifest" => RetentionObjectRoleV2::PartManifest,
            "set_manifest" => RetentionObjectRoleV2::SetManifest,
            "verification_evidence" => RetentionObjectRoleV2::VerificationEvidence,
            _ => return Err(RepositoryError::RetentionAuthorityInvalid),
        };
        match self.version {
            Some(version) => RetentionObjectV2::versioned(self.key, version, role),
            None => RetentionObjectV2::new(self.key, role),
        }
        .map_err(|_| RepositoryError::RetentionAuthorityInvalid)
    }
}

fn load_catalog_objects(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
) -> Result<Vec<CatalogObject>, RepositoryError> {
    let mut statement = transaction.prepare(
        "SELECT object.object_key, object.object_version, object.object_role
         FROM catalog_stored_objects AS object
         JOIN catalog_artifact_replicas AS replica
           ON replica.id = object.artifact_replica_id
          AND replica.recovery_point_id = object.recovery_point_id
         WHERE object.recovery_point_id = ?1
           AND replica.destination_fingerprint = ?2
           AND object.state = 'present'
         ORDER BY CASE object.object_role
                      WHEN 'set_manifest' THEN 0
                      WHEN 'part_manifest' THEN 1
                      WHEN 'verification_evidence' THEN 2
                      WHEN 'payload' THEN 3
                      ELSE 4
                  END,
                  object.object_key,
                  object.object_version",
    )?;
    let rows = statement.query_map(
        params![recovery_point_id.as_str(), destination_fingerprint],
        |row| {
            Ok(CatalogObject {
                key: row.get(0)?,
                version: row.get(1)?,
                role: row.get(2)?,
            })
        },
    )?;
    let objects = rows.collect::<Result<Vec<_>, _>>()?;
    if objects.is_empty()
        || objects
            .iter()
            .filter(|object| object.role == "set_manifest")
            .count()
            != 1
    {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    Ok(objects)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetentionSafetyControlDocumentV1 {
    destination_fingerprint: String,
    legal_hold: bool,
    pinned: bool,
    recovery_point_id: String,
    schema_version: u32,
    updated_at_ms: i64,
}

fn load_safety_control(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
) -> Result<RetentionSafetyControlDocumentV1, RepositoryError> {
    let key = retention_control_key(recovery_point_id, destination_fingerprint);
    let row: Option<(String, i64)> = transaction
        .query_row(
            "SELECT value_text, updated_at_ms
             FROM app_meta
             WHERE key = ?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((value_text, updated_at_ms)) = row else {
        return Ok(RetentionSafetyControlDocumentV1 {
            destination_fingerprint: destination_fingerprint.to_owned(),
            legal_hold: false,
            pinned: false,
            recovery_point_id: recovery_point_id.as_str().to_owned(),
            schema_version: 1,
            updated_at_ms: 0,
        });
    };
    let document: RetentionSafetyControlDocumentV1 = serde_json::from_str(&value_text)
        .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    if document.schema_version != 1
        || document.recovery_point_id != recovery_point_id.as_str()
        || document.destination_fingerprint != destination_fingerprint
        || document.updated_at_ms != updated_at_ms
    {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    Ok(document)
}

fn retention_control_key(
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
) -> String {
    format!(
        "retention-control:v1:{}:{}",
        recovery_point_id.as_str(),
        blake3::hash(destination_fingerprint.as_bytes()).to_hex()
    )
}

fn is_last_compliant(
    transaction: &Transaction<'_>,
    authority: &CandidateAuthority,
    recovery_point_id: &RecoveryPointId,
    now_ms: i64,
) -> Result<bool, RepositoryError> {
    if authority.protection_compliance != "compliant" {
        return Ok(false);
    }
    let other_exists: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM catalog_recovery_points AS recovery
             JOIN catalog_snapshot_projection AS snapshot
               ON snapshot.recovery_point_id = recovery.id
             WHERE recovery.source_target_identity = ?1
               AND recovery.id <> ?2
               AND recovery.state = 'complete'
               AND recovery.signature_status = 'valid'
               AND recovery.committed_at_ms <= ?3
               AND snapshot.protection_compliance = 'compliant'
         )",
        params![
            authority.source_target_identity,
            recovery_point_id.as_str(),
            now_ms
        ],
        |row| row.get(0),
    )?;
    Ok(!other_exists)
}

fn is_active_restore_source(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    replica_id: &str,
) -> Result<bool, RepositoryError> {
    transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM restore_runs
                 WHERE recovery_point_id = ?1
                   AND state IN ('created', 'queued', 'running', 'validating')
                   AND (source_replica_id IS NULL OR source_replica_id = ?2)
             )",
            params![recovery_point_id.as_str(), replica_id],
            |row| row.get(0),
        )
        .map_err(RepositoryError::from)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaRepairRequestProbe {
    recovery_point_id: String,
    schema_version: u32,
    #[serde(default)]
    source_reader_secret_ref_id: Option<String>,
    source_destination_fingerprint: String,
    #[serde(default)]
    target_reader_secret_ref_id: Option<String>,
    target_destination_fingerprint: String,
    #[serde(default)]
    target_writer_secret_ref_id: Option<String>,
}

fn is_active_repair_source(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
) -> Result<bool, RepositoryError> {
    let mut statement = transaction.prepare(
        "SELECT request_json
         FROM operation_runs
         WHERE operation_kind = 'replica_repair'
           AND subject_type = 'recovery_point'
           AND subject_id = ?1
           AND state IN ('created', 'queued', 'running')",
    )?;
    let rows = statement.query_map([recovery_point_id.as_str()], |row| row.get::<_, String>(0))?;
    let requests = rows.collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for request_json in requests {
        let value: serde_json::Value = serde_json::from_str(&request_json)
            .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
        let field_count = value
            .as_object()
            .map(serde_json::Map::len)
            .ok_or(RepositoryError::RetentionAuthorityInvalid)?;
        let request: ReplicaRepairRequestProbe = serde_json::from_value(value)
            .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
        if request.recovery_point_id != recovery_point_id.as_str()
            || request.source_destination_fingerprint.trim().is_empty()
            || request.target_destination_fingerprint.trim().is_empty()
            || request.source_destination_fingerprint == request.target_destination_fingerprint
            || !match request.schema_version {
                1 => {
                    field_count == 4
                        && request.source_reader_secret_ref_id.is_none()
                        && request.target_reader_secret_ref_id.is_none()
                        && request.target_writer_secret_ref_id.is_none()
                }
                2 => {
                    field_count == 7
                        && valid_optional_uuid(request.source_reader_secret_ref_id.as_deref())
                        && valid_optional_uuid(request.target_reader_secret_ref_id.as_deref())
                        && valid_optional_uuid(request.target_writer_secret_ref_id.as_deref())
                }
                _ => false,
            }
        {
            return Err(RepositoryError::RetentionAuthorityInvalid);
        }
        if request.source_destination_fingerprint == destination_fingerprint {
            return Ok(true);
        }
    }
    Ok(false)
}

fn valid_optional_uuid(value: Option<&str>) -> bool {
    value.is_none_or(|value| {
        Uuid::parse_str(value)
            .map(|parsed| parsed.to_string() == value)
            .unwrap_or(false)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionTombstoneDocumentProbe {
    destination_fingerprint: String,
    grace_period_ms: i64,
    objects: Vec<CatalogObject>,
    phase: String,
    recovery_point_id: String,
    schema_version: u32,
    tombstoned_at_ms: i64,
}

fn load_tombstone(
    transaction: &Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
    committed_at_ms: i64,
    grace_period_ms: i64,
    current_objects: &[CatalogObject],
) -> Result<Option<i64>, RepositoryError> {
    let mut statement = transaction.prepare(
        "SELECT request_json
         FROM operation_runs
         WHERE operation_kind = 'retention_delete'
           AND subject_type = 'artifact_replica'
           AND subject_id = ?1
           AND state IN ('created', 'queued', 'running')",
    )?;
    let rows = statement.query_map([recovery_point_id.as_str()], |row| row.get::<_, String>(0))?;
    let requests = rows.collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut matching = None;
    for request_json in requests {
        let request_value: serde_json::Value = serde_json::from_str(&request_json)
            .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
        let tombstone_value = if request_value
            .get("phase")
            .and_then(serde_json::Value::as_str)
            == Some("tombstoned")
        {
            request_value
        } else {
            request_value
                .get("tombstone")
                .cloned()
                .ok_or(RepositoryError::RetentionAuthorityInvalid)?
        };
        let document: RetentionTombstoneDocumentProbe = serde_json::from_value(tombstone_value)
            .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
        if document.schema_version != 1
            || document.phase != "tombstoned"
            || document.recovery_point_id != recovery_point_id.as_str()
            || document.grace_period_ms <= 0
            || document.tombstoned_at_ms < committed_at_ms
        {
            return Err(RepositoryError::RetentionAuthorityInvalid);
        }
        if document.destination_fingerprint != destination_fingerprint {
            continue;
        }
        if document.grace_period_ms != grace_period_ms
            || document.objects != current_objects
            || matching.replace(document.tombstoned_at_ms).is_some()
        {
            return Err(RepositoryError::RetentionAuthorityInvalid);
        }
    }
    Ok(matching)
}

fn delete_secret_ref(
    transaction: &Transaction<'_>,
    destination_kind: &str,
    secret_ref_id: Option<&str>,
) -> Result<Option<String>, RepositoryError> {
    match destination_kind {
        "local" if secret_ref_id.is_none() => Ok(None),
        "s3" | "ssh_sftp" => {
            let secret_ref_id = secret_ref_id.ok_or(RepositoryError::RetentionAuthorityInvalid)?;
            if !valid_optional_uuid(Some(secret_ref_id)) {
                return Err(RepositoryError::RetentionAuthorityInvalid);
            }
            let valid: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM secret_refs
                     WHERE id = ?1
                       AND purpose = 'retention_delete'
                       AND state = 'active'
                 )",
                [secret_ref_id],
                |row| row.get(0),
            )?;
            if !valid {
                return Err(RepositoryError::RetentionAuthorityInvalid);
            }
            Ok(Some(secret_ref_id.to_owned()))
        }
        _ => Err(RepositoryError::RetentionAuthorityInvalid),
    }
}

fn parse_destination_fingerprint(value: &str) -> Result<(&str, DestinationId), RepositoryError> {
    let (kind, destination_id) = value
        .split_once(':')
        .ok_or(RepositoryError::RetentionAuthorityInvalid)?;
    if value.len() > 512 || !matches!(kind, "local" | "s3" | "ssh_sftp") {
        return Err(RepositoryError::RetentionAuthorityInvalid);
    }
    let destination_id = DestinationId::from_str(destination_id)
        .map_err(|_| RepositoryError::RetentionAuthorityInvalid)?;
    Ok((kind, destination_id))
}

fn parse_immutability(value: &str) -> Result<ImmutabilityKind, RepositoryError> {
    match value {
        "none" => Ok(ImmutabilityKind::None),
        "governance" => Ok(ImmutabilityKind::ObjectLockGovernance),
        "compliance" => Ok(ImmutabilityKind::ObjectLockCompliance),
        "server_append_only" => Ok(ImmutabilityKind::ServerAppendOnly),
        "unknown" => Err(RepositoryError::RetentionAuthorityInvalid),
        _ => Err(RepositoryError::RetentionAuthorityInvalid),
    }
}

#[cfg(test)]
mod tests {
    use chrono::DateTime;

    use super::*;

    #[test]
    fn gfs_generation_selection_uses_plan_timezone_and_newest_per_bucket() {
        let candidates = vec![
            (
                "february-newest".to_owned(),
                timestamp("2026-02-01T00:30:00+09:00"),
            ),
            (
                "january-newest".to_owned(),
                timestamp("2026-01-31T23:30:00+09:00"),
            ),
            (
                "january-older".to_owned(),
                timestamp("2026-01-01T00:30:00+09:00"),
            ),
        ];
        let retained = retained_gfs_generations(
            &candidates,
            "Asia/Seoul".parse().expect("IANA timezone"),
            &GfsRetentionPolicyV1 {
                daily: 0,
                weekly: 0,
                monthly: 2,
            },
        )
        .expect("select GFS generations");

        assert_eq!(
            retained,
            HashSet::from(["february-newest".to_owned(), "january-newest".to_owned()])
        );
    }

    fn timestamp(value: &str) -> i64 {
        DateTime::parse_from_rfc3339(value)
            .expect("RFC 3339 timestamp")
            .timestamp_millis()
    }
}
