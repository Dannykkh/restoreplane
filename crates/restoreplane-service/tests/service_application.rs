use std::fs;
use std::str::FromStr;
use std::sync::Arc;

use restoreplane_application::{ApplicationCommand, ApplicationRequest, ApplicationUseCases};
use restoreplane_catalog::rusqlite::{params, Connection};
use restoreplane_catalog::{
    ActivityEventSpec, CertificationCapability, ConnectionProbeSpec, ConnectionProfileSpec,
    DatabaseTargetSpec, DestinationProbeSpec, DestinationSecretRoleV2, DestinationSpec,
    DiscoveredDatabaseSpec, DiscoverySnapshotSpec, EngineCertificationSpec,
    OccurrenceMaterializationSpec, OperationalStore, PlanDraftSpec,
};
use restoreplane_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, DestinationRequirement,
    OccurrenceOrigin, PlanRevisionId, WorkspaceId,
};
use restoreplane_service::application::ServiceApplication;
use restoreplane_service::host::{HostConfig, HostRuntime, ServiceHost, ShutdownReason};
use restoreplane_service::ipc::{current_process_identity, WindowsSid};
use restoreplane_service::secrets::{AclPolicy, ServiceSecretStore};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn workspace_commands_create_scope_and_connection_requests_stay_inside_it() {
    let temp = TempDir::new().expect("temp directory");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("workspace-application-test-{}", std::process::id()),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start host");
    let application = ServiceApplication::new(host.health_probe(), host.operational_store());

    let initial = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::WorkspaceList,
            payload: json!({}),
        })
        .expect("list workspaces");
    assert_eq!(
        initial.body["items"]
            .as_array()
            .expect("workspace page")
            .len(),
        1
    );
    assert_eq!(initial.body["totalCount"], 1);
    assert_eq!(initial.body["complete"], true);
    assert_eq!(initial.body["nextCursor"], serde_json::Value::Null);
    let default_workspace_id = initial.body["items"][0]["id"]
        .as_str()
        .expect("default workspace id");

    let created = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::WorkspaceCreate,
            payload: json!({ "displayName": "Analytics" }),
        })
        .expect("create workspace");
    let analytics_workspace_id = created.body["id"].as_str().expect("analytics workspace id");
    assert_eq!(created.body["displayName"], "Analytics");

    application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionSave,
            payload: json!({
                "workspaceId": analytics_workspace_id,
                "displayName": "Analytics SQLite",
                "engine": "sqlite",
                "transport": "file",
                "host": "",
                "port": 1,
                "filePath": "D:/data/analytics.db",
                "tlsMode": "disabled",
                "username": "",
                "password": ""
            }),
        })
        .expect("save scoped connection");

    let analytics_connections = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionList,
            payload: json!({
                "workspaceId": analytics_workspace_id,
                "cursor": null,
                "limit": 100
            }),
        })
        .expect("analytics connections");
    let default_connections = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionList,
            payload: json!({
                "workspaceId": default_workspace_id,
                "cursor": null,
                "limit": 100
            }),
        })
        .expect("default connections");
    assert_eq!(analytics_connections.body["totalCount"], 1);
    assert_eq!(default_connections.body["totalCount"], 0);

    host.shutdown(ShutdownReason::Stop).expect("stop host");
}

