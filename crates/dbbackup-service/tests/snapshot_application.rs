use std::sync::Arc;

use dbbackup_application::{ApplicationCommand, ApplicationRequest, ApplicationUseCases};
use dbbackup_catalog::{
    rusqlite::{params, Connection},
    CertificationCapability, ConnectionProbeSpec, ConnectionProfileSpec, DatabaseTargetSpec,
    DiscoveredDatabaseSpec, DiscoverySnapshotSpec, EngineCertificationSpec, NativeToolSpec,
    SpoolVolumeSpec,
};
use dbbackup_core::ProviderCapability;
use dbbackup_domain::{
    ArtifactPartId, ArtifactReplicaId, ConnectionProfileId, DatabaseTargetId, RecoveryPointId,
    VerificationRunId,
};
use dbbackup_service::application::ServiceApplication;
use dbbackup_service::host::{HostConfig, HostRuntime, ServiceHost, ShutdownReason};
use serde_json::json;
use tempfile::TempDir;

const PG_SERVER_IDENTITY: &str = "postgresql-system-id:7541424833265539027";
const DRIFTED_PG_SERVER_IDENTITY: &str = "postgresql-system-id:7541424833265539028";

#[test]
fn snapshot_list_is_summary_only_and_snapshot_get_pages_selected_recovery_evidence() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("snapshot-application-test-{}", std::process::id()),
            database_path.clone(),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start host");
    let store = host.operational_store().expect("operational store");
    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    let source_identity = "postgresql:db.internal:5432:orders";
    store
        .configuration()
        .insert_connection(ConnectionProfileSpec::tcp(
            connection_id.clone(),
            "Primary PostgreSQL",
            "postgresql",
            "db.internal",
            5432,
            100,
        ))
        .expect("insert connection");
    store
        .configuration()
        .insert_target(DatabaseTargetSpec::new(
            target_id,
            connection_id.clone(),
            "oid:42",
            "orders",
            "orders",
            source_identity,
            110,
        ))
        .expect("insert target");

    let complete_id = RecoveryPointId::new();
    let incomplete_id = RecoveryPointId::new();
    seed_complete_snapshot(&database_path, &complete_id, source_identity);
    seed_incomplete_snapshot(&database_path, &incomplete_id);
    certify_postgresql(&store);
    seed_restore_target_evidence(&store, &database_path, &connection_id);

    let application = ServiceApplication::with_clock(host.health_probe(), Some(store), fixed_clock);
    let complete = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotList,
            payload: json!({"cursor": null, "limit": 25}),
        })
        .expect("default complete snapshot list");
    assert_eq!(complete.body["totalCount"], 1);
    assert_eq!(complete.body["items"][0]["id"], complete_id.as_str());
    assert_eq!(
        complete.body["items"][0]["sourceConnectionId"],
        connection_id.as_str()
    );
    assert_eq!(complete.body["items"][0]["replicaDurability"], "degraded");
    assert_eq!(
        complete.body["items"][0]["protectionCompliance"],
        "non_compliant"
    );
    assert_eq!(
        complete.body["items"][0]["restoreEligibility"],
        "conditional"
    );
    assert_eq!(complete.body["items"][0]["signatureStatus"], "valid");
    assert_eq!(complete.body["items"][0]["signingKeyId"], "signing-key-01");
    assert_eq!(
        complete.body["items"][0]["setManifestChecksumBlake3"],
        "a".repeat(64)
    );
    let summary = complete.body["items"][0]
        .as_object()
        .expect("snapshot summary object");
    for unbounded_child in ["parts", "replicas", "verificationEvidence", "restoreDrills"] {
        assert!(
            !summary.contains_key(unbounded_child),
            "snapshot.list must not nest {unbounded_child}"
        );
    }
    assert_eq!(summary["partCount"], 1);
    assert_eq!(summary["replicaCount"], 2);
    assert_eq!(summary["verificationEvidenceCount"], 1);
    assert_eq!(summary["restoreDrillCount"], 0);
    assert!(
        serde_json::to_vec(&complete.body)
            .expect("serialize summary page")
            .len()
            < 1024 * 1024
    );

    let replicas = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotGet,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "section": "replicas",
                "cursor": null,
                "limit": 1
            }),
        })
        .expect("bounded replica detail page");
    assert_eq!(replicas.body["snapshot"]["id"], complete_id.as_str());
    assert_eq!(replicas.body["section"], "replicas");
    assert_eq!(replicas.body["totalCount"], 2);
    assert_eq!(replicas.body["complete"], false);
    let replica_cursor = replicas.body["nextCursor"]
        .as_str()
        .expect("next replica cursor");
    let remaining_replicas = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotGet,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "section": "replicas",
                "cursor": replica_cursor,
                "limit": 1
            }),
        })
        .expect("remaining replica detail page");
    assert_eq!(remaining_replicas.body["totalCount"], 2);
    assert_eq!(remaining_replicas.body["complete"], true);
    assert!(remaining_replicas.body["nextCursor"].is_null());
    let reader_statuses = [
        replicas.body["items"][0]["readerStatus"].as_str(),
        remaining_replicas.body["items"][0]["readerStatus"].as_str(),
    ];
    assert!(reader_statuses.contains(&Some("eligible")));
    assert!(
        serde_json::to_vec(&replicas.body)
            .expect("serialize replica page")
            .len()
            < 1024 * 1024
    );

    let parts = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotGet,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "section": "parts",
                "cursor": null,
                "limit": 1
            }),
        })
        .expect("bounded part detail page");
    assert_eq!(parts.body["totalCount"], 1);
    assert_eq!(parts.body["complete"], true);
    assert_eq!(parts.body["items"][0]["role"], "database_dump");

    let verification = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotGet,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "section": "verification_evidence",
                "cursor": null,
                "limit": 1
            }),
        })
        .expect("bounded verification detail page");
    assert_eq!(verification.body["totalCount"], 1);
    assert_eq!(verification.body["complete"], true);
    assert_eq!(verification.body["items"][0]["level"], 2);
    assert_eq!(verification.body["items"][0]["result"], "passed");
    assert!(
        verification.body["items"][0].get("evidence").is_none(),
        "arbitrary evidence JSON must not cross the bounded list contract"
    );
    assert!(
        serde_json::to_vec(&verification.body)
            .expect("serialize evidence page")
            .len()
            < 1024 * 1024
    );

    let restore_drills = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotGet,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "section": "restore_drills",
                "cursor": null,
                "limit": 100
            }),
        })
        .expect("bounded restore drill detail page");
    assert_eq!(restore_drills.body["totalCount"], 0);
    assert_eq!(restore_drills.body["complete"], true);
    assert!(restore_drills.body["nextCursor"].is_null());

    let filtered = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotList,
            payload: json!({
                "cursor": null,
                "limit": 25,
                "scope": "complete",
                "search": "orders",
                "connectionId": connection_id.as_str(),
                "engine": "postgresql",
                "fromMs": 900,
                "toMs": 1_100,
                "artifactCompleteness": "complete",
                "replicaDurability": "degraded",
                "protectionCompliance": "non_compliant",
                "minVerificationLevel": 2,
                "restoreEligibility": "conditional",
                "signatureStatus": "valid"
            }),
        })
        .expect("filtered complete snapshot list");
    assert_eq!(filtered.body["totalCount"], 1);

    let diagnostics = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotList,
            payload: json!({
                "cursor": null,
                "limit": 25,
                "scope": "diagnostics"
            }),
        })
        .expect("diagnostic snapshot list");
    assert_eq!(diagnostics.body["totalCount"], 1);
    assert_eq!(diagnostics.body["items"][0]["id"], incomplete_id.as_str());
    assert_eq!(
        diagnostics.body["items"][0]["artifactCompleteness"],
        "incomplete"
    );
    assert_eq!(
        diagnostics.body["items"][0]["restoreEligibility"],
        "blocked"
    );
    assert_eq!(
        diagnostics.body["items"][0]["diagnosticReason"],
        "manifest_unknown_key"
    );

    let rejected = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotList,
            payload: json!({"cursor": null, "limit": 25, "scope": "everything"}),
        })
        .expect_err("unknown scope rejected");
    assert_eq!(rejected.code(), "APPLICATION_PAYLOAD_INVALID");

    let unbounded_detail = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SnapshotGet,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "section": "replicas",
                "cursor": null,
                "limit": 101
            }),
        })
        .expect_err("unbounded snapshot detail page rejected");
    assert_eq!(unbounded_detail.code(), "APPLICATION_PAYLOAD_INVALID");

    let queued = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::VerificationEnqueue,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "level": 2,
                "purpose": "manual"
            }),
        })
        .expect("enqueue certified L2 verification");
    assert_eq!(queued.body["state"], "queued");
    assert_eq!(queued.body["requestedLevel"], 2);
    assert!(queued.body["workItemId"].as_str().is_some());
    let verification_run_id = queued.body["verificationRunId"]
        .as_str()
        .expect("verification run ID");
    let direct = Connection::open(&database_path).expect("open verification assertion");
    let stored: (i64, String, String) = direct
        .query_row(
            "SELECT requested_level, trigger_kind, state
             FROM verification_runs
             WHERE id = ?1",
            [verification_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("queued verification row");
    assert_eq!(stored, (2, "manual".to_owned(), "queued".to_owned()));

    direct
        .execute(
            "UPDATE catalog_recovery_points SET engine = 'mysql' WHERE id = ?1",
            [complete_id.as_str()],
        )
        .expect("represent legacy mysql snapshot");
    direct
        .execute(
            "UPDATE catalog_snapshot_projection SET engine = 'mysql' WHERE recovery_point_id = ?1",
            [complete_id.as_str()],
        )
        .expect("align mysql snapshot projection");
    let unsupported_mysql_l2 = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::VerificationEnqueue,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "level": 2,
                "purpose": "manual"
            }),
        })
        .expect("record unsupported mysql L2");
    assert_eq!(unsupported_mysql_l2.body["state"], "unsupported");
    assert_eq!(
        unsupported_mysql_l2.body["errorCode"],
        "VERIFY_L2_NATIVE_UNSUPPORTED"
    );
    assert!(unsupported_mysql_l2.body["workItemId"].is_null());
    direct
        .execute(
            "UPDATE catalog_recovery_points SET engine = 'postgresql' WHERE id = ?1",
            [complete_id.as_str()],
        )
        .expect("restore postgresql snapshot identity");
    direct
        .execute(
            "UPDATE catalog_snapshot_projection SET engine = 'postgresql' WHERE recovery_point_id = ?1",
            [complete_id.as_str()],
        )
        .expect("restore postgresql snapshot projection");

    let unsupported_l3 = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::VerificationEnqueue,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "level": 3,
                "purpose": "manual"
            }),
        })
        .expect("record unsupported unbound L3");
    assert_eq!(unsupported_l3.body["state"], "unsupported");
    assert_eq!(unsupported_l3.body["requestedLevel"], 3);
    assert_eq!(
        unsupported_l3.body["errorCode"],
        "VERIFICATION_RESTORE_RUN_REQUIRED"
    );
    assert!(unsupported_l3.body["workItemId"].is_null());
    let lower_level_count: i64 = direct
        .query_row(
            "SELECT COUNT(*) FROM verification_runs
             WHERE requested_level < 3
               AND id = ?1",
            [unsupported_l3.body["verificationRunId"]
                .as_str()
                .expect("unsupported run ID")],
            |row| row.get(0),
        )
        .expect("lower-level downgrade count");
    assert_eq!(lower_level_count, 0);

    let blocked = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::VerificationEnqueue,
            payload: json!({
                "snapshotId": incomplete_id.as_str(),
                "level": 1,
                "purpose": "manual"
            }),
        })
        .expect_err("incomplete source must not enqueue");
    assert_eq!(blocked.code(), "APPLICATION_PAYLOAD_INVALID");

    let preflight = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "mode": "create_new",
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders_restore"
            }),
        })
        .expect("authoritative create-new preflight");
    assert_eq!(preflight.body["eligible"], true);
    assert_eq!(preflight.body["targetExists"], false);
    assert_eq!(preflight.body["mode"], "create_new");
    assert_eq!(preflight.body["targetName"], "orders_restore");
    assert_eq!(preflight.body["serverIdentity"], PG_SERVER_IDENTITY);
    assert_eq!(
        preflight.body["targetFingerprint"]
            .as_str()
            .expect("target fingerprint")
            .len(),
        64
    );
    assert_eq!(
        preflight.body["preflightVersion"]
            .as_str()
            .expect("preflight version")
            .len(),
        64
    );
    assert!(preflight.body["checks"]
        .as_array()
        .expect("preflight checks")
        .iter()
        .all(|check| matches!(check["state"].as_str(), Some("passed" | "warning"))));
    assert!(
        !preflight.body.to_string().contains("db.internal"),
        "source/target endpoint must not cross the UI boundary"
    );

    let unregistered_drill = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "mode": "create_new",
                "purpose": "restore_drill",
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders_drill"
            }),
        })
        .expect("unregistered drill preflight");
    assert_eq!(unregistered_drill.body["eligible"], false);
    assert!(unregistered_drill.body["checks"]
        .as_array()
        .expect("drill checks")
        .iter()
        .any(|check| check["id"] == "restore_target_role" && check["state"] == "failed"));

    direct
        .execute(
            "UPDATE connection_profiles
             SET adapter_config_json = '{\"restoreTestTarget\":true}',
                 updated_at_ms = 2700
             WHERE id = ?1",
            [connection_id.as_str()],
        )
        .expect("register isolated restore-test target");
    let drill_preflight = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "mode": "create_new",
                "purpose": "restore_drill",
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders_drill"
            }),
        })
        .expect("registered drill preflight");
    assert_eq!(drill_preflight.body["eligible"], true);
    assert_eq!(drill_preflight.body["targetRole"], "restore_test");
    let drill = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestoreEnqueueSafe,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "sourceReplicaId": drill_preflight.body["source"]["replicaId"],
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders_drill",
                "preflightVersion": drill_preflight.body["preflightVersion"],
                "requiredVerificationLevel": 3,
                "purpose": "restore_drill"
            }),
        })
        .expect("enqueue registered restore drill");
    assert_eq!(drill.body["purpose"], "restore_drill");
    assert!(drill.body["verificationRunId"].as_str().is_some());
    let drill_binding: (String, String, i64) = direct
        .query_row(
            "SELECT restore.purpose, verification.trigger_kind,
                    verification.requested_level
             FROM restore_runs AS restore
             JOIN verification_runs AS verification
               ON verification.restore_run_id = restore.id
             WHERE restore.id = ?1",
            [drill.body["restoreRunId"]
                .as_str()
                .expect("drill restore run ID")],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("drill restore-verification binding");
    assert_eq!(
        drill_binding,
        ("restore_drill".to_owned(), "restore_drill".to_owned(), 3)
    );

    let collision = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "mode": "create_new",
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders"
            }),
        })
        .expect("existing target preflight");
    assert_eq!(collision.body["eligible"], false);
    assert_eq!(collision.body["targetExists"], true);
    assert!(collision.body["checks"]
        .as_array()
        .expect("collision checks")
        .iter()
        .any(|check| check["id"] == "target_existence" && check["state"] == "failed"));

    let overwrite = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "mode": "overwrite",
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders"
            }),
        })
        .expect("overwrite preflight");
    assert_eq!(overwrite.body["eligible"], false);
    assert_eq!(overwrite.body["targetExists"], true);
    assert!(overwrite.body["checks"]
        .as_array()
        .expect("overwrite checks")
        .iter()
        .any(|check| {
            check["id"] == "destructive_target_identity" && check["state"] == "failed"
        }));
    assert_ne!(
        overwrite.body["targetFingerprint"],
        preflight.body["targetFingerprint"]
    );

    let process_count: i64 = direct
        .query_row("SELECT COUNT(*) FROM process_instances", [], |row| {
            row.get(0)
        })
        .expect("process count");
    assert_eq!(
        process_count, 0,
        "preflight rejection must not start a native process"
    );

    store_new_probe(
        &database_path,
        &connection_id,
        "17.4",
        DRIFTED_PG_SERVER_IDENTITY,
        2_900,
    );
    let drifted = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "mode": "create_new",
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders_restore"
            }),
        })
        .expect("preflight after server evidence drift");
    assert_ne!(
        drifted.body["targetFingerprint"],
        preflight.body["targetFingerprint"]
    );
    assert_ne!(
        drifted.body["preflightVersion"],
        preflight.body["preflightVersion"]
    );

    let stale_restore = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestoreEnqueueSafe,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "sourceReplicaId": drifted.body["source"]["replicaId"],
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders_restore",
                "preflightVersion": preflight.body["preflightVersion"],
                "requiredVerificationLevel": 2
            }),
        })
        .expect_err("stale preflight must be rejected");
    assert_eq!(stale_restore.code(), "APPLICATION_PAYLOAD_INVALID");

    let restore = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::RestoreEnqueueSafe,
            payload: json!({
                "snapshotId": complete_id.as_str(),
                "sourceReplicaId": drifted.body["source"]["replicaId"],
                "targetConnectionId": connection_id.as_str(),
                "targetName": "orders_restore",
                "preflightVersion": drifted.body["preflightVersion"],
                "requiredVerificationLevel": 2
            }),
        })
        .expect("enqueue create-new restore");
    assert_eq!(restore.body["state"], "queued");
    assert_eq!(restore.body["mode"], "create_new");
    let restore_run_id = restore.body["restoreRunId"]
        .as_str()
        .expect("restore run ID");
    let restore_row: (String, String, String, String) = direct
        .query_row(
            "SELECT conflict_policy, state, target_database_name, source_replica_id
             FROM restore_runs
             WHERE id = ?1",
            [restore_run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("restore run row");
    assert_eq!(
        restore_row,
        (
            "create_new".to_owned(),
            "queued".to_owned(),
            "orders_restore".to_owned(),
            drifted.body["source"]["replicaId"]
                .as_str()
                .expect("source replica")
                .to_owned(),
        )
    );
    let restore_work_count: i64 = direct
        .query_row(
            "SELECT COUNT(*)
             FROM work_items
             WHERE id = ?1 AND kind = 'restore' AND restore_run_id = ?2",
            params![
                restore.body["workItemId"]
                    .as_str()
                    .expect("restore work ID"),
                restore_run_id
            ],
            |row| row.get(0),
        )
        .expect("restore work row");
    assert_eq!(restore_work_count, 1);

    host.shutdown(ShutdownReason::Stop).expect("stop host");
}

fn fixed_clock() -> i64 {
    3_000
}

fn certify_postgresql(store: &dbbackup_catalog::OperationalStore) {
    let mut certification = EngineCertificationSpec::candidate(
        "cert-postgresql-17-snapshot-test",
        "postgresql",
        ">=17,<18",
        "pg_restore",
        ">=17,<18",
        "windows",
        "x86_64",
        "custom",
        "application-consistent",
        "evidence/snapshot-test.json",
        "e".repeat(64),
        2_000,
    );
    for capability in ProviderCapability::REQUIRED {
        certification = certification.with_capability(CertificationCapability::passed(
            capability.as_str(),
            format!("evidence/{}.json", capability.as_str()),
        ));
    }
    store
        .certification()
        .insert_candidate(certification)
        .expect("insert certification candidate");
    store
        .certification()
        .certify("cert-postgresql-17-snapshot-test", 2_100, 10_000)
        .expect("certify PostgreSQL");
}

fn seed_restore_target_evidence(
    store: &dbbackup_catalog::OperationalStore,
    database_path: &std::path::Path,
    connection_id: &ConnectionProfileId,
) {
    let tool = store
        .certification()
        .record_tool(NativeToolSpec::observed(
            "tool-pg-restore-17",
            "pg_restore",
            r"C:\dbbackup\tools\pg_restore.exe",
            "17.4",
            "x86_64",
            "f".repeat(64),
            "bundled",
            2_000,
        ))
        .expect("record restore tool");
    store
        .certification()
        .trust_verified_tool(tool.id(), "PostgreSQL Global Development Group", 2_100)
        .expect("trust restore tool");
    store
        .configuration()
        .record_connection_probe(ConnectionProbeSpec::passed(
            uuid::Uuid::new_v4().to_string(),
            connection_id.clone(),
            "17.4",
            json!({
                "restorePreflight": true,
                "serverIdentity": PG_SERVER_IDENTITY
            })
            .to_string(),
            2_700,
            2_750,
        ))
        .expect("record target probe");
    store
        .configuration()
        .record_discovery_snapshot(DiscoverySnapshotSpec::successful(
            uuid::Uuid::new_v4().to_string(),
            connection_id.clone(),
            vec![DiscoveredDatabaseSpec::eligible(
                "database:orders",
                "orders",
                "orders",
                "postgresql:db.internal:5432:orders",
            )],
            2_760,
            2_780,
        ))
        .expect("record target discovery");
    store
        .runtime()
        .register_spool_volume(SpoolVolumeSpec::ready(
            uuid::Uuid::new_v4().to_string(),
            "restore-spool",
            r"D:\DbBackup\spool",
            10_000_000,
            8_000_000,
            1_000_000,
            9_000_000,
            2_800,
        ))
        .expect("register restore spool");

    let direct = Connection::open(database_path).expect("open credential fixture");
    let secret_ref_id = uuid::Uuid::new_v4().to_string();
    direct
        .execute(
            "INSERT INTO secret_refs(
                 id, provider, provider_locator, purpose, version, state,
                 fingerprint, created_at_ms
             ) VALUES (
                 ?1, 'external', ?2, 'database_auth', 1, 'active',
                 ?3, 2500
             )",
            params![
                secret_ref_id,
                format!("fixture:{secret_ref_id}"),
                "1".repeat(64)
            ],
        )
        .expect("insert database auth reference");
    direct
        .execute(
            "INSERT INTO connection_secret_bindings(
                 connection_profile_id, secret_role, secret_ref_id, created_at_ms
             ) VALUES (?1, 'database_auth', ?2, 2500)",
            params![connection_id.as_str(), secret_ref_id],
        )
        .expect("bind database auth reference");
}

