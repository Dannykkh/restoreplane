use restoreplane_core::{RequestedVerificationLevel, VerificationPolicy, VerificationSchedule};
use restoreplane_domain::{RecoveryPointId, RestoreRunId, VerificationRunId, WorkItemId};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use uuid::Uuid;

use crate::OperationalStore;

use super::{catalog_projection::project_terminal_verification_run, RepositoryError};

type ExistingVerificationRunRow = (
    String,
    Option<String>,
    i64,
    String,
    Option<String>,
    String,
    Option<String>,
);

#[derive(Clone, Debug)]
pub struct VerificationWorkSpec {
    verification_run_id: VerificationRunId,
    work_item_id: WorkItemId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: Option<String>,
    requested_level: i64,
    trigger_kind: &'static str,
    restore_run_id: Option<RestoreRunId>,
    available_at_ms: i64,
    priority: i64,
    attempt_limit: i64,
}

impl VerificationWorkSpec {
    pub fn post_backup(
        verification_run_id: VerificationRunId,
        work_item_id: WorkItemId,
        recovery_point_id: RecoveryPointId,
        requested_level: i64,
        available_at_ms: i64,
    ) -> Self {
        Self {
            verification_run_id,
            work_item_id,
            recovery_point_id,
            source_replica_id: None,
            requested_level,
            trigger_kind: "post_backup",
            restore_run_id: None,
            available_at_ms,
            priority: 100,
            attempt_limit: 3,
        }
    }

    pub fn post_backup_with_restore(
        verification_run_id: VerificationRunId,
        work_item_id: WorkItemId,
        recovery_point_id: RecoveryPointId,
        restore_run_id: RestoreRunId,
        available_at_ms: i64,
    ) -> Self {
        let mut spec = Self::post_backup(
            verification_run_id,
            work_item_id,
            recovery_point_id,
            3,
            available_at_ms,
        );
        spec.restore_run_id = Some(restore_run_id);
        spec
    }

