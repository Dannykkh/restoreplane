use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use restoreplane_pipeline::{
    evaluate_retention_v2, execute_retention_delete_v2, DeleteErrorV2, DeleteObjectDispositionV2,
    DestinationDeleteV2, ImmutabilityKind, RetentionCandidateV2, RetentionDeleteAuthorizationV2,
    RetentionDeletePlanV2, RetentionDispositionV2, RetentionObjectRoleV2, RetentionObjectV2,
    RetentionReasonV2,
};
use uuid::Uuid;

#[test]
fn dry_run_preserves_every_safety_blocker_and_never_implies_deletion() {
    let cases = [
        (
            base_candidate().with_pinned(true),
            RetentionReasonV2::Pinned,
        ),
        (
            base_candidate().with_legal_hold(true),
            RetentionReasonV2::LegalHold,
        ),
        (
            base_candidate()
                .with_immutability(ImmutabilityKind::ObjectLockCompliance)
                .with_retention_until_ms(Some(2_000)),
            RetentionReasonV2::ObjectLockActive,
        ),
        (
            base_candidate().with_last_compliant(true),
            RetentionReasonV2::LastCompliantRecoveryPoint,
        ),
        (
            base_candidate().with_active_restore_source(true),
            RetentionReasonV2::ActiveRestoreSource,
        ),
        (
            base_candidate().with_active_repair_source(true),
            RetentionReasonV2::ActiveRepairSource,
        ),
    ];

    for (candidate, expected_reason) in cases {
        let decision = evaluate_retention_v2(&candidate, 1_000, 300).expect("dry-run");
        assert_eq!(decision.disposition, RetentionDispositionV2::Preserve);
        assert!(decision.reasons.contains(&expected_reason));
    }
}

#[test]
fn expired_candidate_must_be_tombstoned_and_wait_for_grace_before_delete() {
    let candidate = base_candidate();
    let tombstone = evaluate_retention_v2(&candidate, 1_000, 300).expect("initial dry-run");
    assert_eq!(
        tombstone.disposition,
        RetentionDispositionV2::TombstoneRequired
    );
    assert!(tombstone.reasons.is_empty());

    let waiting = evaluate_retention_v2(
        &candidate.clone().with_tombstoned_at_ms(Some(900)),
        1_000,
        300,
    )
    .expect("grace dry-run");
    assert_eq!(waiting.disposition, RetentionDispositionV2::WaitForGrace);
    assert_eq!(waiting.eligible_after_ms, Some(1_200));

    let eligible = evaluate_retention_v2(&candidate.with_tombstoned_at_ms(Some(600)), 1_000, 300)
        .expect("delete eligibility dry-run");
    assert_eq!(eligible.disposition, RetentionDispositionV2::DeleteEligible);
}

#[test]
fn predelete_plan_can_be_previewed_without_becoming_delete_authority() {
    let plan = RetentionDeletePlanV2::prepare(
        &base_candidate(),
        vec![
            RetentionObjectV2::new("set-manifest.json", RetentionObjectRoleV2::SetManifest)
                .expect("manifest"),
        ],
        1_000,
        300,
    )
    .expect("predelete plan");

    let preview = plan.preview_json().expect("preview JSON");
    assert!(preview.contains("\"disposition\":\"tombstone_required\""));
    let error = RetentionDeleteAuthorizationV2::from_json(&preview)
        .expect_err("a preview must not become executable delete authority");
    assert_eq!(error.code(), "RETENTION_DELETE_NOT_ELIGIBLE");
}

#[test]
fn last_compliant_candidate_reports_the_compliance_impact_explicitly() {
    let decision = evaluate_retention_v2(&base_candidate().with_last_compliant(true), 1_000, 300)
        .expect("dry-run");
    assert!(decision.would_break_compliance);
    assert_eq!(decision.disposition, RetentionDispositionV2::Preserve);
}