fn store_new_probe(
    database_path: &std::path::Path,
    connection_id: &ConnectionProfileId,
    server_version: &str,
    server_identity: &str,
    observed_at_ms: i64,
) {
    let direct = Connection::open(database_path).expect("open drift fixture");
    direct
        .execute(
            "INSERT INTO connection_probe_runs(
                 id, connection_profile_id, trigger_kind, status,
                 server_version, observed_capabilities_json,
                 started_at_ms, completed_at_ms, created_at_ms
             ) VALUES (
                 ?1, ?2, 'preflight', 'passed',
                 ?3, ?4,
                 ?5, ?6, ?5
             )",
            params![
                uuid::Uuid::new_v4().to_string(),
                connection_id.as_str(),
                server_version,
                json!({ "serverIdentity": server_identity }).to_string(),
                observed_at_ms,
                observed_at_ms + 1
            ],
        )
        .expect("insert drifted probe");
    direct
        .execute(
            "UPDATE connection_profiles
             SET state = 'ready', last_probe_status = 'passed',
                 last_probe_at_ms = ?2, updated_at_ms = ?2
             WHERE id = ?1",
            params![connection_id.as_str(), observed_at_ms + 1],
        )
        .expect("update probe projection");
}

fn seed_complete_snapshot(
    database_path: &std::path::Path,
    recovery_point_id: &RecoveryPointId,
    source_identity: &str,
) {
    let connection = Connection::open(database_path).expect("open catalog fixture");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, origin_occurrence_id, origin_backup_run_id,
                 source_target_identity, source_display_name, engine, state,
                 consistency_grade, committed_at_ms, manifest_schema_version,
                 set_manifest_checksum_blake3, signing_key_id, signature_status,
                 required_part_count, present_part_count, plaintext_bytes, cipher_bytes,
                 engine_metadata_json, first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (
                 ?1, 'instance-a', 'occurrence-a', 'backup-a',
                 ?2, 'orders', 'postgresql', 'complete',
                 'application_consistent', 1000, 2,
                 ?3, 'signing-key-01', 'valid',
                 1, 1, 1024, 768,
                 '{\"serverVersion\":\"17.4\"}', 1010, 1020
             )",
            params![recovery_point_id.as_str(), source_identity, "a".repeat(64)],
        )
        .expect("insert complete recovery point");
    let part_id = ArtifactPartId::new();
    connection
        .execute(
            "INSERT INTO catalog_artifact_parts(
                 id, recovery_point_id, part_role, logical_name, required,
                 dump_format, compression_format, encryption_format,
                 plaintext_checksum_blake3, plaintext_bytes, cipher_bytes,
                 metadata_json, created_at_ms
             ) VALUES (
                 ?1, ?2, 'database_dump', 'orders.dump', 1,
                 'custom', 'zstd', 'chacha20poly1305-stream',
                 ?3, 1024, 768, '{}', 1000
             )",
            params![part_id.as_str(), recovery_point_id.as_str(), "b".repeat(64)],
        )
        .expect("insert artifact part");
    let replica_id = ArtifactReplicaId::new();
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, origin_destination_id,
                 destination_fingerprint, destination_kind, requirement, state,
                 immutability_capability, retention_until_ms, reader_status,
                 committed_at_ms, last_observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, NULL,
                 'local:vault-a', 'local', 'required', 'committed',
                 'none', 999999, 'eligible',
                 1000, 1020, '{}'
             )",
            params![replica_id.as_str(), recovery_point_id.as_str()],
        )
        .expect("insert artifact replica");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, NULL, 'set_manifest', 'fixture/set-manifest.json',
                 ?4, 512, 'present', 1020, '{}'
             )",
            params![
                uuid::Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id.as_str(),
                "d".repeat(64)
            ],
        )
        .expect("insert stored set manifest");
    connection
        .execute(
            "INSERT INTO catalog_stored_objects(
                 id, recovery_point_id, artifact_replica_id, artifact_part_id,
                 object_role, object_key, checksum_blake3, size_bytes,
                 state, observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, ?3, ?4, 'payload', 'fixture/orders.dump.enc',
                 ?5, 768, 'present', 1020, '{}'
             )",
            params![
                uuid::Uuid::new_v4().to_string(),
                recovery_point_id.as_str(),
                replica_id.as_str(),
                part_id.as_str(),
                "e".repeat(64)
            ],
        )
        .expect("insert stored payload");
    let unavailable_replica_id = ArtifactReplicaId::new();
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, origin_destination_id,
                 destination_fingerprint, destination_kind, requirement, state,
                 immutability_capability, retention_until_ms, reader_status,
                 committed_at_ms, last_observed_at_ms, metadata_json
             ) VALUES (
                 ?1, ?2, NULL,
                 's3:vault-b', 's3', 'required', 'degraded',
                 'governance', 999999, 'unavailable',
                 NULL, 1020, '{}'
             )",
            params![unavailable_replica_id.as_str(), recovery_point_id.as_str()],
        )
        .expect("insert unavailable artifact replica");
    connection
        .execute(
            "INSERT INTO catalog_snapshot_projection(
                 recovery_point_id, source_target_identity, source_display_name,
                 engine, committed_at_ms, artifact_completeness, replica_durability,
                 protection_compliance, restore_eligibility, highest_verified_level,
                 last_verified_at_ms, search_text, projection_version, projected_at_ms
             ) VALUES (
                 ?1, ?2, 'orders', 'postgresql', 1000, 'complete', 'degraded',
                 'non_compliant', 'conditional', 2,
                 1015, 'orders primary postgresql', 1, 1020
             )",
            params![recovery_point_id.as_str(), source_identity],
        )
        .expect("insert snapshot projection");
    connection
        .execute(
            "INSERT INTO catalog_verification_projection(
                 id, recovery_point_id, origin_verification_run_id,
                 verify_level, result, verified_at_ms, tool_name, tool_version,
                 evidence_checksum_blake3, evidence_locator, evidence_json, ingested_at_ms
             ) VALUES (
                 ?1, ?2, NULL,
                 2, 'passed', 1015, 'pg_restore', '17.4',
                 ?3, NULL, '{\"tocParsed\":true}', 1020
             )",
            params![
                VerificationRunId::new().as_str(),
                recovery_point_id.as_str(),
                "c".repeat(64)
            ],
        )
        .expect("insert verification evidence");
}

fn seed_incomplete_snapshot(database_path: &std::path::Path, recovery_point_id: &RecoveryPointId) {
    let connection = Connection::open(database_path).expect("open diagnostic fixture");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, source_target_identity, source_display_name,
                 engine, state, consistency_grade, committed_at_ms,
                 manifest_schema_version, set_manifest_checksum_blake3, signing_key_id,
                 signature_status, required_part_count, present_part_count,
                 engine_metadata_json, first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (
                 ?1, 'instance-b', 'target:unknown', 'unknown source',
                 'postgresql', 'incomplete', 'unknown', NULL,
                 2, ?2, 'missing-key', 'unknown_key', 1, 0,
                 '{}', 2000, 2010
             )",
            params![recovery_point_id.as_str(), "d".repeat(64)],
        )
        .expect("insert incomplete recovery point");
}

struct NoopRuntime;

impl HostRuntime for NoopRuntime {
    fn started(&self, _diagnostics_only: bool) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stop_accepting_ipc(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stop_claiming_work(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn drain_clients(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn checkpointed(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stopped(&self, _reason: ShutdownReason) {}
}
