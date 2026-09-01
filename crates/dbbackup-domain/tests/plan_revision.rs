use uuid::uuid;

use dbbackup_domain::{
    BackupPlanId, DatabaseTargetId, DestinationId, DestinationRequirement, OccurrenceIdentity,
    OccurrenceOrigin, PlanRevisionDraft, PlanRevisionId,
};

#[test]
fn publish_requires_target_destination_and_required_membership() {
    let plan_id = BackupPlanId::new();
    let revision_id = PlanRevisionId::new();
    let target_id = DatabaseTargetId::new();
    let destination_id = DestinationId::new();

    let no_target = PlanRevisionDraft::new(revision_id.clone(), plan_id.clone())
        .with_destination(destination_id.clone(), DestinationRequirement::Required)
        .expect("add destination")
        .publish(100)
        .expect_err("publish requires a target");
    assert_eq!(no_target.code(), "PLAN_REVISION_TARGET_REQUIRED");

    let no_destination = PlanRevisionDraft::new(revision_id.clone(), plan_id.clone())
        .with_target(target_id.clone())
        .expect("add target")
        .publish(100)
        .expect_err("publish requires a destination");
    assert_eq!(no_destination.code(), "PLAN_REVISION_DESTINATION_REQUIRED");

    let best_effort_only = PlanRevisionDraft::new(revision_id, plan_id)
        .with_target(target_id)
        .expect("add target")
        .with_destination(destination_id, DestinationRequirement::BestEffort)
        .expect("add destination")
        .publish(100)
        .expect_err("publish requires one required destination");
    assert_eq!(
        best_effort_only.code(),
        "PLAN_REVISION_REQUIRED_DESTINATION_REQUIRED"
    );
}

#[test]
fn published_revision_exposes_an_immutable_validated_snapshot() {
    let plan_id = BackupPlanId::new();
    let revision_id = PlanRevisionId::new();
    let target_id = DatabaseTargetId::new();
    let required_destination = DestinationId::new();
    let best_effort_destination = DestinationId::new();

    let published = PlanRevisionDraft::new(revision_id.clone(), plan_id.clone())
        .with_target(target_id.clone())
        .expect("add target")
        .with_destination(
            required_destination.clone(),
            DestinationRequirement::Required,
        )
        .expect("add required destination")
        .with_destination(
            best_effort_destination.clone(),
            DestinationRequirement::BestEffort,
        )
        .expect("add best-effort destination")
        .publish(1_000)
        .expect("publish valid revision");

    assert_eq!(published.id(), &revision_id);
    assert_eq!(published.plan_id(), &plan_id);
    assert_eq!(published.published_at_ms(), 1_000);
    assert_eq!(published.targets(), &[target_id]);
    assert_eq!(published.destinations().len(), 2);
    assert!(published
        .destinations()
        .iter()
        .any(|item| item.destination_id() == &required_destination
            && item.requirement() == DestinationRequirement::Required));

    let duplicate_target = PlanRevisionDraft::new(PlanRevisionId::new(), BackupPlanId::new())
        .with_target(DatabaseTargetId::new())
        .expect("add first target");
    let existing_target = duplicate_target.targets()[0].clone();
    assert_eq!(
        duplicate_target
            .with_target(existing_target)
            .expect_err("duplicate target must be rejected")
            .code(),
        "PLAN_REVISION_TARGET_DUPLICATE"
    );
}

#[test]
fn occurrence_identity_matches_partial_unique_indexes() {
    let revision_id = PlanRevisionId::new();
    let scheduled = OccurrenceIdentity::scheduled(
        revision_id.clone(),
        OccurrenceOrigin::Scheduled,
        1_700_000_000_000,
    )
    .expect("scheduled identity");
    let catch_up =
        OccurrenceIdentity::scheduled(revision_id, OccurrenceOrigin::CatchUp, 1_700_000_000_000)
            .expect("catch-up identity");
    assert_eq!(
        scheduled, catch_up,
        "scheduled and catch-up share revision+UTC identity"
    );

    let manual = OccurrenceIdentity::manual("request-42").expect("manual identity");
    assert_ne!(scheduled, manual);
    assert_eq!(
        OccurrenceIdentity::manual("  ")
            .expect_err("blank manual key")
            .code(),
        "OCCURRENCE_IDEMPOTENCY_KEY_INVALID"
    );
    assert_eq!(
        OccurrenceIdentity::scheduled(
            PlanRevisionId::new(),
            OccurrenceOrigin::Manual,
            1_700_000_000_000,
        )
        .expect_err("manual origin cannot use scheduled identity")
        .code(),
        "OCCURRENCE_ORIGIN_INVALID"
    );

    let namespace = uuid!("4802422f-1347-5d76-a6ff-2f9e4738d1b1");
    assert_eq!(
        scheduled.deterministic_id(namespace),
        catch_up.deterministic_id(namespace)
    );
    assert_ne!(
        scheduled.deterministic_id(namespace),
        manual.deterministic_id(namespace)
    );
}
