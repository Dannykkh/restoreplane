use dbbackup_catalog::{
    ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec, OccurrenceMaterializationSpec,
    OperationalStore, PlanDraftSpec, SchedulerCursorAdvanceSpec,
};
use dbbackup_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
    OccurrenceOrigin, PlanRevisionId,
};
use tempfile::TempDir;

#[test]
fn enabled_published_plan_and_its_durable_monotonic_cursor_are_projected_for_the_engine() {
    let fixture = Fixture::new();
    let scheduler = fixture.store.scheduler();

    let initial = scheduler
        .scheduled_plans()
        .expect("scheduled plans before first tick");
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].plan_revision_id(), &fixture.revision_id);
    assert_eq!(initial[0].schedule_expression(), "15 * * * *");
    assert_eq!(initial[0].timezone(), "UTC");
    assert_eq!(initial[0].catchup_policy(), "bounded_latest_one");
    assert_eq!(initial[0].overlap_policy(), "coalesce_one");
    assert_eq!(initial[0].published_at_ms(), 1_000);
    assert_eq!(initial[0].last_evaluated_at_ms(), None);
    assert!(!initial[0].has_active_backup_work());

    let advanced = scheduler
        .advance_cursor(SchedulerCursorAdvanceSpec::success(
            fixture.revision_id.clone(),
            2_000,
            Some(1_900),
        ))
        .expect("advance cursor");
    assert!(advanced.advanced());

    let stale = scheduler
        .advance_cursor(SchedulerCursorAdvanceSpec::success(
            fixture.revision_id.clone(),
            1_500,
            Some(1_400),
        ))
        .expect("stale cursor is rejected without regression");
    assert!(!stale.advanced());

    fixture
        .store
        .runtime()
        .materialize_occurrence(OccurrenceMaterializationSpec::due(
            fixture.revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            2_100,
            2_100,
        ))
        .expect("active scheduled work");

    let current = scheduler
        .scheduled_plans()
        .expect("scheduled plans after cursor and work");
    assert_eq!(current[0].last_evaluated_at_ms(), Some(2_000));
    assert_eq!(current[0].last_materialized_for_ms(), Some(1_900));
    assert!(current[0].has_active_backup_work());
}

struct Fixture {
    _temp: TempDir,
    store: OperationalStore,
    revision_id: PlanRevisionId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let store =
            OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
        let configuration = store.configuration();
        let connection_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();

        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_id.clone(),
                "scheduler-postgresql",
                "postgresql",
                "db.internal",
                5432,
                900,
            ))
            .expect("connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_id,
                "oid:1",
                "orders",
                "orders",
                "orders-fingerprint",
                910,
            ))
            .expect("target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "scheduler-vault",
                r#"{"path":"D:\\backup"}"#,
                920,
            ))
            .expect("destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::cron(
                plan_id,
                revision_id.clone(),
                "hourly",
                1,
                "15 * * * *",
                "UTC",
                "a".repeat(64),
                930,
            ))
            .expect("plan");
        configuration
            .add_revision_target(&revision_id, &target_id, 940)
            .expect("target membership");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                950,
            )
            .expect("destination membership");
        configuration
            .publish_revision(&revision_id, 1_000)
            .expect("publish");

        Self {
            _temp: temp,
            store,
            revision_id,
        }
    }
}