#[test]
fn health_and_schema_commands_read_the_live_host_without_exposing_store_errors() {
    let temp = TempDir::new().expect("temp directory");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("application-test-{}", std::process::id()),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start host");
    let application = ServiceApplication::new(host.health_probe(), host.operational_store())
        .with_maintenance_root(temp.path().to_path_buf());

    let health = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ServiceHealth,
            payload: json!({}),
        })
        .expect("service health");
    assert_eq!(health.body["phase"], "rehearsal_ready");
    assert_eq!(health.body["schemaVersion"], 4);
    assert_eq!(health.body["applicationTableCount"], 67);
    assert_eq!(health.body["executionAuthority"], "legacy_direct");
    assert_eq!(health.body["cutoverMode"], "rehearsal_only");
    assert!(
        health.body.get("cutoverMarker").is_some(),
        "health contract must expose a nullable operator recovery marker"
    );
    assert_eq!(health.body["cutoverMarker"], serde_json::Value::Null);
    assert!(
        health.body.get("recoveryDisposition").is_some(),
        "health contract must expose a nullable rollback/forward-fix boundary"
    );
    assert_eq!(health.body["recoveryDisposition"], serde_json::Value::Null);
    assert_eq!(
        health.body["currentBinaryVersion"],
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(health.body["minimumReaderVersion"], serde_json::Value::Null);
    assert_eq!(health.body["claimsEnabled"], false);
    assert_eq!(health.body["acceptsIpc"], true);
    assert_eq!(health.body["scheduler"]["state"], "not_configured");
    assert_eq!(health.body["scheduler"]["configuredPlanCount"], 0);
    assert_eq!(
        health.body["scheduler"]["lastTickAtMs"],
        serde_json::Value::Null
    );
    assert_eq!(health.body["retention"]["state"], "not_configured");
    assert_eq!(health.body["retention"]["configuredPolicyCount"], 0);
    assert_eq!(
        health.body["retention"]["lastScanAtMs"],
        serde_json::Value::Null
    );
    assert!(health.body.get("debug").is_none());

    let schema = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SchemaStatus,
            payload: json!({}),
        })
        .expect("schema status");
    assert_eq!(schema.body["userVersion"], 4);
    assert_eq!(schema.body["applicationTableCount"], 67);
    assert_eq!(schema.body["journalMode"], "wal");
    assert_eq!(schema.body["synchronous"], "full");
    assert_eq!(schema.body["foreignKeys"], true);

    let diagnostics = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ServiceDiagnostics,
            payload: json!({}),
        })
        .expect("service diagnostics");
    assert_eq!(diagnostics.body["schemaVersion"], 1);
    assert_eq!(diagnostics.body["schema"]["userVersion"], 4);
    assert_eq!(diagnostics.body["queue"]["total"], 0);
    assert_eq!(diagnostics.body["leases"]["stale"], 0);
    assert_eq!(diagnostics.body["notifications"]["pending"], 0);

    let control_backup = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ControlStoreBackup,
            payload: json!({}),
        })
        .expect("online control backup");
    assert_eq!(control_backup.body["integrityResult"], "passed");
    assert_eq!(control_backup.body["rehearsalResult"], "passed");
    assert!(std::path::Path::new(
        control_backup.body["backupPath"]
            .as_str()
            .expect("control backup path")
    )
    .exists());

    let drain = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ServiceDrain,
            payload: json!({}),
        })
        .expect("begin service drain");
    assert_eq!(drain.body["state"], "draining");
    assert_eq!(
        host.health().phase(),
        restoreplane_service::host::HostPhase::Draining
    );
    let blocked_mutation = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionSave,
            payload: json!({}),
        })
        .expect_err("draining service blocks new mutations");
    assert_eq!(blocked_mutation.code(), "APPLICATION_REHEARSAL_ONLY");

    host.shutdown(ShutdownReason::Stop).expect("stop host");
    let stopped = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ServiceHealth,
            payload: json!({}),
        })
        .expect("stopped health remains queryable");
    assert_eq!(stopped.body["phase"], "stopped");
    assert_eq!(stopped.body["acceptsIpc"], false);
}

