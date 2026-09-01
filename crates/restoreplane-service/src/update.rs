use std::fmt;

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateStage {
    Download,
    VerifyDownloadedArtifact,
    StopClaims,
    Drain,
    ControlBackup,
    CompatibilityCheck,
    StopService,
    VerifyStagedArtifact,
    ReplaceBinary,
    MigrateSchema,
    StartService,
    Reconcile,
    CommitMinimumReader,
    ResumeClaims,
}

impl UpdateStage {
    pub const ORDER: [Self; 14] = [
        Self::Download,
        Self::VerifyDownloadedArtifact,
        Self::StopClaims,
        Self::Drain,
        Self::ControlBackup,
        Self::CompatibilityCheck,
        Self::StopService,
        Self::VerifyStagedArtifact,
        Self::ReplaceBinary,
        Self::MigrateSchema,
        Self::StartService,
        Self::Reconcile,
        Self::CommitMinimumReader,
        Self::ResumeClaims,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryDisposition {
    NotRequired,
    PreserveCurrentAuthority,
    ResumeCurrentBuild,
    RestorePreviousBinary,
    ForwardFixClaimsStopped,
}

pub trait UpdatePlatform {
    type Error;

    fn perform(&mut self, stage: UpdateStage) -> Result<(), Self::Error>;
    fn recover(&mut self, disposition: RecoveryDisposition) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateJournal {
    completed_stages: Vec<UpdateStage>,
    recovery_disposition: RecoveryDisposition,
}

impl UpdateJournal {
    pub fn completed_stages(&self) -> &[UpdateStage] {
        &self.completed_stages
    }

    pub const fn recovery_disposition(&self) -> RecoveryDisposition {
        self.recovery_disposition
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct UpdateCoordinator;

impl UpdateCoordinator {
    pub fn execute<P>(
        &self,
        platform: &mut P,
    ) -> Result<UpdateJournal, UpdateExecutionError<P::Error>>
    where
        P: UpdatePlatform,
    {
        let mut completed_stages = Vec::with_capacity(UpdateStage::ORDER.len());
        for stage in UpdateStage::ORDER {
            if let Err(source) = platform.perform(stage) {
                let disposition = recovery_for(stage);
                let recovery_error = platform.recover(disposition).err();
                return Err(UpdateExecutionError {
                    failed_stage: stage,
                    recovery_disposition: disposition,
                    recovery_completed: recovery_error.is_none(),
                    source,
                    recovery_error,
                });
            }
            completed_stages.push(stage);
        }
        Ok(UpdateJournal {
            completed_stages,
            recovery_disposition: RecoveryDisposition::NotRequired,
        })
    }
}

#[derive(Debug)]
pub struct UpdateExecutionError<E> {
    failed_stage: UpdateStage,
    recovery_disposition: RecoveryDisposition,
    recovery_completed: bool,
    source: E,
    recovery_error: Option<E>,
}

impl<E> UpdateExecutionError<E> {
    pub const fn failed_stage(&self) -> UpdateStage {
        self.failed_stage
    }

    pub const fn recovery_disposition(&self) -> RecoveryDisposition {
        self.recovery_disposition
    }

    pub const fn recovery_completed(&self) -> bool {
        self.recovery_completed
    }

    pub const fn stage_error(&self) -> &E {
        &self.source
    }

    pub const fn recovery_error(&self) -> Option<&E> {
        self.recovery_error.as_ref()
    }
}

impl<E> fmt::Display for UpdateExecutionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SERVICE_UPDATE_STAGE_FAILED")
    }
}

impl<E> std::error::Error for UpdateExecutionError<E> where E: fmt::Debug + fmt::Display {}

const fn recovery_for(stage: UpdateStage) -> RecoveryDisposition {
    match stage {
        UpdateStage::Download | UpdateStage::VerifyDownloadedArtifact => {
            RecoveryDisposition::PreserveCurrentAuthority
        }
        UpdateStage::StopClaims
        | UpdateStage::Drain
        | UpdateStage::ControlBackup
        | UpdateStage::CompatibilityCheck => RecoveryDisposition::ResumeCurrentBuild,
        UpdateStage::StopService
        | UpdateStage::VerifyStagedArtifact
        | UpdateStage::ReplaceBinary => RecoveryDisposition::RestorePreviousBinary,
        UpdateStage::MigrateSchema
        | UpdateStage::StartService
        | UpdateStage::Reconcile
        | UpdateStage::CommitMinimumReader
        | UpdateStage::ResumeClaims => RecoveryDisposition::ForwardFixClaimsStopped,
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ReaderVersion(semver::Version);

impl ReaderVersion {
    pub fn parse(value: &str) -> Result<Self, ReaderCompatibilityError> {
        semver::Version::parse(value)
            .map(Self)
            .map_err(|_| ReaderCompatibilityError::InvalidVersion)
    }

    pub fn ensure_at_least(&self, minimum: &Self) -> Result<(), ReaderCompatibilityError> {
        if self < minimum {
            Err(ReaderCompatibilityError::BelowMinimum)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderCompatibilityError {
    InvalidVersion,
    BelowMinimum,
}

impl fmt::Display for ReaderCompatibilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidVersion => "SERVICE_READER_VERSION_INVALID",
            Self::BelowMinimum => "SERVICE_MINIMUM_READER_VERSION_UNMET",
        })
    }
}

impl std::error::Error for ReaderCompatibilityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSupportStatus {
    Experimental,
    Candidate,
    Supported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderPromotionEvidence {
    provider: String,
    revision: String,
    build_id: String,
    evidence_builds: [String; 4],
    passed: [bool; 4],
}

impl ProviderPromotionEvidence {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: impl Into<String>,
        revision: impl Into<String>,
        build_id: impl Into<String>,
        backup_passed: bool,
        restore_passed: bool,
        reader_passed: bool,
        conformance_passed: bool,
    ) -> Self {
        let build_id = build_id.into();
        Self {
            provider: provider.into(),
            revision: revision.into(),
            evidence_builds: std::array::from_fn(|_| build_id.clone()),
            build_id,
            passed: [
                backup_passed,
                restore_passed,
                reader_passed,
                conformance_passed,
            ],
        }
    }

    pub fn new_with_evidence_builds(
        provider: impl Into<String>,
        revision: impl Into<String>,
        build_id: impl Into<String>,
        evidence_builds: [&str; 4],
        passed: [bool; 4],
    ) -> Self {
        Self {
            provider: provider.into(),
            revision: revision.into(),
            build_id: build_id.into(),
            evidence_builds: evidence_builds.map(str::to_owned),
            passed,
        }
    }
}

pub fn evaluate_provider_promotion(evidence: &ProviderPromotionEvidence) -> ProviderSupportStatus {
    if evidence.provider.trim().is_empty()
        || evidence.revision.trim().is_empty()
        || evidence.build_id.trim().is_empty()
    {
        return ProviderSupportStatus::Experimental;
    }
    if evidence.passed.into_iter().all(|passed| passed)
        && evidence
            .evidence_builds
            .iter()
            .all(|build| build == &evidence.build_id)
    {
        ProviderSupportStatus::Supported
    } else {
        ProviderSupportStatus::Candidate
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RolloutDecision {
    Continue,
    Halt,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RolloutObservation {
    unexplained_missed_occurrences: u64,
    duplicate_recovery_points: u64,
    stale_fence_commits: u64,
    secret_canary_matches: u64,
    queue_freshness_ms: u64,
    restore_drill_pass_rate: f64,
    support_bundle_failures: u64,
}

impl RolloutObservation {
    pub fn healthy(queue_freshness_ms: u64, restore_drill_pass_rate: f64) -> Self {
        Self {
            unexplained_missed_occurrences: 0,
            duplicate_recovery_points: 0,
            stale_fence_commits: 0,
            secret_canary_matches: 0,
            queue_freshness_ms,
            restore_drill_pass_rate,
            support_bundle_failures: 0,
        }
    }

    pub const fn with_stale_fence_commits(mut self, count: u64) -> Self {
        self.stale_fence_commits = count;
        self
    }

    pub const fn with_queue_freshness_ms(mut self, freshness_ms: u64) -> Self {
        self.queue_freshness_ms = freshness_ms;
        self
    }

    pub const fn with_restore_drill_pass_rate(mut self, pass_rate: f64) -> Self {
        self.restore_drill_pass_rate = pass_rate;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RolloutPolicy {
    max_queue_freshness_ms: u64,
    minimum_restore_drill_pass_rate: f64,
    max_support_bundle_failures: u64,
}

impl RolloutPolicy {
    pub const fn new(
        max_queue_freshness_ms: u64,
        minimum_restore_drill_pass_rate: f64,
        max_support_bundle_failures: u64,
    ) -> Self {
        Self {
            max_queue_freshness_ms,
            minimum_restore_drill_pass_rate,
            max_support_bundle_failures,
        }
    }

    pub fn evaluate(&self, observation: &RolloutObservation) -> RolloutDecision {
        let safety_violation = observation.unexplained_missed_occurrences != 0
            || observation.duplicate_recovery_points != 0
            || observation.stale_fence_commits != 0
            || observation.secret_canary_matches != 0;
        let stop_metric = observation.queue_freshness_ms > self.max_queue_freshness_ms
            || !observation.restore_drill_pass_rate.is_finite()
            || observation.restore_drill_pass_rate < self.minimum_restore_drill_pass_rate
            || observation.support_bundle_failures > self.max_support_bundle_failures;
        if safety_violation || stop_metric {
            RolloutDecision::Halt
        } else {
            RolloutDecision::Continue
        }
    }
}
