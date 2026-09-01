use restoreplane_service::update::{
    evaluate_provider_promotion, ProviderPromotionEvidence, ProviderSupportStatus,
    ReaderCompatibilityError, ReaderVersion, RecoveryDisposition, RolloutDecision,
    RolloutObservation, RolloutPolicy, UpdateCoordinator, UpdateExecutionError, UpdatePlatform,
    UpdateStage,
};

#[derive(Default)]
struct RecordingPlatform {
    failed_stage: Option<UpdateStage>,
    stages: Vec<UpdateStage>,
    recoveries: Vec<RecoveryDisposition>,
}

impl RecordingPlatform {
    fn failing_at(stage: UpdateStage) -> Self {
        Self {
            failed_stage: Some(stage),
            ..Self::default()
        }
    }
}

impl UpdatePlatform for RecordingPlatform {
    type Error = &'static str;

    fn perform(&mut self, stage: UpdateStage) -> Result<(), Self::Error> {
        self.stages.push(stage);
        if self.failed_stage == Some(stage) {
            Err("injected")
        } else {
            Ok(())
        }
    }

    fn recover(&mut self, disposition: RecoveryDisposition) -> Result<(), Self::Error> {
        self.recoveries.push(disposition);
        Ok(())
    }
}

#[test]
fn update_executes_the_fail_closed_release_sequence() {
    let mut platform = RecordingPlatform::default();
    let journal = UpdateCoordinator
        .execute(&mut platform)
        .expect("complete update");

    assert_eq!(platform.stages, UpdateStage::ORDER);
    assert_eq!(journal.completed_stages(), UpdateStage::ORDER.as_slice());
    assert_eq!(
        journal.recovery_disposition(),
        RecoveryDisposition::NotRequired
    );
    assert!(platform.recoveries.is_empty());
}

#[test]
fn every_update_fault_has_an_explicit_recovery_boundary() {
    for stage in UpdateStage::ORDER {
        let mut platform = RecordingPlatform::failing_at(stage);
        let error = UpdateCoordinator
            .execute(&mut platform)
            .expect_err("injected stage must stop the update");

        assert_eq!(error.failed_stage(), stage);
        assert_eq!(error.recovery_disposition(), expected_recovery(stage));
        assert!(error.recovery_completed());
        assert_eq!(platform.recoveries, [expected_recovery(stage)]);
        assert_eq!(platform.stages.last(), Some(&stage));
        assert!(!platform
            .stages
            .iter()
            .skip_while(|observed| **observed != stage)
            .skip(1)
            .any(|_| true));
    }
}

#[test]
fn minimum_reader_guard_rejects_old_or_malformed_binaries() {
    let minimum = ReaderVersion::parse("2.4.0").expect("minimum version");
    assert!(ReaderVersion::parse("2.4.0")
        .expect("same version")
        .ensure_at_least(&minimum)
        .is_ok());
    assert!(ReaderVersion::parse("2.5.1")
        .expect("newer version")
        .ensure_at_least(&minimum)
        .is_ok());
    assert_eq!(
        ReaderVersion::parse("2.3.9")
            .expect("old version")
            .ensure_at_least(&minimum),
        Err(ReaderCompatibilityError::BelowMinimum)
    );
    assert_eq!(
        ReaderVersion::parse("2.4"),
        Err(ReaderCompatibilityError::InvalidVersion)
    );
}

#[test]
fn minimum_reader_guard_accepts_semver_prereleases_and_orders_them_before_release() {
    let minimum = ReaderVersion::parse("2.4.0").expect("minimum version");
    let release_candidate = ReaderVersion::parse("2.4.0-rc.1").expect("valid release candidate");

    assert_eq!(
        release_candidate.ensure_at_least(&minimum),
        Err(ReaderCompatibilityError::BelowMinimum)
    );
}

#[test]
fn provider_promotion_requires_same_build_backup_restore_reader_and_conformance() {
    let complete =
        ProviderPromotionEvidence::new("sqlite", "revision-a", "build-a", true, true, true, true);
    assert_eq!(
        evaluate_provider_promotion(&complete),
        ProviderSupportStatus::Supported
    );

    let missing_restore =
        ProviderPromotionEvidence::new("mongodb", "revision-a", "build-a", true, false, true, true);
    assert_eq!(
        evaluate_provider_promotion(&missing_restore),
        ProviderSupportStatus::Candidate
    );

    let mixed_build = ProviderPromotionEvidence::new_with_evidence_builds(
        "firebird",
        "revision-a",
        "build-a",
        ["build-a", "build-b", "build-a", "build-a"],
        [true, true, true, true],
    );
    assert_eq!(
        evaluate_provider_promotion(&mixed_build),
        ProviderSupportStatus::Candidate
    );
}

#[test]
fn staged_rollout_halts_on_any_safety_invariant_or_configured_stop_metric() {
    let policy = RolloutPolicy::new(5_000, 0.95, 1);
    let healthy = RolloutObservation::healthy(1_000, 1.0);
    assert_eq!(policy.evaluate(&healthy), RolloutDecision::Continue);

    let stale_fence = healthy.clone().with_stale_fence_commits(1);
    assert_eq!(policy.evaluate(&stale_fence), RolloutDecision::Halt);

    let stale_queue = healthy.clone().with_queue_freshness_ms(5_001);
    assert_eq!(policy.evaluate(&stale_queue), RolloutDecision::Halt);

    let weak_restore_rate = healthy.with_restore_drill_pass_rate(0.90);
    assert_eq!(policy.evaluate(&weak_restore_rate), RolloutDecision::Halt);
}

fn expected_recovery(stage: UpdateStage) -> RecoveryDisposition {
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

fn _assert_error_is_public<E: std::error::Error>() {}

#[test]
fn update_error_is_a_stable_public_error_type() {
    _assert_error_is_public::<UpdateExecutionError<&'static str>>();
}