#[test]
fn update_pending_boot_is_fail_closed_until_the_new_binary_commits_and_restarts() {
    let temp = TempDir::new().expect("temp directory");
    let updates = temp.path().join("updates");
    std::fs::create_dir_all(&updates).expect("updates directory");
    let pending_path = updates.join("update-pending.json");
    let update_id = uuid::Uuid::new_v4().to_string();
    std::fs::write(
        &pending_path,
        serde_json::to_vec(&json!({
            "schemaVersion": 1,
            "updateId": update_id,
            "targetVersion": env!("CARGO_PKG_VERSION"),
            "createdAtMs": 0,
        }))
        .expect("pending JSON"),
    )
    .expect("pending marker");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("update-pending-test-{}", std::process::id()),
            temp.path().join("pending-operational.db"),
        )
        .expect("host config")
        .with_update_pending(true),
        Arc::new(NoopRuntime),
    )
    .expect("update-pending host");
    assert_eq!(
        host.health().phase(),
        restoreplane_service::host::HostPhase::Draining
    );
    assert!(host.health().update_pending());
    assert!(!host.health().claims_enabled());

    let application = ServiceApplication::new(host.health_probe(), host.operational_store())
        .with_maintenance_root(temp.path().to_path_buf());
    let committed = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ServiceUpdateCommit,
            payload: json!({}),
        })
        .expect("commit verified update");
    assert_eq!(committed.body["state"], "committed");
    assert_eq!(committed.body["minimumReaderAdvanced"], false);
    assert_eq!(committed.body["restartRequired"], true);
    assert!(!pending_path.exists());
    host.shutdown(ShutdownReason::Stop).expect("stop host");
}

#[test]
fn backup_scope_list_returns_scope_specific_health_without_target_fallbacks() {
    let temp = TempDir::new().expect("temp directory");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("scope-health-application-test-{}", std::process::id()),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start host");
    let store = host.operational_store().expect("operational store");
    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    store
        .configuration()
        .insert_connection(ConnectionProfileSpec::tcp(
            connection_id.clone(),
            "Scope PostgreSQL",
            "postgresql",
            "db.internal",
            5432,
            100,
        ))
        .expect("connection");
    store
        .configuration()
        .insert_target(DatabaseTargetSpec::new(
            target_id.clone(),
            connection_id,
            "database:orders",
            "orders",
            "orders",
            "database-fingerprint",
            110,
        ))
        .expect("target");

    let application =
        ServiceApplication::with_clock(host.health_probe(), Some(store), scope_health_clock);
    let response = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::BackupScopeList,
            payload: json!({
                "workspaceId": restoreplane_catalog::DEFAULT_WORKSPACE_ID,
                "databaseTargetId": target_id.as_str(),
                "cursor": null,
                "limit": 1,
                "state": "active",
                "search": "orders"
            }),
        })
        .expect("scope health response");
    let scope = response.body["items"]
        .as_array()
        .expect("scope page")
        .first()
        .expect("whole scope");
    assert_eq!(scope["databaseName"], "orders");
    assert_eq!(scope["connectionName"], "Scope PostgreSQL");
    assert_eq!(scope["mode"], "whole_database");
    assert_eq!(response.body["nextCursor"], json!(null));
    assert_eq!(response.body["totalCount"], 1);
    assert_eq!(scope["driftState"], "unknown");
    assert_eq!(scope["eligibleObjectCount"], serde_json::Value::Null);
    assert_eq!(scope["planCount"], 0);
    assert_eq!(scope["retentionPolicyCount"], 0);
    assert_eq!(scope["latestBackupState"], serde_json::Value::Null);
    assert_eq!(scope["restoreEligibility"], "unknown");
    assert_eq!(scope["artifactCompleteness"], "unknown");
    assert_eq!(scope["protectionCompliance"], "not_configured");

    host.shutdown(ShutdownReason::Stop).expect("stop host");
}

fn scope_health_clock() -> i64 {
    10_000
}

fn overdue_scope_health_clock() -> i64 {
    604_820_000
}