    pub fn manual(
        verification_run_id: VerificationRunId,
        work_item_id: WorkItemId,
        recovery_point_id: RecoveryPointId,
        source_replica_id: Option<String>,
        requested_level: i64,
        available_at_ms: i64,
    ) -> Self {
        Self {
            verification_run_id,
            work_item_id,
            recovery_point_id,
            source_replica_id,
            requested_level,
            trigger_kind: "manual",
            restore_run_id: None,
            available_at_ms,
            priority: 200,
            attempt_limit: 3,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !(1..=3).contains(&self.requested_level)
            || self.available_at_ms < 0
            || !(0..=1_000).contains(&self.priority)
            || self.attempt_limit <= 0
            || self.attempt_limit > 100
            || !matches!(
                self.trigger_kind,
                "post_backup" | "scheduled" | "manual" | "restore_drill"
            )
            || self.source_replica_id.as_deref().is_some_and(invalid_text)
        {
            return Err(RepositoryError::VerificationInputInvalid);
        }
        if self.requested_level == 3 && self.restore_run_id.is_none() {
            return Err(RepositoryError::VerificationRestoreRunRequired);
        }
        if self.requested_level < 3 && self.restore_run_id.is_some() {
            return Err(RepositoryError::VerificationInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct UnsupportedVerificationSpec {
    verification_run_id: VerificationRunId,
    recovery_point_id: RecoveryPointId,
    source_replica_id: Option<String>,
    requested_level: i64,
    trigger_kind: &'static str,
    restore_run_id: Option<RestoreRunId>,
    error_code: String,
    observed_at_ms: i64,
}

impl UnsupportedVerificationSpec {
    pub fn post_backup(
        verification_run_id: VerificationRunId,
        recovery_point_id: RecoveryPointId,
        requested_level: i64,
        error_code: impl Into<String>,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            verification_run_id,
            recovery_point_id,
            source_replica_id: None,
            requested_level,
            trigger_kind: "post_backup",
            restore_run_id: None,
            error_code: error_code.into(),
            observed_at_ms,
        }
    }

    pub fn manual(
        verification_run_id: VerificationRunId,
        recovery_point_id: RecoveryPointId,
        source_replica_id: Option<String>,
        requested_level: i64,
        error_code: impl Into<String>,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            verification_run_id,
            recovery_point_id,
            source_replica_id,
            requested_level,
            trigger_kind: "manual",
            restore_run_id: None,
            error_code: error_code.into(),
            observed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !(1..=3).contains(&self.requested_level)
            || self.observed_at_ms < 0
            || self.error_code.is_empty()
            || self.error_code.len() > 128
            || !self
                .error_code
                .bytes()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
            || self.source_replica_id.as_deref().is_some_and(invalid_text)
        {
            return Err(RepositoryError::VerificationInputInvalid);
        }
        if self.requested_level < 3 && self.restore_run_id.is_some() {
            return Err(RepositoryError::VerificationInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationEnqueueResult {
    verification_run_id: VerificationRunId,
    work_item_id: Option<WorkItemId>,
    created: bool,
}

impl VerificationEnqueueResult {
    pub fn verification_run_id(&self) -> &VerificationRunId {
        &self.verification_run_id
    }

    pub fn work_item_id(&self) -> Option<&WorkItemId> {
        self.work_item_id.as_ref()
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PostBackupVerificationDisposition {
    Queued,
    Unsupported,
    NotDue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostBackupVerificationOutcome {
    disposition: PostBackupVerificationDisposition,
    requested_level: Option<i64>,
    verification_run_id: Option<VerificationRunId>,
    work_item_id: Option<WorkItemId>,
    reason_code: Option<String>,
    outbox_intents_created: usize,
}

impl PostBackupVerificationOutcome {
    pub const fn disposition(&self) -> PostBackupVerificationDisposition {
        self.disposition
    }

    pub const fn requested_level(&self) -> Option<i64> {
        self.requested_level
    }

    pub fn verification_run_id(&self) -> Option<&VerificationRunId> {
        self.verification_run_id.as_ref()
    }

    pub fn work_item_id(&self) -> Option<&WorkItemId> {
        self.work_item_id.as_ref()
    }

    pub fn reason_code(&self) -> Option<&str> {
        self.reason_code.as_deref()
    }

    pub const fn outbox_intents_created(&self) -> usize {
        self.outbox_intents_created
    }
}

#[derive(Clone)]
pub struct VerificationRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn verification(&self) -> VerificationRepository {
        VerificationRepository {
            store: self.clone(),
        }
    }
}

impl VerificationRepository {
    pub fn enqueue(
        &self,
        spec: VerificationWorkSpec,
    ) -> Result<VerificationEnqueueResult, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = enqueue_in_transaction(&transaction, &spec)?;
            transaction.commit()?;
            Ok(result)
        })
    }

    pub fn record_unsupported(
        &self,
        spec: UnsupportedVerificationSpec,
    ) -> Result<VerificationEnqueueResult, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = record_unsupported_in_transaction(&transaction, &spec)?;
            transaction.commit()?;
            Ok(result)
        })
    }

    pub fn schedule_post_backup(
        &self,
        recovery_point_id: &RecoveryPointId,
        observed_at_ms: i64,
    ) -> Result<PostBackupVerificationOutcome, RepositoryError> {
        if observed_at_ms < 0 {
            return Err(RepositoryError::VerificationInputInvalid);
        }
        let recovery_point_id = recovery_point_id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let outcome = schedule_post_backup_in_transaction(
                &transaction,
                &recovery_point_id,
                observed_at_ms,
            )?;
            transaction.commit()?;
            Ok(outcome)
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

pub(super) fn schedule_post_backup_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    observed_at_ms: i64,
) -> Result<PostBackupVerificationOutcome, RepositoryError> {
    ensure_recovery_point_eligible(transaction, recovery_point_id)?;
    let context: Option<(String, String, String, String)> = transaction
        .query_row(
            "SELECT
                 revisions.verification_policy_json,
                 backup_runs.engine,
                 revisions.backup_plan_id,
                 recovery_points.source_target_identity
             FROM catalog_recovery_points AS recovery_points
             JOIN backup_runs
               ON backup_runs.id = recovery_points.origin_backup_run_id
             JOIN backup_plan_revisions AS revisions
               ON revisions.id = backup_runs.plan_revision_id
             WHERE recovery_points.id = ?1",
            [recovery_point_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((policy_json, engine, plan_id, source_target_identity)) = context else {
        return Ok(not_due_outcome("VERIFICATION_POLICY_NOT_LOCAL"));
    };

    let policy = match VerificationPolicy::from_json(&policy_json) {
        Ok(policy) => policy,
        Err(error) => {
            let reason = error.code();
            let outbox_intents_created = record_activity_and_outbox(
                transaction,
                &plan_id,
                recovery_point_id,
                None,
                None,
                "verification.policy_invalid",
                "error",
                Some(reason),
                observed_at_ms,
            )?;
            return Ok(PostBackupVerificationOutcome {
                disposition: PostBackupVerificationDisposition::Unsupported,
                requested_level: None,
                verification_run_id: None,
                work_item_id: None,
                reason_code: Some(reason.to_owned()),
                outbox_intents_created,
            });
        }
    };
    let requested_level = i64::from(policy.requested_level().as_u8());
    match policy.schedule() {
        VerificationSchedule::EveryRun => {}
        VerificationSchedule::Weekly => {
            let cutoff = observed_at_ms.saturating_sub(7 * 24 * 60 * 60 * 1_000);
            let recent_exists: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM verification_runs AS verification
                     JOIN catalog_recovery_points AS prior
                       ON prior.id = verification.recovery_point_id
                     WHERE prior.source_target_identity = ?1
                       AND verification.requested_level = ?2
                       AND verification.created_at_ms >= ?3
                       AND verification.state IN ('queued', 'running', 'passed')
                 )",
                params![source_target_identity, requested_level, cutoff],
                |row| row.get(0),
            )?;
            if recent_exists {
                return Ok(not_due_outcome("VERIFICATION_WEEKLY_NOT_DUE"));
            }
        }
        VerificationSchedule::Cron(_) => {
            return Ok(not_due_outcome("VERIFICATION_CRON_NOT_DUE"));
        }
    }

    let capability = match policy.requested_level() {
        RequestedVerificationLevel::L1 => "verify_l1",
        RequestedVerificationLevel::L2 => "verify_l2",
        RequestedVerificationLevel::L3 => "restore_l3",
    };
    let support_status: Option<String> = transaction
        .query_row(
            "SELECT capabilities.support_status
             FROM engine_certifications AS certifications
             JOIN engine_certification_capabilities AS capabilities
               ON capabilities.certification_id = certifications.id
             WHERE certifications.engine = ?1
               AND capabilities.capability = ?2
               AND certifications.status = 'certified'
               AND certifications.certified_at_ms <= ?3
               AND (certifications.expires_at_ms IS NULL OR certifications.expires_at_ms > ?3)
             ORDER BY certifications.certified_at_ms DESC, certifications.id
             LIMIT 1",
            params![engine, capability, observed_at_ms],
            |row| row.get(0),
        )
        .optional()?;

    let verification_run_id = VerificationRunId::from_stable_name(
        Uuid::NAMESPACE_URL,
        format!(
            "verification:post_backup:{}:l{}",
            recovery_point_id.as_str(),
            requested_level
        )
        .as_bytes(),
    );
    let unsupported_reason =
        if requested_level == 2 && !matches!(engine.as_str(), "postgresql" | "sqlite") {
            Some("VERIFY_L2_NATIVE_UNSUPPORTED")
        } else {
            match support_status.as_deref() {
                Some("passed") if requested_level < 3 => None,
                Some("passed") => Some("VERIFICATION_RESTORE_RUN_REQUIRED"),
                Some("limited") => Some("VERIFY_LEVEL_LIMITED"),
                _ => Some("VERIFY_LEVEL_UNSUPPORTED"),
            }
        };

    let (result, event_code, severity, reason_code) = if let Some(reason) = unsupported_reason {
        let spec = UnsupportedVerificationSpec::post_backup(
            verification_run_id.clone(),
            recovery_point_id.clone(),
            requested_level,
            reason,
            observed_at_ms,
        );
        let result = record_unsupported_in_transaction(transaction, &spec)?;
        (result, "verification.unsupported", "warning", Some(reason))
    } else {
        let work_item_id = WorkItemId::from_stable_name(
            Uuid::NAMESPACE_URL,
            format!(
                "verification-work:post_backup:{}:l{}",
                recovery_point_id.as_str(),
                requested_level
            )
            .as_bytes(),
        );
        let spec = VerificationWorkSpec::post_backup(
            verification_run_id.clone(),
            work_item_id,
            recovery_point_id.clone(),
            requested_level,
            observed_at_ms,
        );
        let result = enqueue_in_transaction(transaction, &spec)?;
        (result, "verification.queued", "info", None)
    };
    let outbox_intents_created = record_activity_and_outbox(
        transaction,
        &plan_id,
        recovery_point_id,
        Some(&verification_run_id),
        Some(requested_level),
        event_code,
        severity,
        reason_code,
        observed_at_ms,
    )?;
    Ok(PostBackupVerificationOutcome {
        disposition: if reason_code.is_some() {
            PostBackupVerificationDisposition::Unsupported
        } else {
            PostBackupVerificationDisposition::Queued
        },
        requested_level: Some(requested_level),
        verification_run_id: Some(verification_run_id),
        work_item_id: result.work_item_id,
        reason_code: reason_code.map(str::to_owned),
        outbox_intents_created,
    })
}

fn enqueue_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    spec: &VerificationWorkSpec,
) -> Result<VerificationEnqueueResult, RepositoryError> {
    spec.validate()?;
    ensure_recovery_point_eligible(transaction, &spec.recovery_point_id)?;
    ensure_l3_restore_binding(
        transaction,
        &spec.recovery_point_id,
        spec.requested_level,
        spec.restore_run_id.as_ref(),
    )?;
    let source_replica_id = if spec.requested_level < 3 {
        Some(select_required_verification_replica(
            transaction,
            &spec.recovery_point_id,
            spec.source_replica_id.as_deref(),
        )?)
    } else {
        spec.source_replica_id.clone()
    };
    let run_inserted = transaction.execute(
        "INSERT INTO verification_runs(
             id, recovery_point_id, source_replica_id, requested_level,
             trigger_kind, state, restore_run_id, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, ?7)
         ON CONFLICT DO NOTHING",
        params![
            spec.verification_run_id.as_str(),
            spec.recovery_point_id.as_str(),
            source_replica_id.as_deref(),
            spec.requested_level,
            spec.trigger_kind,
            spec.restore_run_id.as_ref().map(RestoreRunId::as_str),
            spec.available_at_ms
        ],
    )?;
    if run_inserted == 0 {
        ensure_existing_run_matches(
            transaction,
            &spec.verification_run_id,
            &spec.recovery_point_id,
            source_replica_id.as_deref(),
            spec.requested_level,
            spec.trigger_kind,
            spec.restore_run_id.as_ref(),
            None,
        )?;
    }
    let work_inserted = transaction.execute(
        "INSERT INTO work_items(
             id, kind, verification_run_id, priority, state,
             available_at_ms, attempt_limit, created_at_ms, updated_at_ms
         ) VALUES (?1, 'verification', ?2, ?3, 'queued', ?4, ?5, ?4, ?4)
         ON CONFLICT DO NOTHING",
        params![
            spec.work_item_id.as_str(),
            spec.verification_run_id.as_str(),
            spec.priority,
            spec.available_at_ms,
            spec.attempt_limit
        ],
    )?;
    if work_inserted == 0 {
        let existing: Option<(String, String)> = transaction
            .query_row(
                "SELECT id, kind FROM work_items WHERE verification_run_id = ?1",
                [spec.verification_run_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if existing != Some((spec.work_item_id.to_string(), "verification".to_owned())) {
            return Err(RepositoryError::VerificationIdentityConflict);
        }
    }
    Ok(VerificationEnqueueResult {
        verification_run_id: spec.verification_run_id.clone(),
        work_item_id: Some(spec.work_item_id.clone()),
        created: run_inserted == 1 && work_inserted == 1,
    })
}

fn record_unsupported_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    spec: &UnsupportedVerificationSpec,
) -> Result<VerificationEnqueueResult, RepositoryError> {
    spec.validate()?;
    ensure_recovery_point_eligible(transaction, &spec.recovery_point_id)?;
    let inserted = transaction.execute(
        "INSERT INTO verification_runs(
             id, recovery_point_id, source_replica_id, requested_level,
             trigger_kind, state, restore_run_id, completed_at_ms,
             error_code, created_at_ms, updated_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4,
             ?5, 'unsupported', ?6, ?7,
             ?8, ?7, ?7
         )
         ON CONFLICT DO NOTHING",
        params![
            spec.verification_run_id.as_str(),
            spec.recovery_point_id.as_str(),
            spec.source_replica_id.as_deref(),
            spec.requested_level,
            spec.trigger_kind,
            spec.restore_run_id.as_ref().map(RestoreRunId::as_str),
            spec.observed_at_ms,
            spec.error_code
        ],
    )?;
    if inserted == 0 {
        ensure_existing_run_matches(
            transaction,
            &spec.verification_run_id,
            &spec.recovery_point_id,
            spec.source_replica_id.as_deref(),
            spec.requested_level,
            spec.trigger_kind,
            spec.restore_run_id.as_ref(),
            Some(spec.error_code.as_str()),
        )?;
    }
    project_terminal_verification_run(
        transaction,
        spec.verification_run_id.as_str(),
        spec.observed_at_ms,
    )?;
    Ok(VerificationEnqueueResult {
        verification_run_id: spec.verification_run_id.clone(),
        work_item_id: None,
        created: inserted == 1,
    })
}

#[allow(clippy::too_many_arguments)]
fn record_activity_and_outbox(
    transaction: &rusqlite::Transaction<'_>,
    plan_id: &str,
    recovery_point_id: &RecoveryPointId,
    verification_run_id: Option<&VerificationRunId>,
    requested_level: Option<i64>,
    event_code: &str,
    severity: &str,
    reason_code: Option<&str>,
    observed_at_ms: i64,
) -> Result<usize, RepositoryError> {
    let event_id = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!(
            "activity:{event_code}:{}:{}",
            recovery_point_id.as_str(),
            requested_level.unwrap_or(0)
        )
        .as_bytes(),
    )
    .to_string();
    transaction.execute(
        "INSERT INTO activity_events(
             id, occurred_at_ms, severity, event_code, entity_type, entity_id,
             work_item_id, message, attributes_json, redaction_version, created_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6,
             NULL, ?7, ?8, 1, ?2
         )
         ON CONFLICT(id) DO NOTHING",
        params![
            event_id,
            observed_at_ms,
            severity,
            event_code,
            if verification_run_id.is_some() {
                "verification_run"
            } else {
                "recovery_point"
            },
            verification_run_id
                .map(VerificationRunId::as_str)
                .unwrap_or_else(|| recovery_point_id.as_str()),
            if reason_code.is_some() {
                "Verification policy could not be executed at the requested level"
            } else {
                "Verification work was durably queued"
            },
            serde_json::json!({
                "reasonCode": reason_code,
                "recoveryPointId": recovery_point_id.as_str(),
                "requestedLevel": requested_level,
                "verificationRunId": verification_run_id.map(VerificationRunId::as_str),
            })
            .to_string()
        ],
    )?;
    let activity_event_seq: i64 = transaction.query_row(
        "SELECT seq FROM activity_events WHERE id = ?1",
        [event_id.as_str()],
        |row| row.get(0),
    )?;