#[tokio::test]
async fn confirmed_delete_is_manifest_first_and_records_partial_results_truthfully() {
    let candidate = base_candidate().with_tombstoned_at_ms(Some(600));
    let plan = RetentionDeletePlanV2::prepare(
        &candidate,
        vec![
            RetentionObjectV2::new("parts/retry.enc", RetentionObjectRoleV2::Payload)
                .expect("retry object"),
            RetentionObjectV2::new("set-manifest.json", RetentionObjectRoleV2::SetManifest)
                .expect("manifest"),
            RetentionObjectV2::new("parts/locked.enc", RetentionObjectRoleV2::Payload)
                .expect("locked object"),
            RetentionObjectV2::new("parts/deleted.enc", RetentionObjectRoleV2::Payload)
                .expect("deletable object"),
        ],
        1_000,
        300,
    )
    .expect("delete plan");
    let digest = plan.digest_blake3().to_owned();
    let confirmed = plan.confirm(&digest).expect("bound confirmation");
    let operator = RecordingDeleteOperator::default();

    let report = execute_retention_delete_v2(&confirmed, &operator).await;

    assert_eq!(
        operator.order.lock().expect("order")[0],
        "set-manifest.json",
        "commit marker must disappear before payload deletion"
    );
    assert_eq!(report.deleted_count(), 2);
    assert_eq!(report.retained_by_lock_count(), 1);
    assert_eq!(report.retry_required_count(), 1);
    assert!(report.observations.iter().any(|observation| {
        observation.object_key == "parts/locked.enc"
            && observation.disposition == DeleteObjectDispositionV2::RetainedByLock
    }));
    assert_eq!(report.retry_object_keys(), ["parts/retry.enc"]);
}

#[tokio::test]
async fn failed_set_manifest_delete_blocks_every_remaining_object() {
    let cases = [
        (
            MarkerFailure::Retryable,
            DeleteObjectDispositionV2::RetryRequired,
            "not_attempted:set_manifest_delete_retry_required",
        ),
        (
            MarkerFailure::Locked,
            DeleteObjectDispositionV2::RetainedByLock,
            "not_attempted:set_manifest_delete_retained_by_lock",
        ),
        (
            MarkerFailure::Terminal,
            DeleteObjectDispositionV2::FailedTerminal,
            "not_attempted:set_manifest_delete_failed_terminal",
        ),
    ];

    for (failure, marker_disposition, blocked_diagnostic) in cases {
        let candidate = base_candidate().with_tombstoned_at_ms(Some(600));
        let plan = RetentionDeletePlanV2::prepare(
            &candidate,
            vec![
                RetentionObjectV2::new("parts/one.enc", RetentionObjectRoleV2::Payload)
                    .expect("first payload"),
                RetentionObjectV2::new("set-manifest.json", RetentionObjectRoleV2::SetManifest)
                    .expect("manifest"),
                RetentionObjectV2::new("parts/two.enc", RetentionObjectRoleV2::Payload)
                    .expect("second payload"),
            ],
            1_000,
            300,
        )
        .expect("delete plan");
        let digest = plan.digest_blake3().to_owned();
        let confirmed = plan.confirm(&digest).expect("bound confirmation");
        let operator = MarkerFailureDeleteOperator::new(failure);

        let report = execute_retention_delete_v2(&confirmed, &operator).await;

        assert_eq!(
            *operator.order.lock().expect("delete order"),
            vec!["set-manifest.json"],
            "payload deletion must not be attempted until the commit marker is gone"
        );
        assert_eq!(report.observations.len(), 3);
        assert_eq!(report.observations[0].disposition, marker_disposition);
        assert_eq!(report.blocked_count(), 2);
        assert!(report.observations[1..].iter().all(|observation| {
            observation.disposition == DeleteObjectDispositionV2::NotAttemptedMarkerBlocked
                && observation.diagnostic.as_deref() == Some(blocked_diagnostic)
        }));
        let expected_retry_keys = if failure == MarkerFailure::Retryable {
            vec!["set-manifest.json"]
        } else {
            Vec::new()
        };
        assert_eq!(report.retry_object_keys(), expected_retry_keys);
    }
}

#[test]
fn dry_run_digest_mismatch_cannot_authorize_a_different_delete_plan() {
    let plan = RetentionDeletePlanV2::prepare(
        &base_candidate().with_tombstoned_at_ms(Some(600)),
        vec![
            RetentionObjectV2::new("set-manifest.json", RetentionObjectRoleV2::SetManifest)
                .expect("manifest"),
        ],
        1_000,
        300,
    )
    .expect("plan");
    let error = plan
        .confirm(&"f".repeat(64))
        .expect_err("confirmation must bind the exact dry-run");
    assert_eq!(error.code(), "RETENTION_CONFIRMATION_MISMATCH");
}