#[test]
fn console_read_commands_use_operational_store_pages_and_reject_unbounded_payloads() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("console-query-test-{}", std::process::id()),
            database_path.clone(),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start host");
    let store = host.operational_store().expect("operational store");
    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    let destination_id = DestinationId::new();
    let plan_id = BackupPlanId::new();
    let revision_id = PlanRevisionId::new();
    let configuration = store.configuration();
    configuration
        .insert_connection(ConnectionProfileSpec::tcp(
            connection_id.clone(),
            "Primary PostgreSQL",
            "postgresql",
            "db.internal",
            5432,
            100,
        ))
        .expect("insert connection");
    configuration
        .insert_target(DatabaseTargetSpec::new(
            target_id.clone(),
            connection_id,
            "oid:42",
            "orders",
            "orders",
            "source-fingerprint",
            110,
        ))
        .expect("insert target");
    configuration
        .insert_destination(DestinationSpec::local(
            destination_id.clone(),
            "Local Vault",
            r#"{"path":"D:\\backup","secret":"must-not-leak"}"#,
            120,
        ))
        .expect("insert destination");
    configuration
        .insert_plan_draft(PlanDraftSpec::manual(
            plan_id,
            revision_id.clone(),
            "Orders nightly",
            1,
            "a".repeat(64),
            130,
        ))
        .expect("insert plan");
    configuration
        .add_revision_target(&revision_id, &target_id, 131)
        .expect("add target");
    configuration
        .add_revision_destination(
            &revision_id,
            &destination_id,
            DestinationRequirement::Required,
            132,
        )
        .expect("add destination");
    let connection = Connection::open(&database_path).expect("recovery projection writer");
    connection
        .execute(
            "UPDATE backup_plan_revisions
             SET verification_policy_json = ?2
             WHERE id = ?1",
            params![
                revision_id.as_str(),
                r#"{"requestedLevel":2,"schedule":"weekly"}"#
            ],
        )
        .expect("weekly verification policy");
    configuration
        .publish_revision(&revision_id, 140)
        .expect("publish revision");
    let occurrence = store
        .runtime()
        .materialize_occurrence(OccurrenceMaterializationSpec::due(
            revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            8_000,
            8_000,
        ))
        .expect("backup occurrence");
    let (backup_run_id, source_fingerprint): (String, String) = connection
        .query_row(
            "SELECT run.id, run.source_fingerprint
             FROM backup_runs AS run
             JOIN backup_run_scopes AS scope ON scope.backup_run_id = run.id
             WHERE run.plan_occurrence_id = ?1 AND scope.backup_scope_id = ?2",
            params![occurrence.occurrence_id().as_str(), target_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("whole database backup run");
    let recovery_point_id = "20000000-0000-4000-8000-000000000099";
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, origin_occurrence_id, origin_backup_run_id,
                 source_target_identity, source_display_name, engine, state,
                 consistency_grade, committed_at_ms, manifest_schema_version,
                 set_manifest_checksum_blake3, signing_key_id, signature_status,
                 required_part_count, present_part_count, engine_metadata_json,
                 first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (?1, 'instance-wire', ?2, ?3, ?4, 'orders', 'postgresql',
                       'complete', 'transactional', 9000, 2, ?5, 'key-wire', 'valid',
                       1, 1, '{}', 9100, 9100)",
            params![
                recovery_point_id,
                occurrence.occurrence_id().as_str(),
                backup_run_id,
                source_fingerprint,
                "2".repeat(64)
            ],
        )
        .expect("recovery point");
    connection
        .execute(
            "INSERT INTO catalog_snapshot_projection(
                 recovery_point_id, source_target_identity, source_display_name,
                 engine, committed_at_ms, artifact_completeness, replica_durability,
                 protection_compliance, restore_eligibility, highest_verified_level,
                 last_verified_at_ms, search_text, projection_version, projected_at_ms
             ) VALUES (?1, ?2, 'orders', 'postgresql', 9000, 'complete', 'healthy',
                       'compliant', 'eligible', 2, 9050, 'orders', 1, 9100)",
            params![recovery_point_id, source_fingerprint],
        )
        .expect("snapshot projection");
    connection
        .execute(
            "UPDATE backup_runs
             SET state = 'succeeded', recovery_point_id = ?2, started_at_ms = 8500,
                 completed_at_ms = 9000, updated_at_ms = 9000
             WHERE id = ?1",
            params![backup_run_id, recovery_point_id],
        )
        .expect("successful backup run");
    store
        .activity()
        .append_batch(vec![ActivityEventSpec::info(
            "event-1",
            150,
            "BACKUP_QUEUED",
            "work_item",
            "work-1",
            "backup is waiting for a source permit",
        )])
        .expect("append activity");

    let application = ServiceApplication::with_clock(
        host.health_probe(),
        Some(store),
        overdue_scope_health_clock,
    );
    let connections = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionList,
            payload: json!({"cursor": null, "limit": 25}),
        })
        .expect("connection list");
    assert_eq!(
        connections.body["items"][0]["displayName"],
        "Primary PostgreSQL"
    );
    assert_eq!(connections.body["items"][0]["targetCount"], 1);
    assert_eq!(connections.body["items"][0]["targets"], json!([]));
    let targets = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::TargetList,
            payload: json!({"cursor": null, "limit": 100}),
        })
        .expect("target list");
    assert_eq!(targets.body["items"][0]["displayName"], "orders");
    assert_eq!(
        targets.body["items"][0]["protectionScope"],
        "whole_database"
    );
    assert_eq!(
        targets.body["items"][0]["latestRecoveryPointId"],
        recovery_point_id
    );
    assert_eq!(
        targets.body["items"][0]["protectionCompliance"],
        "compliant"
    );
    assert_eq!(targets.body["items"][0]["verificationOverdue"], true);
    assert_eq!(targets.body["items"][0]["displayName"], "orders");

    let destinations = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::DestinationList,
            payload: json!({"cursor": null, "limit": 25}),
        })
        .expect("destination list");
    let destination_json = destinations.body.to_string();
    assert!(!destination_json.contains("must-not-leak"));
    assert!(!destination_json.contains("\"secret\""));

    let plans = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::PlanList,
            payload: json!({"cursor": null, "limit": 25}),
        })
        .expect("plan list");
    assert_eq!(plans.body["items"][0]["requiredDestinationCount"], 1);

    let activity = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ActivityList,
            payload: json!({"cursor": null, "limit": 50, "severity": null}),
        })
        .expect("activity list");
    assert_eq!(activity.body["items"][0]["eventCode"], "BACKUP_QUEUED");

    let engine_matrix = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::EngineMatrix,
            payload: json!({}),
        })
        .expect("engine matrix");
    assert_eq!(engine_matrix.body["complete"], true);
    assert_eq!(engine_matrix.body["certificationTotalCount"], 0);
    assert_eq!(engine_matrix.body["rows"].as_array().map(Vec::len), Some(9));
    assert_eq!(
        engine_matrix.body["rows"]
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["engine"] == "sqlite"))
            .map(|row| &row["supportStatus"]),
        Some(&json!("unavailable"))
    );

    let invalid = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ActivityList,
            payload: json!({"cursor": null, "limit": 501, "severity": null}),
        })
        .expect_err("unbounded page rejected");
    assert_eq!(invalid.code(), "APPLICATION_PAYLOAD_INVALID");
}