    let mut statement = transaction.prepare(
        "SELECT rules.id, rules.event_filter_json, rules.severity_threshold
         FROM plan_notification_rules AS bindings
         JOIN notification_rules AS rules
           ON rules.id = bindings.notification_rule_id
          AND rules.enabled = 1
         WHERE bindings.backup_plan_id = ?1
         ORDER BY rules.id",
    )?;
    let rules = statement
        .query_map([plan_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let payload_json = serde_json::json!({
        "eventCode": event_code,
        "reasonCode": reason_code,
        "recoveryPointId": recovery_point_id.as_str(),
        "requestedLevel": requested_level,
        "schemaVersion": 1,
        "severity": severity,
        "verificationRunId": verification_run_id.map(VerificationRunId::as_str),
    })
    .to_string();
    let dedupe_key = format!(
        "{event_code}:{}:{}",
        recovery_point_id.as_str(),
        requested_level.unwrap_or(0)
    );
    let mut created = 0usize;
    for (rule_id, filter_json, threshold) in rules {
        if !notification_rule_matches(&filter_json, event_code)
            || severity_rank(severity) < severity_rank(&threshold)
        {
            continue;
        }
        let outbox_id = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("notification:{rule_id}:{dedupe_key}").as_bytes(),
        )
        .to_string();
        let inserted = transaction.execute(
            "INSERT INTO notification_outbox(
                 id, notification_rule_id, activity_event_seq, deduplication_key,
                 state, payload_json, available_at_ms, attempt_limit,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 'pending', ?5, ?6, 5,
                 ?6, ?6
             )
             ON CONFLICT(notification_rule_id, deduplication_key) DO NOTHING",
            params![
                outbox_id,
                rule_id,
                activity_event_seq,
                dedupe_key,
                payload_json,
                observed_at_ms
            ],
        )?;
        if inserted == 0 {
            let existing: Option<String> = transaction
                .query_row(
                    "SELECT payload_json
                     FROM notification_outbox
                     WHERE notification_rule_id = ?1
                       AND deduplication_key = ?2",
                    params![rule_id, dedupe_key],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.as_deref() != Some(payload_json.as_str()) {
                return Err(RepositoryError::NotificationDeduplicationConflict);
            }
        } else {
            created += 1;
        }
    }
    Ok(created)
}

fn notification_rule_matches(filter_json: &str, event_code: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(filter_json) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.is_empty() {
        return true;
    }
    if object
        .get("event")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|event| event == "*" || event == event_code)
    {
        return true;
    }
    object
        .get("events")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|events| {
            events.iter().any(|event| {
                event
                    .as_str()
                    .is_some_and(|event| event == "*" || event == event_code)
            })
        })
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "debug" => 0,
        "info" => 1,
        "warning" => 2,
        "error" => 3,
        "critical" => 4,
        _ => u8::MAX,
    }
}