#[test]
fn delete_secret_version_is_part_of_the_confirmed_authorization_digest() {
    let candidate = base_candidate().with_tombstoned_at_ms(Some(600));
    let objects =
        vec![
            RetentionObjectV2::new("set-manifest.json", RetentionObjectRoleV2::SetManifest)
                .expect("manifest"),
        ];
    let secret_a = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let secret_b = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    let plan_a = RetentionDeletePlanV2::prepare_with_delete_secret_ref(
        &candidate,
        objects.clone(),
        1_000,
        300,
        Some(secret_a),
    )
    .expect("plan A");
    let plan_b = RetentionDeletePlanV2::prepare_with_delete_secret_ref(
        &candidate,
        objects,
        1_000,
        300,
        Some(secret_b),
    )
    .expect("plan B");

    assert_ne!(plan_a.digest_blake3(), plan_b.digest_blake3());
    let digest = plan_a.digest_blake3().to_owned();
    assert_eq!(
        plan_a
            .confirm(&digest)
            .expect("confirmed plan")
            .authorization()
            .delete_secret_ref_id(),
        Some(secret_a)
    );
}

#[test]
fn delete_scope_allows_only_server_evaluation_time_to_advance() {
    let candidate = base_candidate().with_tombstoned_at_ms(Some(600));
    let objects =
        vec![
            RetentionObjectV2::new("set-manifest.json", RetentionObjectRoleV2::SetManifest)
                .expect("manifest"),
        ];
    let first = RetentionDeletePlanV2::prepare(&candidate, objects.clone(), 1_000, 300)
        .expect("first authorization");
    let later = RetentionDeletePlanV2::prepare(&candidate, objects.clone(), 1_001, 300)
        .expect("later authorization");
    let changed_object = RetentionDeletePlanV2::prepare(
        &candidate,
        vec![
            RetentionObjectV2::new("set-manifest.json", RetentionObjectRoleV2::SetManifest)
                .expect("manifest"),
            RetentionObjectV2::new("parts/new.enc", RetentionObjectRoleV2::Payload)
                .expect("new object"),
        ],
        1_001,
        300,
    )
    .expect("changed object authorization");

    assert_ne!(first.digest_blake3(), later.digest_blake3());
    assert!(first
        .authorization()
        .has_same_delete_scope(later.authorization())
        .expect("validated scope"));
    assert!(!first
        .authorization()
        .has_same_delete_scope(changed_object.authorization())
        .expect("validated changed scope"));
}

#[test]
fn one_physical_object_cannot_be_authorized_under_multiple_roles() {
    let error = RetentionDeletePlanV2::prepare(
        &base_candidate().with_tombstoned_at_ms(Some(600)),
        vec![
            RetentionObjectV2::new("shared.bin", RetentionObjectRoleV2::SetManifest)
                .expect("set manifest identity"),
            RetentionObjectV2::new("parts/middle.json", RetentionObjectRoleV2::PartManifest)
                .expect("intervening part manifest"),
            RetentionObjectV2::new("shared.bin", RetentionObjectRoleV2::Payload)
                .expect("duplicated payload identity"),
        ],
        1_000,
        300,
    )
    .expect_err("a key/version identity must have exactly one role");
    assert_eq!(error.code(), "RETENTION_OBJECT_SET_INVALID");
}

#[tokio::test]
async fn versioned_object_identity_is_bound_to_confirmation_and_delete_observation() {
    let candidate = base_candidate().with_tombstoned_at_ms(Some(600));
    let plan_a = RetentionDeletePlanV2::prepare(
        &candidate,
        vec![RetentionObjectV2::versioned(
            "set-manifest.json",
            "version-a",
            RetentionObjectRoleV2::SetManifest,
        )
        .expect("versioned manifest")],
        1_000,
        300,
    )
    .expect("versioned plan A");
    let plan_b = RetentionDeletePlanV2::prepare(
        &candidate,
        vec![RetentionObjectV2::versioned(
            "set-manifest.json",
            "version-b",
            RetentionObjectRoleV2::SetManifest,
        )
        .expect("versioned manifest")],
        1_000,
        300,
    )
    .expect("versioned plan B");
    assert_ne!(
        plan_a.digest_blake3(),
        plan_b.digest_blake3(),
        "confirmation must bind an exact object version"
    );

    let digest = plan_a.digest_blake3().to_owned();
    let confirmed = plan_a.confirm(&digest).expect("version-bound confirmation");
    let report = execute_retention_delete_v2(&confirmed, &RecordingDeleteOperator::default()).await;
    assert_eq!(
        report.observations[0].object_version.as_deref(),
        Some("version-a")
    );
}