#[test]
fn connection_save_encrypts_credentials_and_settings_read_uses_the_operational_store() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("operational.db");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("console-mutation-test-{}", std::process::id()),
            database_path.clone(),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start host");
    let store = host.operational_store().expect("operational store");
    let owner_sid = current_process_identity()
        .expect("current process identity")
        .user_sid()
        .clone();
    let service_sid = WindowsSid::parse("S-1-5-18").expect("LocalSystem SID");
    assert_ne!(
        owner_sid, service_sid,
        "fixture requires distinct identities"
    );
    let secret_root = temp.path().join("secret-store");
    let secrets = Arc::new(
        ServiceSecretStore::open(
            secret_root.clone(),
            store.secret_refs(),
            AclPolicy::new(service_sid, owner_sid).expect("ACL policy"),
        )
        .expect("secret store"),
    );
    let application = ServiceApplication::new(host.health_probe(), Some(store.clone()))
        .with_secret_store(secrets);
    let canary = "application-secret-canary-36fdaac5";

    let saved = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionSave,
            payload: json!({
                "displayName": "Application PostgreSQL",
                "engine": "postgresql",
                "transport": "tcp",
                "host": "db.internal",
                "port": 5432,
                "filePath": "",
                "tlsMode": "verify_full",
                "username": "backup_operator",
                "password": canary
            }),
        })
        .expect("save connection");
    assert_eq!(saved.body["state"], "draft");
    assert!(saved.body["connectionId"].as_str().is_some());
    assert!(!saved.body.to_string().contains(canary));

    let connections = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionList,
            payload: json!({"cursor": null, "limit": 25}),
        })
        .expect("connection list");
    assert_eq!(
        connections.body["items"][0]["displayName"],
        "Application PostgreSQL"
    );
    assert!(!connections.body.to_string().contains(canary));

    let connection_id = ConnectionProfileId::from_str(
        saved.body["connectionId"]
            .as_str()
            .expect("connection ID response"),
    )
    .expect("typed connection ID");
    let probe = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionProbe,
            payload: json!({"connectionId": connection_id.as_str()}),
        })
        .expect("enqueue connection probe");
    assert_eq!(probe.body["state"], "queued");
    let probe_work_item_id = probe.body["workItemId"]
        .as_str()
        .expect("probe work item ID");
    let queued = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::WorkGet,
            payload: json!({"workItemId": probe_work_item_id}),
        })
        .expect("query queued probe");
    assert_eq!(queued.body["operationKind"], "connection_probe");
    assert_eq!(queued.body["fencingToken"], 0);
    let stale_cancel = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::WorkCancel,
            payload: json!({
                "workItemId": probe_work_item_id,
                "fencingToken": 1
            }),
        })
        .expect_err("stale fencing token must not cancel work");
    assert_eq!(stale_cancel.code(), "WORK_ITEM_FENCE_STALE");
    let still_queued = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::WorkGet,
            payload: json!({"workItemId": probe_work_item_id}),
        })
        .expect("query work after stale cancellation");
    assert_eq!(still_queued.body["state"], "queued");
    let cancelled = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::WorkCancel,
            payload: json!({
                "workItemId": probe_work_item_id,
                "fencingToken": 0
            }),
        })
        .expect("cancel queued probe");
    assert_eq!(cancelled.body["state"], "cancelled");
    store
        .configuration()
        .record_connection_probe(ConnectionProbeSpec::passed(
            uuid::Uuid::new_v4().to_string(),
            connection_id.clone(),
            "17.5",
            r#"{"probe":"passed"}"#,
            90,
            100,
        ))
        .expect("record authoritative passed connection probe");

    let discovery_run_id = uuid::Uuid::new_v4().to_string();
    store
        .configuration()
        .record_discovery_snapshot(DiscoverySnapshotSpec::successful(
            &discovery_run_id,
            connection_id.clone(),
            vec![DiscoveredDatabaseSpec::eligible(
                "database:orders",
                "orders",
                "orders",
                "postgresql:db.internal:5432:orders",
            )],
            100,
            110,
        ))
        .expect("record completed discovery");
    let discovery_page = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::DiscoveryResultList,
            payload: json!({
                "discoveryRunId": discovery_run_id,
                "cursor": null,
                "limit": 100,
                "search": "ORDERS",
                "eligibility": "eligible"
            }),
        })
        .expect("paged discovery results");
    assert_eq!(discovery_page.body["totalCount"], 1);
    assert_eq!(discovery_page.body["items"][0]["databaseName"], "orders");
    assert_eq!(discovery_page.body["nextCursor"], json!(null));
    let selected = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::TargetSelect,
            payload: json!({
                "connectionId": connection_id.as_str(),
                "discoveryRunId": discovery_run_id,
                "engineObjectIds": ["database:orders"]
            }),
        })
        .expect("select discovered target");
    let target_id = selected.body["targets"][0]["id"]
        .as_str()
        .expect("selected target ID")
        .to_owned();
    assert_eq!(selected.body["targets"][0]["displayName"], "orders");
    assert_eq!(
        selected.body["targets"][0]["protectionCompliance"],
        "not_configured"
    );

    let destination = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::DestinationSave,
            payload: json!({
                "displayName": "Application S3 Vault",
                "kind": "s3",
                "endpoint": {
                    "bucket": "application-backups",
                    "prefix": "production",
                    "region": "ap-northeast-2",
                    "forcePathStyle": false,
                    "provider": "aws",
                    "objectLockMode": "governance",
                    "retentionDays": 30,
                    "minDwellDays": 7,
                    "checksumAlgo": "crc32c",
                    "complianceOptIn": false
                },
                "credentials": {
                    "backupWriter": {
                        "schemaVersion": 1,
                        "accessKeyId": "writer",
                        "secretAccessKey": canary
                    },
                    "restoreReader": {
                        "schemaVersion": 1,
                        "accessKeyId": "reader",
                        "secretAccessKey": canary
                    }
                }
            }),
        })
        .expect("save destination");
    let destination_id = DestinationId::from_str(
        destination.body["destinationId"]
            .as_str()
            .expect("destination ID response"),
    )
    .expect("typed destination ID");
    store
        .configuration()
        .record_destination_probes(vec![
            DestinationProbeSpec::passed(
                uuid::Uuid::new_v4().to_string(),
                destination_id.clone(),
                DestinationSecretRoleV2::BackupWriter,
                r#"{"writer":true}"#,
                Some(10_000_000),
                1,
                110,
                120,
            ),
            DestinationProbeSpec::passed(
                uuid::Uuid::new_v4().to_string(),
                destination_id.clone(),
                DestinationSecretRoleV2::RestoreReader,
                r#"{"reader":true}"#,
                Some(10_000_000),
                1,
                110,
                120,
            ),
        ])
        .expect("record authoritative destination role probes");
    seed_postgresql_plan_certification(&store);
    let destinations = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::DestinationList,
            payload: json!({"cursor": null, "limit": 25}),
        })
        .expect("destination list");
    assert!(!destinations.body.to_string().contains(canary));
    assert_eq!(
        destinations.body["items"][0]["roles"][0]["credentialBound"],
        true
    );
    assert_eq!(
        destinations.body["items"][0]["roles"][1]["credentialBound"],
        true
    );
    let foreign_workspace = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::WorkspaceCreate,
            payload: json!({"displayName": "Foreign plan destination"}),
        })
        .expect("create foreign workspace");
    let foreign_workspace_id = WorkspaceId::from_str(
        foreign_workspace.body["id"]
            .as_str()
            .expect("foreign workspace ID"),
    )
    .expect("typed foreign workspace ID");
    let foreign_destination_id = DestinationId::new();
    store
        .configuration()
        .insert_destination_in_workspace(
            &foreign_workspace_id,
            DestinationSpec::local(
                foreign_destination_id.clone(),
                "Foreign local vault",
                r#"{"path":"D:\\foreign"}"#,
                900,
            ),
        )
        .expect("insert foreign destination");
    let cross_workspace_plan = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::PlanSave,
            payload: json!({
                "displayName": "Cross workspace plan",
                "targetIds": [target_id],
                "destinations": [{
                    "destinationId": foreign_destination_id.as_str(),
                    "requirement": "required"
                }],
                "schedule": {
                    "kind": "cron",
                    "expression": "15 * * * *",
                    "timezone": "Asia/Seoul"
                },
                "rpoMs": 7_200_000,
                "retention": {"daily": 7, "weekly": 4, "monthly": 12},
                "verification": {"requestedLevel": 2}
            }),
        })
        .expect_err("foreign destination must not cross workspace plan ownership");
    assert_eq!(cross_workspace_plan.code(), "APPLICATION_PAYLOAD_INVALID");
    let draft = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::PlanSave,
            payload: json!({
                "displayName": "Application orders hourly",
                "targetIds": [target_id],
                "destinations": [{
                    "destinationId": destination_id.as_str(),
                    "requirement": "required"
                }],
                "schedule": {
                    "kind": "cron",
                    "expression": "15 * * * *",
                    "timezone": "Asia/Seoul"
                },
                "rpoMs": 7_200_000,
                "retention": {"daily": 7, "weekly": 4, "monthly": 12},
                "verification": {"requestedLevel": 2}
            }),
        })
        .expect("save plan draft");
    let published = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::PlanPublish,
            payload: json!({
                "planId": draft.body["planId"],
                "revisionId": draft.body["revisionId"]
            }),
        })
        .expect("publish plan");
    assert_eq!(published.body["state"], "enabled");
    let plans = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::PlanList,
            payload: json!({"cursor": null, "limit": 25}),
        })
        .expect("plan list");
    assert_eq!(plans.body["items"][0]["state"], "enabled");
    assert_eq!(plans.body["items"][0]["rpoMs"], 7_200_000);

    let settings = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::SettingsRead,
            payload: json!({}),
        })
        .expect("settings read");
    assert!(settings.body["resourcePools"].is_array());
    assert!(settings.body["spoolVolumes"].is_array());
    assert!(settings.body["tools"].is_array());
    assert!(settings.body["notificationChannels"].is_array());
    assert_eq!(settings.body["complete"], true);
    assert!(settings.body["inventoryCounts"]["resourcePools"].is_number());
    assert!(settings.body["inventoryCounts"]["spoolVolumes"].is_number());
    assert!(settings.body["inventoryCounts"]["tools"].is_number());
    assert!(settings.body["inventoryCounts"]["notificationChannels"].is_number());
    assert_eq!(settings.body["controlBackup"]["lastStatus"], "unknown");

    host.shutdown(ShutdownReason::Stop).expect("stop host");
    for path in persisted_files(&database_path, &secret_root) {
        let bytes = fs::read(&path).expect("read persisted file");
        assert!(
            !contains(&bytes, canary.as_bytes()),
            "plaintext credential leaked into {}",
            path.display()
        );
    }
}

