use thiserror::Error;
use uuid::Uuid;

use crate::ImmutabilityKind;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionCandidateV2 {
    recovery_point_id: Uuid,
    destination_fingerprint: String,
    committed_at_ms: i64,
    policy_expired: bool,
    pinned: bool,
    legal_hold: bool,
    immutability: ImmutabilityKind,
    retention_until_ms: Option<i64>,
    last_compliant: bool,
    active_restore_source: bool,
    active_repair_source: bool,
    tombstoned_at_ms: Option<i64>,
}

impl RetentionCandidateV2 {
    pub fn new(
        recovery_point_id: Uuid,
        destination_fingerprint: impl Into<String>,
        committed_at_ms: i64,
        policy_expired: bool,
    ) -> Result<Self, RetentionError> {
        let value = Self {
            recovery_point_id,
            destination_fingerprint: destination_fingerprint.into(),
            committed_at_ms,
            policy_expired,
            pinned: false,
            legal_hold: false,
            immutability: ImmutabilityKind::None,
            retention_until_ms: None,
            last_compliant: false,
            active_restore_source: false,
            active_repair_source: false,
            tombstoned_at_ms: None,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn with_pinned(mut self, value: bool) -> Self {
        self.pinned = value;
        self
    }

    pub fn with_legal_hold(mut self, value: bool) -> Self {
        self.legal_hold = value;
        self
    }

    pub fn with_immutability(mut self, value: ImmutabilityKind) -> Self {
        self.immutability = value;
        self
    }

    pub fn with_retention_until_ms(mut self, value: Option<i64>) -> Self {
        self.retention_until_ms = value;
        self
    }

    pub fn with_last_compliant(mut self, value: bool) -> Self {
        self.last_compliant = value;
        self
    }

    pub fn with_active_restore_source(mut self, value: bool) -> Self {
        self.active_restore_source = value;
        self
    }

    pub fn with_active_repair_source(mut self, value: bool) -> Self {
        self.active_repair_source = value;
        self
    }

    pub fn with_tombstoned_at_ms(mut self, value: Option<i64>) -> Self {
        self.tombstoned_at_ms = value;
        self
    }

    fn validate(&self) -> Result<(), RetentionError> {
        if self.recovery_point_id.is_nil()
            || self.destination_fingerprint.trim().is_empty()
            || self.destination_fingerprint.len() > 512
            || self.committed_at_ms < 0
            || self
                .retention_until_ms
                .is_some_and(|value| value < self.committed_at_ms)
            || self
                .tombstoned_at_ms
                .is_some_and(|value| value < self.committed_at_ms)
        {
            return Err(RetentionError::InputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionDispositionV2 {
    Preserve,
    TombstoneRequired,
    WaitForGrace,
    DeleteEligible,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionReasonV2 {
    PolicyNotExpired,
    Pinned,
    LegalHold,
    ObjectLockActive,
    ObjectLockEvidenceMissing,
    ServerAppendOnly,
    LastCompliantRecoveryPoint,
    ActiveRestoreSource,
    ActiveRepairSource,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionDecisionV2 {
    pub disposition: RetentionDispositionV2,
    pub reasons: Vec<RetentionReasonV2>,
    pub eligible_after_ms: Option<i64>,
    pub would_break_compliance: bool,
}

pub fn evaluate_retention_v2(
    candidate: &RetentionCandidateV2,
    now_ms: i64,
    grace_period_ms: i64,
) -> Result<RetentionDecisionV2, RetentionError> {
    candidate.validate()?;
    if now_ms < 0 || grace_period_ms <= 0 {
        return Err(RetentionError::InputInvalid);
    }

    let mut reasons = Vec::new();
    if !candidate.policy_expired {
        reasons.push(RetentionReasonV2::PolicyNotExpired);
    }
    if candidate.pinned {
        reasons.push(RetentionReasonV2::Pinned);
    }
    if candidate.legal_hold {
        reasons.push(RetentionReasonV2::LegalHold);
    }
    match candidate.immutability {
        ImmutabilityKind::ObjectLockGovernance | ImmutabilityKind::ObjectLockCompliance => {
            match candidate.retention_until_ms {
                Some(until) if until > now_ms => {
                    reasons.push(RetentionReasonV2::ObjectLockActive);
                }
                None => reasons.push(RetentionReasonV2::ObjectLockEvidenceMissing),
                Some(_) => {}
            }
        }
        ImmutabilityKind::ServerAppendOnly => {
            reasons.push(RetentionReasonV2::ServerAppendOnly);
        }
        ImmutabilityKind::None => {}
    }
    if candidate.last_compliant {
        reasons.push(RetentionReasonV2::LastCompliantRecoveryPoint);
    }
    if candidate.active_restore_source {
        reasons.push(RetentionReasonV2::ActiveRestoreSource);
    }
    if candidate.active_repair_source {
        reasons.push(RetentionReasonV2::ActiveRepairSource);
    }

    let would_break_compliance = candidate.last_compliant;
    if !reasons.is_empty() {
        return Ok(RetentionDecisionV2 {
            disposition: RetentionDispositionV2::Preserve,
            reasons,
            eligible_after_ms: None,
            would_break_compliance,
        });
    }
    let Some(tombstoned_at_ms) = candidate.tombstoned_at_ms else {
        return Ok(RetentionDecisionV2 {
            disposition: RetentionDispositionV2::TombstoneRequired,
            reasons,
            eligible_after_ms: None,
            would_break_compliance,
        });
    };
    let eligible_after_ms = tombstoned_at_ms
        .checked_add(grace_period_ms)
        .ok_or(RetentionError::InputInvalid)?;
    Ok(RetentionDecisionV2 {
        disposition: if now_ms < eligible_after_ms {
            RetentionDispositionV2::WaitForGrace
        } else {
            RetentionDispositionV2::DeleteEligible
        },
        reasons,
        eligible_after_ms: Some(eligible_after_ms),
        would_break_compliance,
    })
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RetentionError {
    #[error("RETENTION_INPUT_INVALID")]
    InputInvalid,
    #[error("RETENTION_OBJECT_SET_INVALID")]
    ObjectSetInvalid,
    #[error("RETENTION_DELETE_NOT_ELIGIBLE")]
    DeleteNotEligible,
    #[error("RETENTION_CONFIRMATION_MISMATCH")]
    ConfirmationMismatch,
}

impl RetentionError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "RETENTION_INPUT_INVALID",
            Self::ObjectSetInvalid => "RETENTION_OBJECT_SET_INVALID",
            Self::DeleteNotEligible => "RETENTION_DELETE_NOT_ELIGIBLE",
            Self::ConfirmationMismatch => "RETENTION_CONFIRMATION_MISMATCH",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionObjectRoleV2 {
    Payload,
    PartManifest,
    SetManifest,
    VerificationEvidence,
}

impl RetentionObjectRoleV2 {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Payload => "payload",
            Self::PartManifest => "part_manifest",
            Self::SetManifest => "set_manifest",
            Self::VerificationEvidence => "verification_evidence",
        }
    }

    const fn delete_order(self) -> u8 {
        match self {
            Self::SetManifest => 0,
            Self::PartManifest => 1,
            Self::VerificationEvidence => 2,
            Self::Payload => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionObjectV2 {
    key: String,
    version: Option<String>,
    role: RetentionObjectRoleV2,
}

impl RetentionObjectV2 {
    pub fn new(
        key: impl Into<String>,
        role: RetentionObjectRoleV2,
    ) -> Result<Self, RetentionError> {
        let value = Self {
            key: key.into(),
            version: None,
            role,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn versioned(
        key: impl Into<String>,
        version: impl Into<String>,
        role: RetentionObjectRoleV2,
    ) -> Result<Self, RetentionError> {
        let value = Self {
            key: key.into(),
            version: Some(version.into()),
            role,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    fn validate(&self) -> Result<(), RetentionError> {
        if !valid_object_key(&self.key)
            || self.version.as_deref().is_some_and(|version| {
                version.is_empty()
                    || version.len() > 1_024
                    || version
                        .chars()
                        .any(|value| matches!(value, '\0' | '\r' | '\n'))
            })
        {
            return Err(RetentionError::ObjectSetInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionAuthorizationObjectV1 {
    key: String,
    role: String,
    version: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetentionAuthorizationDocumentV1 {
    active_repair_source: bool,
    active_restore_source: bool,
    committed_at_ms: i64,
    delete_secret_ref_id: Option<String>,
    destination_fingerprint: String,
    disposition: String,
    grace_period_ms: i64,
    immutability: String,
    last_compliant: bool,
    legal_hold: bool,
    now_ms: i64,
    objects: Vec<RetentionAuthorizationObjectV1>,
    pinned: bool,
    policy_expired: bool,
    recovery_point_id: Uuid,
    retention_until_ms: Option<i64>,
    tombstoned_at_ms: Option<i64>,
    would_break_compliance: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionDeleteAuthorizationV2 {
    schema_version: u32,
    digest_blake3: String,
    plan: RetentionAuthorizationDocumentV1,
}

impl RetentionDeleteAuthorizationV2 {
    pub fn to_json(&self) -> Result<String, RetentionError> {
        self.validate()?;
        serde_json::to_string(self).map_err(|_| RetentionError::InputInvalid)
    }

    pub fn from_json(value: &str) -> Result<Self, RetentionError> {
        if value.is_empty() || value.len() > 1024 * 1024 {
            return Err(RetentionError::InputInvalid);
        }
        let authorization =
            serde_json::from_str::<Self>(value).map_err(|_| RetentionError::InputInvalid)?;
        authorization.validate()?;
        Ok(authorization)
    }

    pub fn digest_blake3(&self) -> &str {
        &self.digest_blake3
    }

    pub const fn recovery_point_id(&self) -> Uuid {
        self.plan.recovery_point_id
    }

    pub fn destination_fingerprint(&self) -> &str {
        &self.plan.destination_fingerprint
    }

    pub fn delete_secret_ref_id(&self) -> Option<&str> {
        self.plan.delete_secret_ref_id.as_deref()
    }

    pub const fn tombstoned_at_ms(&self) -> Option<i64> {
        self.plan.tombstoned_at_ms
    }

    pub const fn evaluated_at_ms(&self) -> i64 {
        self.plan.now_ms
    }

    pub const fn grace_period_ms(&self) -> i64 {
        self.plan.grace_period_ms
    }

    pub fn has_same_delete_scope(&self, other: &Self) -> Result<bool, RetentionError> {
        self.validate()?;
        other.validate()?;
        let mut this_plan = self.plan.clone();
        let mut other_plan = other.plan.clone();
        this_plan.now_ms = 0;
        other_plan.now_ms = 0;
        Ok(this_plan == other_plan)
    }

    pub fn confirm(
        self,
        expected_digest_blake3: &str,
        expected_recovery_point_id: Uuid,
        expected_destination_fingerprint: &str,
    ) -> Result<ConfirmedRetentionDeleteV2, RetentionError> {
        let objects = self.validate()?;
        if self.digest_blake3 != expected_digest_blake3
            || self.plan.recovery_point_id != expected_recovery_point_id
            || self.plan.destination_fingerprint != expected_destination_fingerprint
        {
            return Err(RetentionError::ConfirmationMismatch);
        }
        Ok(ConfirmedRetentionDeleteV2 {
            authorization: self,
            objects,
        })
    }

    fn validate(&self) -> Result<Vec<RetentionObjectV2>, RetentionError> {
        if self.schema_version != 1
            || self.digest_blake3.len() != 64
            || !self
                .digest_blake3
                .bytes()
                .all(|value| value.is_ascii_hexdigit())
        {
            return Err(RetentionError::InputInvalid);
        }
        if self
            .plan
            .delete_secret_ref_id
            .as_deref()
            .is_some_and(|value| {
                Uuid::parse_str(value)
                    .map(|parsed| parsed.to_string() != value)
                    .unwrap_or(true)
            })
        {
            return Err(RetentionError::InputInvalid);
        }
        let immutability = parse_immutability(&self.plan.immutability)?;
        let candidate = RetentionCandidateV2::new(
            self.plan.recovery_point_id,
            self.plan.destination_fingerprint.clone(),
            self.plan.committed_at_ms,
            self.plan.policy_expired,
        )?
        .with_pinned(self.plan.pinned)
        .with_legal_hold(self.plan.legal_hold)
        .with_immutability(immutability)
        .with_retention_until_ms(self.plan.retention_until_ms)
        .with_last_compliant(self.plan.last_compliant)
        .with_active_restore_source(self.plan.active_restore_source)
        .with_active_repair_source(self.plan.active_repair_source)
        .with_tombstoned_at_ms(self.plan.tombstoned_at_ms);
        let decision =
            evaluate_retention_v2(&candidate, self.plan.now_ms, self.plan.grace_period_ms)?;
        if decision.disposition != RetentionDispositionV2::DeleteEligible
            || self.plan.disposition != disposition_str(decision.disposition)
            || self.plan.would_break_compliance != decision.would_break_compliance
        {
            return Err(RetentionError::DeleteNotEligible);
        }

        let mut objects = self
            .plan
            .objects
            .iter()
            .map(|object| {
                let role = parse_object_role(&object.role)?;
                match &object.version {
                    Some(version) => RetentionObjectV2::versioned(&object.key, version, role),
                    None => RetentionObjectV2::new(&object.key, role),
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let original = objects.clone();
        sort_retention_objects(&mut objects);
        if objects != original
            || objects.is_empty()
            || objects
                .iter()
                .filter(|object| object.role == RetentionObjectRoleV2::SetManifest)
                .count()
                != 1
            || has_duplicate_object_identity(&objects)
        {
            return Err(RetentionError::ObjectSetInvalid);
        }

        let canonical = serde_json::to_vec(&self.plan).map_err(|_| RetentionError::InputInvalid)?;
        if blake3::hash(&canonical).to_hex().as_str() != self.digest_blake3 {
            return Err(RetentionError::ConfirmationMismatch);
        }
        Ok(objects)
    }
}

#[derive(Clone, Debug)]
pub struct RetentionDeletePlanV2 {
    decision: RetentionDecisionV2,
    authorization: RetentionDeleteAuthorizationV2,
}

impl RetentionDeletePlanV2 {
    pub fn prepare(
        candidate: &RetentionCandidateV2,
        objects: Vec<RetentionObjectV2>,
        now_ms: i64,
        grace_period_ms: i64,
    ) -> Result<Self, RetentionError> {
        Self::prepare_with_delete_secret_ref(candidate, objects, now_ms, grace_period_ms, None)
    }

    pub fn prepare_with_delete_secret_ref(
        candidate: &RetentionCandidateV2,
        mut objects: Vec<RetentionObjectV2>,
        now_ms: i64,
        grace_period_ms: i64,
        delete_secret_ref_id: Option<&str>,
    ) -> Result<Self, RetentionError> {
        if delete_secret_ref_id.is_some_and(|value| {
            Uuid::parse_str(value)
                .map(|parsed| parsed.to_string() != value)
                .unwrap_or(true)
        }) {
            return Err(RetentionError::InputInvalid);
        }
        let decision = evaluate_retention_v2(candidate, now_ms, grace_period_ms)?;
        sort_retention_objects(&mut objects);
        if objects.is_empty()
            || objects
                .iter()
                .filter(|object| object.role == RetentionObjectRoleV2::SetManifest)
                .count()
                != 1
            || has_duplicate_object_identity(&objects)
        {
            return Err(RetentionError::ObjectSetInvalid);
        }
        let document = RetentionAuthorizationDocumentV1 {
            active_repair_source: candidate.active_repair_source,
            active_restore_source: candidate.active_restore_source,
            committed_at_ms: candidate.committed_at_ms,
            delete_secret_ref_id: delete_secret_ref_id.map(str::to_owned),
            destination_fingerprint: candidate.destination_fingerprint.clone(),
            disposition: disposition_str(decision.disposition).to_owned(),
            grace_period_ms,
            immutability: immutability_str(candidate.immutability).to_owned(),
            last_compliant: candidate.last_compliant,
            legal_hold: candidate.legal_hold,
            now_ms,
            objects: objects
                .into_iter()
                .map(|object| RetentionAuthorizationObjectV1 {
                    key: object.key,
                    role: object.role.as_str().to_owned(),
                    version: object.version,
                })
                .collect(),
            pinned: candidate.pinned,
            policy_expired: candidate.policy_expired,
            recovery_point_id: candidate.recovery_point_id,
            retention_until_ms: candidate.retention_until_ms,
            tombstoned_at_ms: candidate.tombstoned_at_ms,
            would_break_compliance: decision.would_break_compliance,
        };
        let canonical = serde_json::to_vec(&document).map_err(|_| RetentionError::InputInvalid)?;
        Ok(Self {
            decision,
            authorization: RetentionDeleteAuthorizationV2 {
                schema_version: 1,
                digest_blake3: blake3::hash(&canonical).to_hex().to_string(),
                plan: document,
            },
        })
    }

    pub fn digest_blake3(&self) -> &str {
        self.authorization.digest_blake3()
    }

    pub const fn decision(&self) -> &RetentionDecisionV2 {
        &self.decision
    }

    pub const fn authorization(&self) -> &RetentionDeleteAuthorizationV2 {
        &self.authorization
    }

    pub fn preview_json(&self) -> Result<String, RetentionError> {
        serde_json::to_string(&self.authorization).map_err(|_| RetentionError::InputInvalid)
    }

    pub fn confirm(
        self,
        confirmed_digest_blake3: &str,
    ) -> Result<ConfirmedRetentionDeleteV2, RetentionError> {
        if self.decision.disposition != RetentionDispositionV2::DeleteEligible {
            return Err(RetentionError::DeleteNotEligible);
        }
        let recovery_point_id = self.authorization.recovery_point_id();
        let destination_fingerprint = self.authorization.destination_fingerprint().to_owned();
        self.authorization.confirm(
            confirmed_digest_blake3,
            recovery_point_id,
            &destination_fingerprint,
        )
    }
}

#[derive(Clone, Debug)]
pub struct ConfirmedRetentionDeleteV2 {
    authorization: RetentionDeleteAuthorizationV2,
    objects: Vec<RetentionObjectV2>,
}

impl ConfirmedRetentionDeleteV2 {
    pub fn authorization(&self) -> &RetentionDeleteAuthorizationV2 {
        &self.authorization
    }
}

#[async_trait]
pub trait DestinationDeleteV2: Send + Sync {
    async fn delete_object(&self, key: &str, version: Option<&str>) -> Result<(), DeleteErrorV2>;
}

pub struct LocalDeleteOperatorV2 {
    root: std::path::PathBuf,
}

impl LocalDeleteOperatorV2 {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait]
impl DestinationDeleteV2 for LocalDeleteOperatorV2 {
    async fn delete_object(&self, key: &str, version: Option<&str>) -> Result<(), DeleteErrorV2> {
        if !valid_object_key(key) || version.is_some() {
            return Err(DeleteErrorV2::Backend {
                key: key.to_owned(),
                message: if version.is_some() {
                    "local destination does not support object versions".to_owned()
                } else {
                    "invalid relative object key".to_owned()
                },
                retryable: false,
            });
        }
        match tokio::fs::remove_file(self.root.join(key)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(DeleteErrorV2::Backend {
                key: key.to_owned(),
                retryable: matches!(
                    error.kind(),
                    std::io::ErrorKind::Interrupted
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                ),
                message: error.to_string(),
            }),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DeleteErrorV2 {
    #[error("retention lock preserves {key} until {retention_until_ms:?}")]
    RetentionLocked {
        key: String,
        retention_until_ms: Option<i64>,
    },
    #[error("delete backend failed for {key}: {message}")]
    Backend {
        key: String,
        message: String,
        retryable: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteObjectDispositionV2 {
    Deleted,
    RetainedByLock,
    RetryRequired,
    FailedTerminal,
    NotAttemptedMarkerBlocked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteObjectObservationV2 {
    pub object_key: String,
    pub object_version: Option<String>,
    pub role: RetentionObjectRoleV2,
    pub disposition: DeleteObjectDispositionV2,
    pub diagnostic: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetentionDeleteReportV2 {
    pub observations: Vec<DeleteObjectObservationV2>,
}

impl RetentionDeleteReportV2 {
    pub fn deleted_count(&self) -> usize {
        self.count(DeleteObjectDispositionV2::Deleted)
    }

    pub fn retained_by_lock_count(&self) -> usize {
        self.count(DeleteObjectDispositionV2::RetainedByLock)
    }

    pub fn retry_required_count(&self) -> usize {
        self.count(DeleteObjectDispositionV2::RetryRequired)
    }

    pub fn blocked_count(&self) -> usize {
        self.count(DeleteObjectDispositionV2::NotAttemptedMarkerBlocked)
    }

    pub fn retry_object_keys(&self) -> Vec<&str> {
        self.observations
            .iter()
            .filter(|observation| {
                observation.disposition == DeleteObjectDispositionV2::RetryRequired
            })
            .map(|observation| observation.object_key.as_str())
            .collect()
    }

    fn count(&self, disposition: DeleteObjectDispositionV2) -> usize {
        self.observations
            .iter()
            .filter(|observation| observation.disposition == disposition)
            .count()
    }
}

pub async fn execute_retention_delete_v2(
    confirmed: &ConfirmedRetentionDeleteV2,
    operator: &dyn DestinationDeleteV2,
) -> RetentionDeleteReportV2 {
    let mut observations = Vec::with_capacity(confirmed.objects.len());
    let Some((set_manifest, remaining_objects)) = confirmed.objects.split_first() else {
        return RetentionDeleteReportV2 { observations };
    };
    debug_assert_eq!(set_manifest.role, RetentionObjectRoleV2::SetManifest);

    let marker_observation = delete_retention_object_v2(set_manifest, operator).await;
    let blocked_diagnostic = match marker_observation.disposition {
        DeleteObjectDispositionV2::Deleted => None,
        DeleteObjectDispositionV2::RetainedByLock => {
            Some("not_attempted:set_manifest_delete_retained_by_lock")
        }
        DeleteObjectDispositionV2::RetryRequired => {
            Some("not_attempted:set_manifest_delete_retry_required")
        }
        DeleteObjectDispositionV2::FailedTerminal => {
            Some("not_attempted:set_manifest_delete_failed_terminal")
        }
        DeleteObjectDispositionV2::NotAttemptedMarkerBlocked => {
            Some("not_attempted:set_manifest_delete_not_attempted")
        }
    };
    observations.push(marker_observation);

    for object in remaining_objects {
        let observation = if let Some(diagnostic) = blocked_diagnostic {
            DeleteObjectObservationV2 {
                object_key: object.key.clone(),
                object_version: object.version.clone(),
                role: object.role,
                disposition: DeleteObjectDispositionV2::NotAttemptedMarkerBlocked,
                diagnostic: Some(diagnostic.to_owned()),
            }
        } else {
            delete_retention_object_v2(object, operator).await
        };
        observations.push(observation);
    }
    RetentionDeleteReportV2 { observations }
}

async fn delete_retention_object_v2(
    object: &RetentionObjectV2,
    operator: &dyn DestinationDeleteV2,
) -> DeleteObjectObservationV2 {
    let (disposition, diagnostic) = match operator
        .delete_object(&object.key, object.version.as_deref())
        .await
    {
        Ok(()) => (DeleteObjectDispositionV2::Deleted, None),
        Err(DeleteErrorV2::RetentionLocked {
            retention_until_ms, ..
        }) => (
            DeleteObjectDispositionV2::RetainedByLock,
            Some(format!("retention_until_ms={retention_until_ms:?}")),
        ),
        Err(DeleteErrorV2::Backend {
            message,
            retryable: true,
            ..
        }) => (DeleteObjectDispositionV2::RetryRequired, Some(message)),
        Err(DeleteErrorV2::Backend {
            message,
            retryable: false,
            ..
        }) => (DeleteObjectDispositionV2::FailedTerminal, Some(message)),
    };
    DeleteObjectObservationV2 {
        object_key: object.key.clone(),
        object_version: object.version.clone(),
        role: object.role,
        disposition,
        diagnostic,
    }
}

fn valid_object_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 2_048
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn sort_retention_objects(objects: &mut [RetentionObjectV2]) {
    objects.sort_by(|left, right| {
        left.role
            .delete_order()
            .cmp(&right.role.delete_order())
            .then_with(|| left.key.cmp(&right.key))
            .then_with(|| left.version.cmp(&right.version))
    });
}

fn has_duplicate_object_identity(objects: &[RetentionObjectV2]) -> bool {
    let mut seen = std::collections::HashSet::with_capacity(objects.len());
    objects
        .iter()
        .any(|object| !seen.insert((object.key.as_str(), object.version.as_deref())))
}

fn parse_object_role(value: &str) -> Result<RetentionObjectRoleV2, RetentionError> {
    match value {
        "payload" => Ok(RetentionObjectRoleV2::Payload),
        "part_manifest" => Ok(RetentionObjectRoleV2::PartManifest),
        "set_manifest" => Ok(RetentionObjectRoleV2::SetManifest),
        "verification_evidence" => Ok(RetentionObjectRoleV2::VerificationEvidence),
        _ => Err(RetentionError::ObjectSetInvalid),
    }
}

fn parse_immutability(value: &str) -> Result<ImmutabilityKind, RetentionError> {
    match value {
        "none" => Ok(ImmutabilityKind::None),
        "object_lock_governance" => Ok(ImmutabilityKind::ObjectLockGovernance),
        "object_lock_compliance" => Ok(ImmutabilityKind::ObjectLockCompliance),
        "server_append_only" => Ok(ImmutabilityKind::ServerAppendOnly),
        _ => Err(RetentionError::InputInvalid),
    }
}

const fn disposition_str(disposition: RetentionDispositionV2) -> &'static str {
    match disposition {
        RetentionDispositionV2::Preserve => "preserve",
        RetentionDispositionV2::TombstoneRequired => "tombstone_required",
        RetentionDispositionV2::WaitForGrace => "wait_for_grace",
        RetentionDispositionV2::DeleteEligible => "delete_eligible",
    }
}

const fn immutability_str(immutability: ImmutabilityKind) -> &'static str {
    match immutability {
        ImmutabilityKind::None => "none",
        ImmutabilityKind::ObjectLockGovernance => "object_lock_governance",
        ImmutabilityKind::ObjectLockCompliance => "object_lock_compliance",
        ImmutabilityKind::ServerAppendOnly => "server_append_only",
    }
}
