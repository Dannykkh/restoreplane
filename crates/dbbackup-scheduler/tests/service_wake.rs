use dbbackup_catalog::{
    ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec, OperationalStore, PlanDraftSpec,
};
use dbbackup_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
    PlanRevisionId,
};
use dbbackup_scheduler::occurrence::{evaluate_due_window, CatchUpPolicy, WakeMode};
use dbbackup_scheduler::service_wake::ServiceWake;
use dbbackup_scheduler::ScheduleSpec;
use tempfile::TempDir;

#[test]
fn repeated_service_wake_materializes_one_occurrence_and_one_work_per_target() {
    let fixture = Fixture::new();
    let evaluation = evaluate_due_window(
        &ScheduleSpec::Daily { hour: 3, minute: 0 },
        "UTC",
        1_735_948_800_000,
        1_735_963_200_000,
        WakeMode::Regular,
        CatchUpPolicy::BoundedLatestOne,
    )
    .expect("one regular due");
    assert_eq!(evaluation.due().len(), 1);

    let wake = ServiceWake::new(fixture.store.runtime(), fixture.revision_id.clone());
    let first = wake
        .materialize(&evaluation, 1_735_963_200_000)
        .expect("materialize first tick");
    assert_eq!(first.created_occurrences(), 1);
    assert_eq!(first.created_backup_runs(), 2);
    assert_eq!(first.created_work_items(), 2);
    assert_eq!(first.missed_occurrences(), 0);

    let replay = wake
        .materialize(&evaluation, 1_735_963_200_000)
        .expect("replay identical tick");
    assert_eq!(replay.created_occurrences(), 0);
    assert_eq!(replay.created_backup_runs(), 0);
    assert_eq!(replay.created_work_items(), 0);

    let direct = dbbackup_catalog::rusqlite::Connection::open(&fixture.db_path)
        .expect("open materialization probe");
    let counts: (i64, i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM plan_occurrences),
                 (SELECT COUNT(*) FROM backup_runs),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'backup'),
                 (
                     SELECT COUNT(*)
                     FROM (
                         SELECT plan_occurrence_id, database_target_id
                         FROM backup_runs
                         GROUP BY plan_occurrence_id, database_target_id
                     )
                 )",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query exact cardinality");
    assert_eq!(counts, (1, 2, 2, 2));
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    revision_id: PlanRevisionId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        let configuration = store.configuration();
        let connection_id = ConnectionProfileId::new();
        let first_target_id = DatabaseTargetId::new();
        let second_target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();

        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_id.clone(),
                "wake-postgresql",
                "postgresql",
                "db.internal",
                5432,
                1_000,
            ))
            .expect("insert connection");
        for (target_id, object_id, name, fingerprint) in [
            (
                first_target_id.clone(),
                "oid:1",
                "orders",
                "fingerprint-orders",
            ),
            (
                second_target_id.clone(),
                "oid:2",
                "billing",
                "fingerprint-billing",
            ),
        ] {
            configuration
                .insert_target(DatabaseTargetSpec::new(
                    target_id,
                    connection_id.clone(),
                    object_id,
                    name,
                    name,
                    fingerprint,
                    1_010,
                ))
                .expect("insert target");
        }
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "wake-vault",
                r#"{"path":"D:\\backup"}"#,
                1_020,
            ))
            .expect("insert destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::cron(
                plan_id,
                revision_id.clone(),
                "nightly",
                1,
                "0 3 * * *",
                "UTC",
                "a".repeat(64),
                1_030,
            ))
            .expect("insert scheduled plan");
        configuration
            .add_revision_target(&revision_id, &first_target_id, 1_031)
            .expect("add first target");
        configuration
            .add_revision_target(&revision_id, &second_target_id, 1_032)
            .expect("add second target");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                1_033,
            )
            .expect("add destination");
        configuration
            .publish_revision(&revision_id, 1_040)
            .expect("publish revision");

        Self {
            _temp: temp,
            db_path,
            store,
            revision_id,
        }
    }
}