fn not_due_outcome(reason_code: &str) -> PostBackupVerificationOutcome {
    PostBackupVerificationOutcome {
        disposition: PostBackupVerificationDisposition::NotDue,
        requested_level: None,
        verification_run_id: None,
        work_item_id: None,
        reason_code: Some(reason_code.to_owned()),
        outbox_intents_created: 0,
    }
}

fn ensure_recovery_point_eligible(
    transaction: &rusqlite::Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
) -> Result<(), RepositoryError> {
    let state: Option<(String, String)> = transaction
        .query_row(
            "SELECT state, signature_status
             FROM catalog_recovery_points
             WHERE id = ?1",
            [recovery_point_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if state != Some(("complete".to_owned(), "valid".to_owned())) {
        return Err(RepositoryError::RecoveryPointIncomplete);
    }
    Ok(())
}

fn ensure_l3_restore_binding(
    transaction: &rusqlite::Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    requested_level: i64,
    restore_run_id: Option<&RestoreRunId>,
) -> Result<(), RepositoryError> {
    if requested_level < 3 {
        return Ok(());
    }
    let restore_run_id = restore_run_id.ok_or(RepositoryError::VerificationRestoreRunRequired)?;
    let binding: Option<(String, String)> = transaction
        .query_row(
            "SELECT recovery_point_id, purpose FROM restore_runs WHERE id = ?1",
            [restore_run_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if binding != Some((recovery_point_id.to_string(), "restore_drill".to_owned())) {
        return Err(RepositoryError::VerificationRestoreRunRequired);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn ensure_existing_run_matches(
    transaction: &rusqlite::Transaction<'_>,
    verification_run_id: &VerificationRunId,
    recovery_point_id: &RecoveryPointId,
    source_replica_id: Option<&str>,
    requested_level: i64,
    trigger_kind: &str,
    restore_run_id: Option<&RestoreRunId>,
    unsupported_error_code: Option<&str>,
) -> Result<(), RepositoryError> {
    let existing: Option<ExistingVerificationRunRow> = transaction
        .query_row(
            "SELECT recovery_point_id, source_replica_id, requested_level, trigger_kind,
                        restore_run_id, state, error_code
                 FROM verification_runs
                 WHERE id = ?1",
            [verification_run_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((recovery, source_replica, level, trigger, restore, state, error_code)) = existing
    else {
        return Err(RepositoryError::VerificationIdentityConflict);
    };
    let state_matches = unsupported_error_code
        .map_or(!matches!(state.as_str(), "unsupported"), |expected| {
            state == "unsupported" && error_code.as_deref() == Some(expected)
        });
    if recovery != recovery_point_id.as_str()
        || source_replica.as_deref() != source_replica_id
        || level != requested_level
        || trigger != trigger_kind
        || restore.as_deref() != restore_run_id.map(RestoreRunId::as_str)
        || !state_matches
    {
        return Err(RepositoryError::VerificationIdentityConflict);
    }
    Ok(())
}

fn select_required_verification_replica(
    transaction: &rusqlite::Transaction<'_>,
    recovery_point_id: &RecoveryPointId,
    requested_replica_id: Option<&str>,
) -> Result<String, RepositoryError> {
    transaction
        .query_row(
            "SELECT replica.id
             FROM catalog_artifact_replicas AS replica
             JOIN catalog_recovery_points AS recovery
               ON recovery.id = replica.recovery_point_id
             WHERE replica.recovery_point_id = ?1
               AND (?2 IS NULL OR replica.id = ?2)
               AND replica.requirement = 'required'
               AND replica.state = 'committed'
               AND replica.reader_status = 'eligible'
               AND (
                   SELECT COUNT(*)
                   FROM catalog_artifact_parts AS part
                   WHERE part.recovery_point_id = recovery.id
                     AND part.required = 1
               ) = recovery.required_part_count
               AND EXISTS (
                   SELECT 1
                   FROM catalog_stored_objects AS manifest
                   WHERE manifest.artifact_replica_id = replica.id
                     AND manifest.object_role = 'set_manifest'
                     AND manifest.state = 'present'
               )
               AND NOT EXISTS (
                   SELECT 1
                   FROM catalog_artifact_parts AS part
                   WHERE part.recovery_point_id = replica.recovery_point_id
                     AND part.required = 1
                     AND NOT EXISTS (
                         SELECT 1
                         FROM catalog_stored_objects AS payload
                         WHERE payload.artifact_replica_id = replica.id
                           AND payload.artifact_part_id = part.id
                           AND payload.object_role = 'payload'
                           AND payload.state = 'present'
                     )
               )
             ORDER BY replica.id
             LIMIT 1",
            params![recovery_point_id.as_str(), requested_replica_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(RepositoryError::RecoveryPointIncomplete)
}

fn invalid_text(value: &str) -> bool {
    value.is_empty() || value.len() > 512 || value.contains(['\0', '\r', '\n'])
}