fn seed_postgresql_plan_certification(store: &OperationalStore) {
    let certification = [
        "probe",
        "discovery",
        "tool_preflight",
        "backup",
        "verify_l1",
        "verify_l2",
        "restore_create_new",
        "validation",
        "restore_l3",
    ]
    .into_iter()
    .fold(
        EngineCertificationSpec::candidate(
            "service-application-postgresql-certification",
            "postgresql",
            ">=17,<19",
            "pg_dump",
            ">=17,<19",
            "windows",
            "x86_64",
            "custom",
            "application-consistent",
            "package://provider-certifications.json#/postgresql",
            "a".repeat(64),
            1,
        ),
        |spec, capability| {
            spec.with_capability(CertificationCapability::passed(
                capability,
                format!("package://provider-certifications.json#/postgresql/{capability}"),
            ))
        },
    );
    let repository = store.certification();
    repository
        .insert_candidate(certification)
        .expect("insert PostgreSQL certification candidate");
    repository
        .certify(
            "service-application-postgresql-certification",
            2,
            i64::MAX - 1,
        )
        .expect("certify PostgreSQL plan capabilities");
}

fn persisted_files(
    database_path: &std::path::Path,
    secret_root: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let mut files = ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| std::path::PathBuf::from(format!("{}{suffix}", database_path.display())))
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    collect_files(secret_root, &mut files);
    files
}

fn collect_files(root: &std::path::Path, output: &mut Vec<std::path::PathBuf>) {
    if !root.exists() {
        return;
    }
    for entry in fs::read_dir(root).expect("read directory") {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            collect_files(&path, output);
        } else {
            output.push(path);
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

struct NoopRuntime;

impl HostRuntime for NoopRuntime {
    fn started(
        &self,
        _diagnostics_only: bool,
    ) -> Result<(), restoreplane_service::host::HostError> {
        Ok(())
    }

    fn stop_accepting_ipc(&self) -> Result<(), restoreplane_service::host::HostError> {
        Ok(())
    }

    fn stop_claiming_work(&self) -> Result<(), restoreplane_service::host::HostError> {
        Ok(())
    }

    fn drain_clients(&self) -> Result<(), restoreplane_service::host::HostError> {
        Ok(())
    }

    fn checkpointed(&self) -> Result<(), restoreplane_service::host::HostError> {
        Ok(())
    }

    fn stopped(&self, _reason: ShutdownReason) {}
}
