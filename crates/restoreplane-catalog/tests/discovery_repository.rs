use restoreplane_catalog::{
    ConnectionProfileSpec, DatabaseTargetSpec, DiscoveredDatabaseSpec, DiscoverySnapshotSpec,
    OperationalStore,
};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn missing_discovery_observation_preserves_target_and_records_first_drift_time() {
    let temp_dir = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp_dir.path().join("operational.db"))
        .expect("open operational store");
    let repository = store.configuration();
    let connection_id = ConnectionProfileId::new();
    let present_target_id = DatabaseTargetId::new();
    let missing_target_id = DatabaseTargetId::new();
    repository
        .insert_connection(ConnectionProfileSpec::tcp(
            connection_id.clone(),
            "primary-pg",
            "postgresql",
            "db.internal",
            5432,
            100,
        ))
        .expect("insert connection");
    for (id, engine_object_id, name, fingerprint) in [
        (
            present_target_id.clone(),
            "oid:100",
            "orders",
            "orders-fingerprint",
        ),
        (
            missing_target_id.clone(),
            "oid:200",
            "billing",
            "billing-fingerprint",
        ),
    ] {
        repository
            .insert_target(DatabaseTargetSpec::new(
                id,
                connection_id.clone(),
                engine_object_id,
                name,
                name,
                fingerprint,
                110,
            ))
            .expect("insert selected target");
    }

    let first_run_id = Uuid::new_v4().to_string();
    let first = repository
        .record_discovery_snapshot(DiscoverySnapshotSpec::successful(
            first_run_id.clone(),
            connection_id.clone(),
            vec![
                DiscoveredDatabaseSpec::eligible(
                    "oid:100",
                    "orders",
                    "orders",
                    "orders-fingerprint",
                ),
                DiscoveredDatabaseSpec::eligible(
                    "oid:300",
                    "analytics",
                    "analytics",
                    "analytics-fingerprint",
                ),
            ],
            1_000,
            1_010,
        ))
        .expect("record first discovery");
    assert_eq!(first.added_count(), 1);
    assert_eq!(first.missing_count(), 1);
    assert_eq!(first.changed_count(), 0);
    assert_eq!(
        repository
            .target_count(&connection_id)
            .expect("target count"),
        2
    );

    let present = repository
        .target_discovery_status(&present_target_id)
        .expect("present target status");
    assert_eq!(present.state(), "active");
    assert_eq!(present.missing_since_ms(), None);
    assert_eq!(
        present.last_seen_discovery_run_id(),
        Some(first_run_id.as_str())
    );
    let missing = repository
        .target_discovery_status(&missing_target_id)
        .expect("missing target status");
    assert_eq!(missing.state(), "attention");
    assert_eq!(missing.missing_since_ms(), Some(1_010));
    assert_eq!(missing.source_fingerprint(), "billing-fingerprint");

    repository
        .record_discovery_snapshot(DiscoverySnapshotSpec::successful(
            Uuid::new_v4().to_string(),
            connection_id.clone(),
            vec![DiscoveredDatabaseSpec::eligible(
                "oid:100",
                "orders",
                "orders",
                "orders-fingerprint",
            )],
            2_000,
            2_010,
        ))
        .expect("record repeated missing discovery");
    let still_missing = repository
        .target_discovery_status(&missing_target_id)
        .expect("repeated missing target");
    assert_eq!(still_missing.state(), "attention");
    assert_eq!(
        still_missing.missing_since_ms(),
        Some(1_010),
        "first missing time must remain stable"
    );
    assert_eq!(still_missing.source_fingerprint(), "billing-fingerprint");

    let changed = repository
        .record_discovery_snapshot(DiscoverySnapshotSpec::successful(
            Uuid::new_v4().to_string(),
            connection_id,
            vec![DiscoveredDatabaseSpec::eligible(
                "oid:100",
                "orders",
                "orders",
                "changed-orders-fingerprint",
            )],
            3_000,
            3_010,
        ))
        .expect("record changed discovery");
    assert_eq!(changed.changed_count(), 1);
    let drifted = repository
        .target_discovery_status(&present_target_id)
        .expect("drifted target");
    assert_eq!(drifted.state(), "attention");
    assert_eq!(
        drifted.source_fingerprint(),
        "orders-fingerprint",
        "observation drift must not rewrite the selected target identity"
    );
}