#[tokio::test]
async fn confirmed_authorization_round_trips_and_rejects_object_set_tampering() {
    let candidate = base_candidate().with_tombstoned_at_ms(Some(600));
    let plan = RetentionDeletePlanV2::prepare(
        &candidate,
        vec![
            RetentionObjectV2::versioned(
                "set-manifest.json",
                "manifest-version",
                RetentionObjectRoleV2::SetManifest,
            )
            .expect("manifest"),
            RetentionObjectV2::versioned(
                "parts/orders.enc",
                "payload-version",
                RetentionObjectRoleV2::Payload,
            )
            .expect("payload"),
        ],
        1_000,
        300,
    )
    .expect("delete plan");
    let digest = plan.digest_blake3().to_owned();
    let confirmed = plan.confirm(&digest).expect("confirmed plan");
    let encoded = confirmed
        .authorization()
        .to_json()
        .expect("authorization JSON");
    let restored =
        RetentionDeleteAuthorizationV2::from_json(&encoded).expect("validated authorization");
    let rebound = restored
        .confirm(
            &digest,
            uuid("11111111-1111-4111-8111-111111111111"),
            "local:vault-a",
        )
        .expect("rebind durable authorization");
    let report = execute_retention_delete_v2(&rebound, &RecordingDeleteOperator::default()).await;
    assert_eq!(report.deleted_count(), 2);
    assert_eq!(
        report.observations[0].object_version.as_deref(),
        Some("manifest-version")
    );

    let tampered = encoded.replace("payload-version", "payload-version-tampered");
    let error = RetentionDeleteAuthorizationV2::from_json(&tampered)
        .expect_err("object-set mutation must invalidate the authorization digest");
    assert_eq!(error.code(), "RETENTION_CONFIRMATION_MISMATCH");
}

fn base_candidate() -> RetentionCandidateV2 {
    RetentionCandidateV2::new(
        uuid("11111111-1111-4111-8111-111111111111"),
        "local:vault-a",
        100,
        true,
    )
    .expect("candidate")
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("fixture UUID")
}

#[derive(Default)]
struct RecordingDeleteOperator {
    order: Arc<Mutex<Vec<String>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkerFailure {
    Retryable,
    Locked,
    Terminal,
}

struct MarkerFailureDeleteOperator {
    order: Arc<Mutex<Vec<String>>>,
    failure: MarkerFailure,
}

impl MarkerFailureDeleteOperator {
    fn new(failure: MarkerFailure) -> Self {
        Self {
            order: Arc::new(Mutex::new(Vec::new())),
            failure,
        }
    }
}

#[async_trait]
impl DestinationDeleteV2 for MarkerFailureDeleteOperator {
    async fn delete_object(&self, key: &str, _version: Option<&str>) -> Result<(), DeleteErrorV2> {
        self.order.lock().expect("order").push(key.to_owned());
        if key == "set-manifest.json" {
            return Err(match self.failure {
                MarkerFailure::Retryable => DeleteErrorV2::Backend {
                    key: key.to_owned(),
                    message: "transient marker failure".to_owned(),
                    retryable: true,
                },
                MarkerFailure::Locked => DeleteErrorV2::RetentionLocked {
                    key: key.to_owned(),
                    retention_until_ms: Some(2_000),
                },
                MarkerFailure::Terminal => DeleteErrorV2::Backend {
                    key: key.to_owned(),
                    message: "terminal marker failure".to_owned(),
                    retryable: false,
                },
            });
        }
        Ok(())
    }
}

#[async_trait]
impl DestinationDeleteV2 for RecordingDeleteOperator {
    async fn delete_object(&self, key: &str, _version: Option<&str>) -> Result<(), DeleteErrorV2> {
        self.order.lock().expect("order").push(key.to_owned());
        if key.contains("locked") {
            return Err(DeleteErrorV2::RetentionLocked {
                key: key.to_owned(),
                retention_until_ms: Some(2_000),
            });
        }
        if key.contains("retry") {
            return Err(DeleteErrorV2::Backend {
                key: key.to_owned(),
                message: "transient fixture failure".to_owned(),
                retryable: true,
            });
        }
        Ok(())
    }
}
