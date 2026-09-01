use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use restoreplane_catalog::{
    ConfigCutoverRollbackReason, ConfigCutoverRollbackSpec, OperationalStore, PersistedCutoverMode,
    PersistedCutoverRecoveryDisposition, PersistedExecutionAuthority,
};
use thiserror::Error;
use uuid::Uuid;

use crate::cutover::{CutoverReadinessError, FileCutoverRecoveryVerifierV1};
pub use crate::cutover::{
    LegacySchedulerControlError, LegacySchedulerControllerV1, WindowsLegacySchedulerControllerV1,
};
use crate::host::{HostError, MaintenanceAuthority};
use crate::ipc::{TokenIdentity, WindowsSid};

#[derive(Clone, Debug)]
pub struct OfflineCutoverRecoveryRequestV1 {
    rollback_id: Uuid,
    cutover_marker: String,
    recovery_bundle_manifest_path: String,
    reason: ConfigCutoverRollbackReason,
    rolled_back_at_ms: i64,
    confirmed_cutover_marker: String,
}

impl OfflineCutoverRecoveryRequestV1 {
    pub fn new(
        rollback_id: Uuid,
        cutover_marker: impl Into<String>,
        recovery_bundle_manifest_path: impl Into<String>,
        reason: ConfigCutoverRollbackReason,
        rolled_back_at_ms: i64,
        confirmed_cutover_marker: impl Into<String>,
    ) -> Self {
        Self {
            rollback_id,
            cutover_marker: cutover_marker.into(),
            recovery_bundle_manifest_path: recovery_bundle_manifest_path.into(),
            reason,
            rolled_back_at_ms,
            confirmed_cutover_marker: confirmed_cutover_marker.into(),
        }
    }

