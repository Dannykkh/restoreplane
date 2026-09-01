use restoreplane_domain::BackupPlanId;
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use uuid::Uuid;

use crate::OperationalStore;

use super::RepositoryError;

#[derive(Clone, Debug)]
pub struct NotificationChannelSpec {
    id: String,
    display_name: String,
    config_json: String,
    created_at_ms: i64,
}

impl NotificationChannelSpec {
    pub fn webhook(
        id: impl Into<String>,
        display_name: impl Into<String>,
        config_json: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            config_json: config_json.into(),
            created_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        validate_identifier(&self.id)?;
        if self.display_name.is_empty()
            || self.display_name.len() > 256
            || self.created_at_ms < 0
            || canonical_json(&self.config_json).is_err()
        {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct NotificationRuleSpec {
    id: String,
    display_name: String,
    channel_id: String,
    event_filter_json: String,
    severity_threshold: String,
    cooldown_ms: i64,
    created_at_ms: i64,
}

impl NotificationRuleSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        display_name: impl Into<String>,
        channel_id: impl Into<String>,
        event_filter_json: impl Into<String>,
        severity_threshold: impl Into<String>,
        cooldown_ms: i64,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            channel_id: channel_id.into(),
            event_filter_json: event_filter_json.into(),
            severity_threshold: severity_threshold.into(),
            cooldown_ms,
            created_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        validate_identifier(&self.id)?;
        validate_identifier(&self.channel_id)?;
        if self.display_name.is_empty()
            || self.display_name.len() > 256
            || !matches!(
                self.severity_threshold.as_str(),
                "info" | "warning" | "error" | "critical"
            )
            || self.cooldown_ms < 0
            || self.created_at_ms < 0
            || canonical_json(&self.event_filter_json).is_err()
        {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct OutboxSpec {
    id: String,
    notification_rule_id: String,
    deduplication_key: String,
    payload_json: String,
    available_at_ms: i64,
    attempt_limit: i64,
}

impl OutboxSpec {
    pub fn new(
        id: impl Into<String>,
        notification_rule_id: impl Into<String>,
        deduplication_key: impl Into<String>,
        payload_json: impl Into<String>,
        available_at_ms: i64,
        attempt_limit: i64,
    ) -> Self {
        Self {
            id: id.into(),
            notification_rule_id: notification_rule_id.into(),
            deduplication_key: deduplication_key.into(),
            payload_json: payload_json.into(),
            available_at_ms,
            attempt_limit,
        }
    }

    fn validate(&self) -> Result<String, RepositoryError> {
        validate_identifier(&self.id)?;
        validate_identifier(&self.notification_rule_id)?;
        if self.deduplication_key.is_empty()
            || self.deduplication_key.len() > 512
            || self.available_at_ms < 0
            || self.attempt_limit <= 0
            || self.attempt_limit > 100
        {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        canonical_json(&self.payload_json)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxUpsert {
    id: String,
    created: bool,
}

impl OutboxUpsert {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn created(&self) -> bool {
        self.created
    }
}

#[derive(Clone, Debug)]
pub struct DeliveryClaimRequest {
    worker_id: String,
    worker_boot_id: String,
    now_ms: i64,
    lease_duration_ms: i64,
}

impl DeliveryClaimRequest {
    pub fn new(
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Self {
        Self {
            worker_id: worker_id.into(),
            worker_boot_id: worker_boot_id.into(),
            now_ms,
            lease_duration_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryClaim {
    outbox_id: String,
    attempt_id: String,
    attempt_no: i64,
    fencing_token: i64,
    worker_id: String,
    worker_boot_id: String,
    channel_kind: String,
    channel_config_json: String,
    deduplication_key: String,
    payload_json: String,
}

impl DeliveryClaim {
    pub fn outbox_id(&self) -> &str {
        &self.outbox_id
    }

    pub const fn attempt_no(&self) -> i64 {
        self.attempt_no
    }

    pub const fn fencing_token(&self) -> i64 {
        self.fencing_token
    }

    pub fn channel_kind(&self) -> &str {
        &self.channel_kind
    }

    pub fn channel_config_json(&self) -> &str {
        &self.channel_config_json
    }

    pub fn deduplication_key(&self) -> &str {
        &self.deduplication_key
    }

    pub fn payload_json(&self) -> &str {
        &self.payload_json
    }
}

#[derive(Clone)]
pub struct NotificationRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn notification(&self) -> NotificationRepository {
        NotificationRepository {
            store: self.clone(),
        }
    }
}

impl NotificationRepository {
    pub fn insert_channel(&self, spec: NotificationChannelSpec) -> Result<(), RepositoryError> {
        spec.validate()?;
        let config_json = canonical_json(&spec.config_json)?;
        self.write(move |connection| {
            connection.execute(
                "INSERT INTO notification_channels(
                     id, display_name, kind, config_json, state, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, 'webhook', ?3, 'draft', ?4, ?4)",
                params![spec.id, spec.display_name, config_json, spec.created_at_ms],
            )?;
            Ok(())
        })
    }

    pub fn mark_channel_ready(
        &self,
        channel_id: &str,
        probed_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        validate_identifier(channel_id)?;
        if probed_at_ms < 0 {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        let channel_id = channel_id.to_owned();
        self.write(move |connection| {
            let affected = connection.execute(
                "UPDATE notification_channels
                 SET state = 'ready', last_probe_status = 'passed',
                     last_probe_at_ms = ?2, updated_at_ms = ?2
                 WHERE id = ?1 AND state IN ('draft', 'attention')",
                params![channel_id, probed_at_ms],
            )?;
            if affected != 1 {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            Ok(())
        })
    }

    pub fn insert_rule(&self, spec: NotificationRuleSpec) -> Result<(), RepositoryError> {
        spec.validate()?;
        let event_filter_json = canonical_json(&spec.event_filter_json)?;
        self.write(move |connection| {
            let channel_ready: bool = connection
                .query_row(
                    "SELECT state = 'ready'
                     FROM notification_channels
                     WHERE id = ?1 AND deleted_at_ms IS NULL",
                    [spec.channel_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(RepositoryError::NotFound)?;
            if !channel_ready {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            connection.execute(
                "INSERT INTO notification_rules(
                     id, display_name, notification_channel_id, enabled,
                     event_filter_json, severity_threshold, cooldown_ms,
                     created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?7)",
                params![
                    spec.id,
                    spec.display_name,
                    spec.channel_id,
                    event_filter_json,
                    spec.severity_threshold,
                    spec.cooldown_ms,
                    spec.created_at_ms
                ],
            )?;
            Ok(())
        })
    }

    pub fn attach_rule_to_plan(
        &self,
        plan_id: &BackupPlanId,
        notification_rule_id: &str,
        created_at_ms: i64,
    ) -> Result<bool, RepositoryError> {
        validate_identifier(notification_rule_id)?;
        if created_at_ms < 0 {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        let plan_id = plan_id.clone();
        let notification_rule_id = notification_rule_id.to_owned();
        self.write(move |connection| {
            let eligible: bool = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1
                     FROM backup_plans AS plans
                     JOIN notification_rules AS rules
                       ON rules.id = ?2
                      AND rules.enabled = 1
                     WHERE plans.id = ?1
                       AND plans.deleted_at_ms IS NULL
                 )",
                params![plan_id.as_str(), notification_rule_id],
                |row| row.get(0),
            )?;
            if !eligible {
                return Err(RepositoryError::NotFound);
            }
            let inserted = connection.execute(
                "INSERT INTO plan_notification_rules(
                     backup_plan_id, notification_rule_id, created_at_ms
                 ) VALUES (?1, ?2, ?3)
                 ON CONFLICT DO NOTHING",
                params![plan_id.as_str(), notification_rule_id, created_at_ms],
            )?;
            Ok(inserted == 1)
        })
    }

    pub fn enqueue(&self, spec: OutboxSpec) -> Result<OutboxUpsert, RepositoryError> {
        let payload_json = spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<(String, String)> = transaction
                .query_row(
                    "SELECT id, payload_json
                     FROM notification_outbox
                     WHERE notification_rule_id = ?1
                       AND deduplication_key = ?2",
                    params![spec.notification_rule_id, spec.deduplication_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((id, existing_payload)) = existing {
                if existing_payload != payload_json {
                    return Err(RepositoryError::NotificationDeduplicationConflict);
                }
                transaction.commit()?;
                return Ok(OutboxUpsert { id, created: false });
            }
            transaction.execute(
                "INSERT INTO notification_outbox(
                     id, notification_rule_id, deduplication_key, state, payload_json,
                     available_at_ms, attempt_limit, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6, ?5, ?5)",
                params![
                    spec.id,
                    spec.notification_rule_id,
                    spec.deduplication_key,
                    payload_json,
                    spec.available_at_ms,
                    spec.attempt_limit
                ],
            )?;
            transaction.commit()?;
            Ok(OutboxUpsert {
                id: spec.id,
                created: true,
            })
        })
    }

    pub fn claim_next(
        &self,
        request: DeliveryClaimRequest,
    ) -> Result<Option<DeliveryClaim>, RepositoryError> {
        if request.worker_id.is_empty()
            || request.worker_boot_id.is_empty()
            || request.now_ms < 0
            || request.lease_duration_ms <= 0
        {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        let lease_expires_at_ms = request
            .now_ms
            .checked_add(request.lease_duration_ms)
            .ok_or(RepositoryError::NotificationInputInvalid)?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let candidate: Option<(String, i64, i64, String, String, String, String)> = transaction
                .query_row(
                    "SELECT outbox.id, outbox.attempt_count, outbox.fencing_token,
                            outbox.deduplication_key, outbox.payload_json,
                            channels.kind, channels.config_json
                     FROM notification_outbox AS outbox
                     JOIN notification_rules AS rules
                       ON rules.id = outbox.notification_rule_id
                      AND rules.enabled = 1
                     JOIN notification_channels AS channels
                       ON channels.id = rules.notification_channel_id
                      AND channels.state = 'ready'
                      AND channels.deleted_at_ms IS NULL
                     WHERE outbox.state = 'pending'
                       AND outbox.available_at_ms <= ?1
                       AND outbox.attempt_count < outbox.attempt_limit
                     ORDER BY outbox.available_at_ms, outbox.created_at_ms, outbox.id
                     LIMIT 1",
                    [request.now_ms],
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
            let Some((
                outbox_id,
                observed_attempt,
                observed_fence,
                deduplication_key,
                payload_json,
                channel_kind,
                channel_config_json,
            )) = candidate
            else {
                transaction.commit()?;
                return Ok(None);
            };
            let attempt_no = observed_attempt + 1;
            let fencing_token = observed_fence + 1;
            let affected = transaction.execute(
                "UPDATE notification_outbox
                 SET state = 'delivering', attempt_count = ?2, fencing_token = ?3,
                     lease_owner = ?4, lease_boot_id = ?5, heartbeat_at_ms = ?6,
                     lease_expires_at_ms = ?7, updated_at_ms = ?6
                 WHERE id = ?1
                   AND state = 'pending'
                   AND available_at_ms <= ?6
                   AND attempt_count = ?8
                   AND fencing_token = ?9
                   AND attempt_count < attempt_limit",
                params![
                    outbox_id,
                    attempt_no,
                    fencing_token,
                    request.worker_id,
                    request.worker_boot_id,
                    request.now_ms,
                    lease_expires_at_ms,
                    observed_attempt,
                    observed_fence
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::NotificationFenceStale);
            }
            let attempt_id = Uuid::new_v4().to_string();
            transaction.execute(
                "INSERT INTO notification_delivery_attempts(
                     id, outbox_id, attempt_no, state, started_at_ms, created_at_ms
                 ) VALUES (?1, ?2, ?3, 'running', ?4, ?4)",
                params![attempt_id, outbox_id, attempt_no, request.now_ms],
            )?;
            transaction.commit()?;
            Ok(Some(DeliveryClaim {
                outbox_id,
                attempt_id,
                attempt_no,
                fencing_token,
                worker_id: request.worker_id,
                worker_boot_id: request.worker_boot_id,
                channel_kind,
                channel_config_json,
                deduplication_key,
                payload_json,
            }))
        })
    }

    pub fn fail_for_retry(
        &self,
        claim: &DeliveryClaim,
        error_code: &str,
        completed_at_ms: i64,
        available_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if error_code.is_empty()
            || error_code.len() > 128
            || completed_at_ms < 0
            || available_at_ms < completed_at_ms
        {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        let claim = claim.clone();
        let error_code = error_code.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let outbox_affected = transaction.execute(
                "UPDATE notification_outbox
                 SET state = CASE
                         WHEN attempt_count >= attempt_limit THEN 'failed_terminal'
                         ELSE 'pending'
                     END,
                     available_at_ms = ?7,
                     lease_owner = NULL, lease_boot_id = NULL,
                     heartbeat_at_ms = NULL, lease_expires_at_ms = NULL,
                     last_error_code = ?8, updated_at_ms = ?9
                 WHERE id = ?1
                   AND state = 'delivering'
                   AND attempt_count = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND lease_expires_at_ms >= ?6",
                params![
                    claim.outbox_id,
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id,
                    claim.worker_boot_id,
                    completed_at_ms,
                    available_at_ms,
                    error_code,
                    completed_at_ms
                ],
            )?;
            if outbox_affected != 1 {
                return Err(RepositoryError::NotificationFenceStale);
            }
            let attempt_affected = transaction.execute(
                "UPDATE notification_delivery_attempts
                 SET state = 'failed', response_code = ?4, completed_at_ms = ?5,
                     next_retry_at_ms = CASE
                         WHEN (SELECT attempt_count < attempt_limit
                               FROM notification_outbox WHERE id = ?2)
                         THEN ?6
                         ELSE NULL
                     END
                 WHERE id = ?1
                   AND outbox_id = ?2
                   AND attempt_no = ?3
                   AND state = 'running'",
                params![
                    claim.attempt_id,
                    claim.outbox_id,
                    claim.attempt_no,
                    error_code,
                    completed_at_ms,
                    available_at_ms
                ],
            )?;
            if attempt_affected != 1 {
                return Err(RepositoryError::NotificationFenceStale);
            }
            observe_notification_failure(
                &transaction,
                &claim.outbox_id,
                &error_code,
                "warning",
                completed_at_ms,
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn mark_failed_terminal(
        &self,
        claim: &DeliveryClaim,
        error_code: &str,
        response_summary: &str,
        completed_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if error_code.is_empty()
            || error_code.len() > 128
            || response_summary.len() > 4 * 1024
            || completed_at_ms < 0
        {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        let claim = claim.clone();
        let error_code = error_code.to_owned();
        let response_summary = response_summary.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let outbox_affected = transaction.execute(
                "UPDATE notification_outbox
                 SET state = 'failed_terminal', last_error_code = ?7,
                     lease_owner = NULL, lease_boot_id = NULL,
                     heartbeat_at_ms = NULL, lease_expires_at_ms = NULL,
                     updated_at_ms = ?6
                 WHERE id = ?1
                   AND state = 'delivering'
                   AND attempt_count = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND lease_expires_at_ms >= ?6",
                params![
                    claim.outbox_id,
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id,
                    claim.worker_boot_id,
                    completed_at_ms,
                    error_code
                ],
            )?;
            if outbox_affected != 1 {
                return Err(RepositoryError::NotificationFenceStale);
            }
            let attempt_affected = transaction.execute(
                "UPDATE notification_delivery_attempts
                 SET state = 'failed', response_code = ?4,
                     response_summary = ?5, completed_at_ms = ?6,
                     next_retry_at_ms = NULL
                 WHERE id = ?1
                   AND outbox_id = ?2
                   AND attempt_no = ?3
                   AND state = 'running'",
                params![
                    claim.attempt_id,
                    claim.outbox_id,
                    claim.attempt_no,
                    error_code,
                    response_summary,
                    completed_at_ms
                ],
            )?;
            if attempt_affected != 1 {
                return Err(RepositoryError::NotificationFenceStale);
            }
            observe_notification_failure(
                &transaction,
                &claim.outbox_id,
                &error_code,
                "error",
                completed_at_ms,
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn recover_expired_leases(&self, now_ms: i64) -> Result<i64, RepositoryError> {
        if now_ms < 0 {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let expired = {
                let mut statement = transaction.prepare(
                    "SELECT id, attempt_count, attempt_limit
                     FROM notification_outbox
                     WHERE state = 'delivering'
                       AND lease_expires_at_ms <= ?1
                     ORDER BY id",
                )?;
                let rows = statement
                    .query_map([now_ms], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            for (outbox_id, attempt_count, attempt_limit) in &expired {
                let terminal = attempt_count >= attempt_limit;
                let state = if terminal {
                    "failed_terminal"
                } else {
                    "pending"
                };
                let next_retry_at_ms = if terminal { None } else { Some(now_ms) };
                let attempt_affected = transaction.execute(
                    "UPDATE notification_delivery_attempts
                     SET state = 'failed', response_code = 'NOTIFICATION_LEASE_EXPIRED',
                         response_summary = 'delivery lease expired before durable completion',
                         completed_at_ms = ?3, next_retry_at_ms = ?4
                     WHERE outbox_id = ?1
                       AND attempt_no = ?2
                       AND state = 'running'",
                    params![outbox_id, attempt_count, now_ms, next_retry_at_ms],
                )?;
                if attempt_affected != 1 {
                    return Err(RepositoryError::NotificationFenceStale);
                }
                let outbox_affected = transaction.execute(
                    "UPDATE notification_outbox
                     SET state = ?3, available_at_ms = MAX(available_at_ms, ?4),
                         lease_owner = NULL, lease_boot_id = NULL,
                         heartbeat_at_ms = NULL, lease_expires_at_ms = NULL,
                         last_error_code = 'NOTIFICATION_LEASE_EXPIRED',
                         updated_at_ms = ?4
                     WHERE id = ?1
                       AND state = 'delivering'
                       AND attempt_count = ?2
                       AND lease_expires_at_ms <= ?4",
                    params![outbox_id, attempt_count, state, now_ms],
                )?;
                if outbox_affected != 1 {
                    return Err(RepositoryError::NotificationFenceStale);
                }
                observe_notification_failure(
                    &transaction,
                    outbox_id,
                    "NOTIFICATION_LEASE_EXPIRED",
                    if terminal { "error" } else { "warning" },
                    now_ms,
                )?;
            }
            let recovered =
                i64::try_from(expired.len()).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
            transaction.commit()?;
            Ok(recovered)
        })
    }

    pub fn mark_delivered(
        &self,
        claim: &DeliveryClaim,
        response_code: &str,
        response_summary: &str,
        delivered_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if response_code.is_empty()
            || response_code.len() > 128
            || response_summary.len() > 4 * 1024
            || delivered_at_ms < 0
        {
            return Err(RepositoryError::NotificationInputInvalid);
        }
        let claim = claim.clone();
        let response_code = response_code.to_owned();
        let response_summary = response_summary.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let outbox_affected = transaction.execute(
                "UPDATE notification_outbox
                 SET state = 'delivered', delivered_at_ms = ?7,
                     lease_owner = NULL, lease_boot_id = NULL,
                     heartbeat_at_ms = NULL, lease_expires_at_ms = NULL,
                     updated_at_ms = ?7
                 WHERE id = ?1
                   AND state = 'delivering'
                   AND attempt_count = ?2
                   AND fencing_token = ?3
                   AND lease_owner = ?4
                   AND lease_boot_id = ?5
                   AND lease_expires_at_ms >= ?6",
                params![
                    claim.outbox_id,
                    claim.attempt_no,
                    claim.fencing_token,
                    claim.worker_id,
                    claim.worker_boot_id,
                    delivered_at_ms,
                    delivered_at_ms
                ],
            )?;
            if outbox_affected != 1 {
                return Err(RepositoryError::NotificationFenceStale);
            }
            let attempt_affected = transaction.execute(
                "UPDATE notification_delivery_attempts
                 SET state = 'delivered', response_code = ?4,
                     response_summary = ?5, completed_at_ms = ?6
                 WHERE id = ?1
                   AND outbox_id = ?2
                   AND attempt_no = ?3
                   AND state = 'running'",
                params![
                    claim.attempt_id,
                    claim.outbox_id,
                    claim.attempt_no,
                    response_code,
                    response_summary,
                    delivered_at_ms
                ],
            )?;
            if attempt_affected != 1 {
                return Err(RepositoryError::NotificationFenceStale);
            }
            transaction.execute(
                "UPDATE attention_states
                 SET state = 'resolved', resolved_at_ms = ?2,
                     resolution_code = 'NOTIFICATION_DELIVERED',
                     updated_at_ms = ?2
                 WHERE fingerprint = ?1
                   AND state IN ('open', 'acknowledged')",
                params![
                    notification_attention_fingerprint(&claim.outbox_id),
                    delivered_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn outbox_count(&self) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row("SELECT COUNT(*) FROM notification_outbox", [], |row| {
                row.get(0)
            })
            .map_err(RepositoryError::from)
    }

    pub fn outbox_state(&self, outbox_id: &str) -> Result<String, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT state FROM notification_outbox WHERE id = ?1",
                [outbox_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    pub fn delivery_attempt_count(&self, outbox_id: &str) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT COUNT(*)
                 FROM notification_delivery_attempts
                 WHERE outbox_id = ?1",
                [outbox_id],
                |row| row.get(0),
            )
            .map_err(RepositoryError::from)
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

fn validate_identifier(value: &str) -> Result<(), RepositoryError> {
    if value.is_empty() || value.len() > 128 {
        return Err(RepositoryError::NotificationInputInvalid);
    }
    Ok(())
}

fn canonical_json(value: &str) -> Result<String, RepositoryError> {
    serde_json::from_str::<serde_json::Value>(value)
        .map(|value| value.to_string())
        .map_err(|_| RepositoryError::NotificationInputInvalid)
}

fn observe_notification_failure(
    transaction: &rusqlite::Transaction<'_>,
    outbox_id: &str,
    error_code: &str,
    severity: &str,
    observed_at_ms: i64,
) -> Result<(), RepositoryError> {
    let event_id = Uuid::new_v4().to_string();
    let event_code = if severity == "error" {
        "NOTIFICATION_DELIVERY_FAILED_TERMINAL"
    } else {
        "NOTIFICATION_DELIVERY_RETRY"
    };
    let message = format!("Notification delivery failed ({error_code})");
    let attributes = serde_json::json!({
        "outboxId": outbox_id,
        "errorCode": error_code
    })
    .to_string();
    transaction.execute(
        "INSERT INTO activity_events(
             id, occurred_at_ms, severity, event_code,
             entity_type, entity_id, message, attributes_json,
             redaction_version, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, 'notification_outbox', ?5, ?6, ?7, 1, ?2)",
        params![
            event_id,
            observed_at_ms,
            severity,
            event_code,
            outbox_id,
            message,
            attributes
        ],
    )?;
    let event_seq = transaction.last_insert_rowid();
    let fingerprint = notification_attention_fingerprint(outbox_id);
    let active: Option<(String, i64)> = transaction
        .query_row(
            "SELECT id, occurrence_count
             FROM attention_states
             WHERE fingerprint = ?1
               AND state IN ('open', 'acknowledged')",
            [fingerprint.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let attention_id = if let Some((attention_id, occurrence_count)) = active {
        let affected = transaction.execute(
            "UPDATE attention_states
             SET severity = ?2, summary = ?3, last_seen_at_ms = ?4,
                 occurrence_count = occurrence_count + 1, updated_at_ms = ?4
             WHERE id = ?1
               AND state IN ('open', 'acknowledged')
               AND occurrence_count = ?5",
            params![
                attention_id,
                severity,
                message,
                observed_at_ms,
                occurrence_count
            ],
        )?;
        if affected != 1 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        attention_id
    } else {
        let attention_id = Uuid::new_v4().to_string();
        transaction.execute(
            "INSERT INTO attention_states(
                 id, fingerprint, category, severity, entity_type, entity_id,
                 state, summary, first_seen_at_ms, last_seen_at_ms,
                 occurrence_count, created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, 'notification_failure', ?3, 'notification_outbox', ?4,
                 'open', ?5, ?6, ?6,
                 1, ?6, ?6
             )",
            params![
                attention_id,
                fingerprint,
                severity,
                outbox_id,
                message,
                observed_at_ms
            ],
        )?;
        attention_id
    };
    transaction.execute(
        "INSERT INTO attention_event_links(
             attention_state_id, activity_event_seq, created_at_ms
         ) VALUES (?1, ?2, ?3)",
        params![attention_id, event_seq, observed_at_ms],
    )?;
    Ok(())
}

fn notification_attention_fingerprint(outbox_id: &str) -> String {
    format!("notification-delivery:{outbox_id}")
}
