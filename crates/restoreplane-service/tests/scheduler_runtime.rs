use restoreplane_catalog::{
    ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec, OperationalStore, PlanDraftSpec,
};
use restoreplane_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
    PlanRevisionId,
};
use restoreplane_service::scheduler::ServiceScheduler;
use tempfile::TempDir;

#[test]
fn recovery_tick_materializes_only_the_latest_due_and_coalesces_while_work_is_active() {
    let fixture = Fixture::new();
    let scheduler = ServiceScheduler::new(fixture.store.clone());
    let first_tick_ms = 1_735_961_400_000;

    let recovered = scheduler
        .run_once(first_tick_ms)
        .expect("recovery scheduler tick");
    assert_eq!(recovered.plans_evaluated(), 1);
    assert_eq!(recovered.plans_coalesced(), 0);
    assert_eq!(recovered.created_occurrences(), 4);
    assert_eq!(recovered.created_work_items(), 1);
    assert_eq!(recovered.missed_occurrences(), 3);

    let next_tick_ms = 1_736_047_800_000;
    let coalesced = scheduler
        .run_once(next_tick_ms)
        .expect("overlap scheduler tick");
    assert_eq!(coalesced.plans_evaluated(), 1);
    assert_eq!(coalesced.plans_coalesced(), 1);
    assert_eq!(coalesced.created_occurrences(), 0);
    assert_eq!(coalesced.created_work_items(), 0);

    let direct = restoreplane_catalog::rusqlite::Connection::open(&fixture.db_path)
        .expect("open scheduler evidence");
    let cursor: (i64, Option<i64>) = direct
        .query_row(
            "SELECT last_evaluated_at_ms, last_materialized_for_ms
             FROM scheduler_cursors
             WHERE plan_revision_id = ?1",
            [fixture.revision_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("durable cursor");
    assert_eq!(cursor, (first_tick_ms, Some(1_735_959_600_000)));

    let counts: (i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM plan_occurrences),
                 (SELECT COUNT(*) FROM backup_runs),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'backup')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("bounded cardinality");
    assert_eq!(counts, (4, 1, 1));
}

#[test]
fn recovery_older_than_the_catchup_window_advances_in_bounded_audited_chunks() {
    let fixture = Fixture::new();
    let scheduler = ServiceScheduler::new(fixture.store.clone());
    let published_at_ms = 1_735_689_600_000;
    let now_ms = published_at_ms + 40 * 24 * 60 * 60 * 1_000 + 4 * 60 * 60 * 1_000;

    let first = scheduler
        .run_once(now_ms)
        .expect("first bounded recovery tick");
    assert_eq!(first.created_work_items(), 0);
    assert_eq!(first.created_occurrences(), 31);
    assert_eq!(first.missed_occurrences(), 31);

    let direct = restoreplane_catalog::rusqlite::Connection::open(&fixture.db_path)
        .expect("open bounded recovery evidence");
    let first_cursor: i64 = direct
        .query_row(
            "SELECT last_evaluated_at_ms
             FROM scheduler_cursors
             WHERE plan_revision_id = ?1",
            [fixture.revision_id.as_str()],
            |row| row.get(0),
        )
        .expect("first bounded cursor");
    assert_eq!(first_cursor, published_at_ms + 31 * 24 * 60 * 60 * 1_000);
    assert!(first_cursor < now_ms);

    let second = scheduler
        .run_once(now_ms)
        .expect("second bounded recovery tick");
    assert_eq!(second.created_work_items(), 1);
    assert_eq!(second.created_occurrences(), 10);
    assert_eq!(second.missed_occurrences(), 9);

    let evidence: (i64, i64, i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT last_evaluated_at_ms
                    FROM scheduler_cursors
                   WHERE plan_revision_id = ?1),
                 (SELECT COUNT(*) FROM plan_occurrences),
                 (SELECT COUNT(*) FROM plan_occurrences WHERE state = 'missed'),
                 (SELECT COUNT(*) FROM work_items WHERE kind = 'backup')",
            [fixture.revision_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("complete bounded recovery evidence");
    assert_eq!(evidence, (now_ms, 41, 40, 1));
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
        let store = OperationalStore::open(db_path.clone()).expect("operational store");
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
                1_735_689_000_000,
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
                1_735_689_100_000,
            ))
            .expect("target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "scheduler-vault",
                r#"{"path":"D:\\backup"}"#,
                1_735_689_200_000,
            ))
            .expect("destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::cron(
                plan_id,
                revision_id.clone(),
                "nightly",
                1,
                "0 3 * * *",
                "UTC",
                "a".repeat(64),
                1_735_689_300_000,
            ))
            .expect("plan");
        configuration
            .add_revision_target(&revision_id, &target_id, 1_735_689_400_000)
            .expect("target membership");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                1_735_689_500_000,
            )
            .expect("destination membership");
        configuration
            .publish_revision(&revision_id, 1_735_689_600_000)
            .expect("publish");

        Self {
            _temp: temp,
            db_path,
            store,
            revision_id,
        }
    }
}