    pub fn with_confirmed_cutover_marker(
        mut self,
        confirmed_cutover_marker: impl Into<String>,
    ) -> Self {
        self.confirmed_cutover_marker = confirmed_cutover_marker.into();
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfflineCutoverRecoveryResultV1 {
    rollback_id: Uuid,
    rollback_created: bool,
    resumed: bool,
    task_count: usize,
}

impl OfflineCutoverRecoveryResultV1 {
    pub const fn rollback_id(&self) -> Uuid {
        self.rollback_id
    }

    pub const fn rollback_created(&self) -> bool {
        self.rollback_created
    }

    pub const fn resumed(&self) -> bool {
        self.resumed
    }

    pub const fn task_count(&self) -> usize {
        self.task_count
    }
}

pub struct OfflineCutoverRecoveryV1 {
    _authority: MaintenanceAuthority,
    store: OperationalStore,
    owner_sid: WindowsSid,
    verifier: FileCutoverRecoveryVerifierV1,
}

impl OfflineCutoverRecoveryV1 {
    pub fn open(
        instance_key: impl AsRef<str>,
        database_path: PathBuf,
        evidence_root: PathBuf,
        owner_sid: WindowsSid,
        controller: Arc<dyn LegacySchedulerControllerV1>,
    ) -> Result<Self, OfflineCutoverRecoveryError> {
        let authority = MaintenanceAuthority::acquire(instance_key.as_ref())?;
        let store = OperationalStore::open(database_path)
            .map_err(|error| OfflineCutoverRecoveryError::OperationalStore(error.code()))?;
        let verifier = FileCutoverRecoveryVerifierV1::new(evidence_root, controller)?;
        Ok(Self {
            _authority: authority,
            store,
            owner_sid,
            verifier,
        })
    }

    pub fn execute(
        &self,
        caller: &TokenIdentity,
        request: OfflineCutoverRecoveryRequestV1,
    ) -> Result<OfflineCutoverRecoveryResultV1, OfflineCutoverRecoveryError> {
        self.authorize(caller)?;
        if request.cutover_marker.is_empty()
            || request.cutover_marker.len() > 256
            || request.cutover_marker != request.confirmed_cutover_marker
            || request.rolled_back_at_ms < 0
        {
            return Err(OfflineCutoverRecoveryError::ConfirmationInvalid);
        }

        let authority = self
            .store
            .cutover()
            .authority()
            .map_err(map_repository_error)?;
        let resume_after_catalog_commit =
            match (authority.execution_authority(), authority.cutover_mode()) {
                (PersistedExecutionAuthority::Service, PersistedCutoverMode::Production) => {
                    if authority.cutover_marker() != Some(request.cutover_marker.as_str()) {
                        return Err(OfflineCutoverRecoveryError::PreconditionFailed);
                    }
                    match authority.recovery_disposition() {
                        Some(PersistedCutoverRecoveryDisposition::RollbackEligible) => false,
                        Some(PersistedCutoverRecoveryDisposition::ForwardFixRequired) | None => {
                            return Err(OfflineCutoverRecoveryError::ForwardFixRequired);
                        }
                    }
                }
                (
                    PersistedExecutionAuthority::LegacyDirect,
                    PersistedCutoverMode::RehearsalOnly,
                ) => true,
                _ => return Err(OfflineCutoverRecoveryError::PreconditionFailed),
            };

        let plan = self
            .verifier
            .verify(
                &request.recovery_bundle_manifest_path,
                resume_after_catalog_commit,
            )
            .map_err(|_| OfflineCutoverRecoveryError::EvidenceInvalid)?;
        let requested_by_sid_hash = blake3::hash(caller.user_sid().as_str().as_bytes())
            .to_hex()
            .to_string();
        let rollback = self
            .store
            .cutover()
            .rollback_if_clean(ConfigCutoverRollbackSpec::new(
                request.rollback_id,
                request.cutover_marker,
                plan.recovery_bundle_digest_blake3(),
                plan.legacy_scheduler_restore_digest_blake3(),
                requested_by_sid_hash,
                request.reason,
                request.rolled_back_at_ms,
            ))
            .map_err(map_repository_error)?;

        if self.verifier.restore(&plan).is_err() {
            return Err(OfflineCutoverRecoveryError::SchedulerRestoreIncomplete);
        }
        Ok(OfflineCutoverRecoveryResultV1 {
            rollback_id: rollback.rollback_id(),
            rollback_created: rollback.created(),
            resumed: !rollback.created(),
            task_count: plan.tasks().len(),
        })
    }

    fn authorize(&self, caller: &TokenIdentity) -> Result<(), OfflineCutoverRecoveryError> {
        if !caller.local() || !caller.elevated_admin() || caller.user_sid() != &self.owner_sid {
            return Err(OfflineCutoverRecoveryError::Unauthorized);
        }
        Ok(())
    }
}

impl fmt::Debug for OfflineCutoverRecoveryV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OfflineCutoverRecoveryV1")
            .field("owner_sid", &self.owner_sid)
            .field("verifier", &self.verifier)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum OfflineCutoverRecoveryError {
    #[error("{0}")]
    Host(#[from] HostError),
    #[error("{0}")]
    OperationalStore(&'static str),
    #[error("CONFIG_CUTOVER_RECOVERY_UNAUTHORIZED")]
    Unauthorized,
    #[error("CONFIG_CUTOVER_RECOVERY_CONFIRMATION_INVALID")]
    ConfirmationInvalid,
    #[error("CONFIG_CUTOVER_RECOVERY_EVIDENCE_INVALID")]
    EvidenceInvalid,
    #[error("CONFIG_CUTOVER_ROLLBACK_PRECONDITION_FAILED")]
    PreconditionFailed,
    #[error("CONFIG_CUTOVER_FORWARD_FIX_REQUIRED")]
    ForwardFixRequired,
    #[error("{0}")]
    Repository(&'static str),
    #[error("CONFIG_CUTOVER_SCHEDULER_RESTORE_INCOMPLETE")]
    SchedulerRestoreIncomplete,
}

impl OfflineCutoverRecoveryError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Host(error) => error.code(),
            Self::OperationalStore(code) | Self::Repository(code) => code,
            Self::Unauthorized => "CONFIG_CUTOVER_RECOVERY_UNAUTHORIZED",
            Self::ConfirmationInvalid => "CONFIG_CUTOVER_RECOVERY_CONFIRMATION_INVALID",
            Self::EvidenceInvalid => "CONFIG_CUTOVER_RECOVERY_EVIDENCE_INVALID",
            Self::PreconditionFailed => "CONFIG_CUTOVER_ROLLBACK_PRECONDITION_FAILED",
            Self::ForwardFixRequired => "CONFIG_CUTOVER_FORWARD_FIX_REQUIRED",
            Self::SchedulerRestoreIncomplete => "CONFIG_CUTOVER_SCHEDULER_RESTORE_INCOMPLETE",
        }
    }
}

impl From<CutoverReadinessError> for OfflineCutoverRecoveryError {
    fn from(_error: CutoverReadinessError) -> Self {
        Self::EvidenceInvalid
    }
}

fn map_repository_error(
    error: restoreplane_catalog::RepositoryError,
) -> OfflineCutoverRecoveryError {
    match error.code() {
        "CONFIG_CUTOVER_FORWARD_FIX_REQUIRED" => OfflineCutoverRecoveryError::ForwardFixRequired,
        "CONFIG_CUTOVER_ROLLBACK_PRECONDITION_FAILED" => {
            OfflineCutoverRecoveryError::PreconditionFailed
        }
        code => OfflineCutoverRecoveryError::Repository(code),
    }
}
