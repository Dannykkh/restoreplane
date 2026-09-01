use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use dbbackup_application::{
    ApplicationCommand, AuthoritySnapshot, BuildIdentityV1, CutoverMode, ExecutionAuthority,
};
use dbbackup_catalog::OperationalStore;
use dbbackup_config::{
    CanonicalConnection, CanonicalDestination, CanonicalPlan, CanonicalTarget,
    CanonicalV1ShadowModel,
};
use dbbackup_domain::IpcRequestId;
use dbbackup_service::application::ServiceApplication;
use dbbackup_service::cutover::{
    CutoverReadinessInputV1, CutoverReadinessVerifierV1, FileCutoverReadinessVerifierV1,
    LegacySchedulerProbeV1,
};
use dbbackup_service::host::{HostConfig, HostRuntime, ServiceHost};
use dbbackup_service::ipc::{
    authorize_and_decode, encode_request, AuthorizationPolicy, DurableIpcDispatcher,
    RawConfirmationToken, RequestEnvelope, TokenIdentity, WindowsSid,
};
use serde_json::{json, Value};
use tempfile::TempDir;
use uuid::Uuid;

const SOURCE_REVISION: &str = "1111111111111111111111111111111111111111";
const SOURCE_TREE_DIGEST: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const BUILD_ID: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const SOURCE_CHECKSUM: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const RELATION_DIGEST: &str = "5555555555555555555555555555555555555555555555555555555555555555";
const REQUIREMENTS_JSON: &str =
    include_str!("../../../release/gates/g1-evidence-requirements.json");
const MATRIX_JSON: &str = include_str!("../../../release/gates/g1-fault-matrix.json");
const SERVICE_SID: &str = "S-1-5-80-1001";
const OWNER_SID: &str = "S-1-5-21-100-200-300-1001";
const OWNER_LOGON_SID: &str = "S-1-5-5-10-20";

#[test]
fn service_owned_files_live_health_and_disabled_tasks_create_a_stable_cutover_proof() {
    let fixture = Fixture::new(false);
    let verifier = fixture.verifier();
    let proof = verifier
        .verify(&fixture.input(), 10_000)
        .expect("verified cutover readiness");

    assert_eq!(proof.shadow_import_id(), fixture.shadow_import_id);
    assert_eq!(proof.source_checksum_blake3(), SOURCE_CHECKSUM);
    assert_eq!(
        proof.relation_digest_blake3(),
        fixture.relation_digest_blake3
    );
    assert_eq!(proof.g1_evidence().source_revision(), SOURCE_REVISION);
    assert_eq!(proof.g1_evidence().source_tree_digest(), SOURCE_TREE_DIGEST);
    assert_eq!(proof.g1_evidence().build_id(), BUILD_ID);
    assert_eq!(proof.recovery_bundle_digest_blake3().len(), 64);
    assert_eq!(proof.legacy_scheduler_freeze_digest_blake3().len(), 64);
    assert_eq!(proof.service_health_digest_blake3().len(), 64);

    let cutover_run_id = Uuid::new_v4();
    let first = proof.config_cutover_spec(cutover_run_id, 10_000);
    let same_readiness_later = verifier
        .verify(&fixture.input(), 10_100)
        .expect("same live readiness later")
        .config_cutover_spec(cutover_run_id, 10_100);
    assert_eq!(
        first.cutover_marker(),
        same_readiness_later.cutover_marker(),
        "server commit time must not change the confirmed readiness fingerprint"
    );
}

#[test]
fn typed_prepare_and_cutover_are_redacted_reverified_atomic_and_replay_safe() {
    let fixture = Fixture::new(false);
    let verifier = Arc::new(fixture.verifier()) as Arc<dyn CutoverReadinessVerifierV1>;
    let application = Arc::new(ServiceApplication::new(
        fixture.host.health_probe(),
        fixture.host.operational_store(),
    ));
    let dispatcher = DurableIpcDispatcher::with_cutover_verifier(
        fixture.store.ipc(),
        application,
        cutover_authority(),
        verifier,
    );
    let cutover_run_id = Uuid::new_v4();
    let prepare_payload = json!({
        "action": "config_cutover",
        "cutoverRunId": cutover_run_id,
        "shadowImportId": fixture.shadow_import_id,
        "sourceChecksumBlake3": SOURCE_CHECKSUM,
        "relationDigestBlake3": fixture.relation_digest_blake3,
        "g1EvidencePath": "g1.json",
        "recoveryBundleManifestPath": "bundle-manifest.json",
        "freezeReceiptPath": "freeze-receipt.json",
        "ttlMs": 60_000
    });
    let prepare_frame = mutation_frame(
        ApplicationCommand::ConfirmationPrepare,
        "config-cutover-prepare-01",
        9_900,
        prepare_payload.clone(),
    );

    let prepared = dispatcher
        .dispatch(authorized_at(&prepare_frame, 10_000))
        .expect("prepare config cutover confirmation");
    assert!(prepared.is_success());
    assert!(!prepared.deduplicated());
    let prepared_body = &prepared.body()["body"];
    assert_eq!(prepared_body["schemaVersion"], 1);
    assert_eq!(prepared_body["action"], "config_cutover");
    assert_eq!(prepared_body["cutoverRunId"], cutover_run_id.to_string());
    assert_eq!(
        prepared_body["shadowImportId"],
        fixture.shadow_import_id.to_string()
    );
    assert_eq!(prepared_body["tokenAvailable"], true);
    let encoded_token = prepared_body["confirmationToken"]
        .as_str()
        .expect("one-time raw token")
        .to_owned();
    let raw_token = RawConfirmationToken::from_encoded(&encoded_token).expect("raw token");
    let confirmation_token_id = prepared_body["confirmationTokenId"]
        .as_str()
        .expect("confirmation token ID")
        .to_owned();
    let cutover_marker = prepared_body["cutoverMarker"]
        .as_str()
        .expect("cutover marker")
        .to_owned();
    let cutover_payload = prepared_body["cutoverRequest"].clone();
    assert_eq!(cutover_payload["confirmationToken"], encoded_token.as_str());
    assert_eq!(
        prepared_body["payloadDigestBlake3"],
        dbbackup_application::canonical_payload_digest(&cutover_payload)
            .expect("cutover payload digest")
    );

    fixture.store.checkpoint_wal().expect("checkpoint prepare");
    let stored_response: String = direct(&fixture.database_path)
        .query_row(
            "SELECT response_json
             FROM ipc_request_ledger
             WHERE command_name = 'confirmation.prepare'",
            [],
            |row| row.get(0),
        )
        .expect("stored prepare response");
    assert!(!stored_response.contains(&encoded_token));
    assert!(!stored_response.contains("\"confirmation_token\""));
    for path in database_files(&fixture.database_path) {
        let bytes = fs::read(path).expect("read database file");
        assert!(!contains(&bytes, encoded_token.as_bytes()));
        assert!(!contains(&bytes, raw_token.expose()));
    }

    let replay = dispatcher
        .dispatch(authorized_at(&prepare_frame, 10_001))
        .expect("prepare replay");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(
        replay.body()["body"]["confirmationTokenId"],
        confirmation_token_id
    );
    assert_eq!(replay.body()["body"]["confirmationToken"], Value::Null);
    assert_eq!(replay.body()["body"]["tokenAvailable"], false);
    assert_eq!(replay.body()["body"]["cutoverRequest"], Value::Null);

    direct(&fixture.database_path)
        .execute_batch(
            "CREATE TRIGGER fail_config_cutover_completion
             BEFORE UPDATE OF state ON ipc_request_ledger
             WHEN OLD.command_name = 'config.cutover'
              AND NEW.state = 'succeeded'
             BEGIN
               SELECT RAISE(ABORT, 'injected config cutover completion failure');
             END;",
        )
        .expect("install cutover completion failure trigger");
    let cutover_frame = mutation_frame(
        ApplicationCommand::ConfigCutover,
        "config-cutover-execute-01",
        10_001,
        cutover_payload.clone(),
    );
    let failed = dispatcher
        .dispatch(authorized_at(&cutover_frame, 10_002))
        .expect_err("late completion failure must roll back cutover");
    assert_eq!(failed.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(
                &confirmation_token_id
                    .parse()
                    .expect("typed confirmation token ID")
            )
            .expect("rolled-back token")
            .state(),
        "issued"
    );
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("rolled-back authority")
            .execution_authority(),
        dbbackup_catalog::PersistedExecutionAuthority::LegacyDirect
    );
    direct(&fixture.database_path)
        .execute_batch("DROP TRIGGER fail_config_cutover_completion;")
        .expect("remove cutover completion failure trigger");

    let committed = dispatcher
        .dispatch(authorized_at(&cutover_frame, 10_003))
        .expect("commit config cutover");
    assert!(committed.is_success());
    assert!(!committed.deduplicated());
    assert_eq!(committed.body()["body"]["cutoverMarker"], cutover_marker);
    assert_eq!(committed.body()["body"]["executionAuthority"], "service");
    assert_eq!(committed.body()["body"]["cutoverMode"], "production");
    let authority = fixture
        .store
        .cutover()
        .authority()
        .expect("persisted production authority");
    assert_eq!(
        authority.execution_authority(),
        dbbackup_catalog::PersistedExecutionAuthority::Service
    );
    assert_eq!(authority.cutover_marker(), Some(cutover_marker.as_str()));
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(
                &confirmation_token_id
                    .parse()
                    .expect("typed confirmation token ID")
            )
            .expect("consumed token")
            .state(),
        "consumed"
    );

    let committed_replay = dispatcher
        .dispatch(authorized_at(&cutover_frame, 10_004))
        .expect("cutover replay");
    assert!(committed_replay.is_success());
    assert!(committed_replay.deduplicated());
    assert_eq!(
        committed_replay.body()["body"]["cutoverMarker"],
        cutover_marker
    );

    let post_cutover = mutation_frame(
        ApplicationCommand::ToolRescan,
        "post-cutover-authority-refresh",
        10_005,
        json!({}),
    );
    let error = dispatcher
        .dispatch(authorized_at(&post_cutover, 10_006))
        .expect_err("typed tool rescan route is not implemented by this dispatcher");
    assert_eq!(
        error.code(),
        "IPC_TYPED_TRANSACTION_REQUIRED",
        "the live dispatcher must observe persisted service authority without a restart"
    );
}

#[test]
fn readiness_drift_after_prepare_rejects_cutover_without_consuming_token() {
    let fixture = Fixture::new(false);
    let verifier = Arc::new(fixture.verifier()) as Arc<dyn CutoverReadinessVerifierV1>;
    let application = Arc::new(ServiceApplication::new(
        fixture.host.health_probe(),
        fixture.host.operational_store(),
    ));
    let dispatcher = DurableIpcDispatcher::with_cutover_verifier(
        fixture.store.ipc(),
        application,
        cutover_authority(),
        verifier,
    );
    let prepare_frame = mutation_frame(
        ApplicationCommand::ConfirmationPrepare,
        "config-cutover-drift-prepare",
        9_900,
        json!({
            "action": "config_cutover",
            "cutoverRunId": Uuid::new_v4(),
            "shadowImportId": fixture.shadow_import_id,
            "sourceChecksumBlake3": SOURCE_CHECKSUM,
            "relationDigestBlake3": fixture.relation_digest_blake3,
            "g1EvidencePath": "g1.json",
            "recoveryBundleManifestPath": "bundle-manifest.json",
            "freezeReceiptPath": "freeze-receipt.json",
            "ttlMs": 60_000
        }),
    );
    let prepared = dispatcher
        .dispatch(authorized_at(&prepare_frame, 10_000))
        .expect("prepare before drift");
    let confirmation_token_id = prepared.body()["body"]["confirmationTokenId"]
        .as_str()
        .expect("confirmation token ID")
        .parse()
        .expect("typed confirmation token ID");
    let cutover_payload = prepared.body()["body"]["cutoverRequest"].clone();

    fs::write(
        fixture.root.path().join("bundle/catalog.db"),
        b"post-prepare-drift",
    )
    .expect("drift bundled catalog");
    let cutover_frame = mutation_frame(
        ApplicationCommand::ConfigCutover,
        "config-cutover-drift-execute",
        10_001,
        cutover_payload,
    );
    let error = dispatcher
        .dispatch(authorized_at(&cutover_frame, 10_002))
        .expect_err("readiness must be reverified at execute");
    assert_eq!(error.code(), "CONFIG_CUTOVER_READINESS_INVALID");
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(&confirmation_token_id)
            .expect("unconsumed token")
            .state(),
        "issued"
    );
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("legacy authority after drift")
            .execution_authority(),
        dbbackup_catalog::PersistedExecutionAuthority::LegacyDirect
    );
}

#[test]
fn stale_g1_tampered_bundle_enabled_task_or_path_escape_fails_closed() {
    let stale = Fixture::new(false);
    let mut evidence: Value = serde_json::from_slice(
        &fs::read(stale.root.path().join("g1.json")).expect("read G1 evidence"),
    )
    .expect("parse G1 evidence");
    evidence["sourceTreeDigest"] =
        json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    fs::write(
        stale.root.path().join("g1.json"),
        serde_json::to_vec(&evidence).expect("stale evidence JSON"),
    )
    .expect("write stale G1 evidence");
    assert_invalid(stale.verifier().verify(&stale.input(), 10_000));

    let tampered = Fixture::new(false);
    fs::write(
        tampered.root.path().join("bundle/catalog.db"),
        b"tampered-catalog",
    )
    .expect("tamper bundled catalog");
    assert_invalid(tampered.verifier().verify(&tampered.input(), 10_000));

    let enabled = Fixture::new(true);
    assert_invalid(enabled.verifier().verify(&enabled.input(), 10_000));

    let stopped = Fixture::new(false);
    stopped
        .host
        .shutdown(dbbackup_service::host::ShutdownReason::Stop)
        .expect("stop service before readiness verification");
    assert_invalid(stopped.verifier().verify(&stopped.input(), 10_000));

    let escaped = Fixture::new(false);
    let input = CutoverReadinessInputV1::new(
        escaped.shadow_import_id,
        SOURCE_CHECKSUM,
        RELATION_DIGEST,
        "../g1.json",
        "bundle-manifest.json",
        "freeze-receipt.json",
    );
    assert_invalid(escaped.verifier().verify(&input, 10_000));
}

fn assert_invalid<T>(result: Result<T, dbbackup_service::cutover::CutoverReadinessError>) {
    let error = match result {
        Ok(_) => panic!("invalid readiness must fail closed"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "CONFIG_CUTOVER_READINESS_INVALID");
}

struct Fixture {
    root: TempDir,
    database_path: std::path::PathBuf,
    store: OperationalStore,
    host: ServiceHost<NoopRuntime>,
    shadow_import_id: Uuid,
    relation_digest_blake3: String,
    probe: Arc<StaticSchedulerProbe>,
}

impl Fixture {
    fn new(task_enabled: bool) -> Self {
        let root = TempDir::new().expect("temp directory");
        fs::create_dir(root.path().join("bundle")).expect("bundle directory");
        let database_path = root.path().join("operational.db");
        let host = ServiceHost::start(
            HostConfig::new(
                format!("cutover-readiness-{}", Uuid::new_v4()),
                database_path.clone(),
            )
            .expect("host config")
            .with_current_build_g1_proven(true),
            Arc::new(NoopRuntime),
        )
        .expect("rehearsal host");
        let store = host
            .operational_store()
            .expect("operational store after service startup");
        let shadow_report = store
            .shadow_import()
            .import_v1(shadow_model(), 7_000)
            .expect("successful shadow import");
        let shadow_import_id =
            Uuid::parse_str(shadow_report.import_run_id()).expect("shadow import UUID");
        let relation_digest_blake3 = shadow_report.relation_digest_blake3().to_owned();

        let task_xml = b"<Task><Settings><Enabled>true</Enabled></Settings></Task>";
        let scheduler_inventory = json!({
            "schemaVersion": 1,
            "tasks": [{
                "taskName": "\\DBBackup\\orders-nightly",
                "enabled": true,
                "taskXml": String::from_utf8(task_xml.to_vec()).expect("task XML"),
                "xmlDigestBlake3": digest(task_xml)
            }]
        });
        let items = vec![
            (
                "legacy_toml",
                "config.toml",
                b"[meta]\nschema_version=1\n".to_vec(),
            ),
            (
                "legacy_keystore",
                "keystore.bin",
                b"encrypted-keystore".to_vec(),
            ),
            ("legacy_catalog", "catalog.db", b"legacy-catalog".to_vec()),
            (
                "scheduler_inventory",
                "scheduler-inventory.json",
                serde_json::to_vec(&scheduler_inventory).expect("scheduler inventory JSON"),
            ),
        ];
        let mut manifest_items = Vec::new();
        for (kind, relative_path, bytes) in items {
            fs::write(root.path().join("bundle").join(relative_path), &bytes).expect("bundle item");
            manifest_items.push(json!({
                "kind": kind,
                "relativePath": relative_path,
                "sizeBytes": bytes.len(),
                "digestBlake3": digest(&bytes)
            }));
        }
        let manifest = json!({
            "schemaVersion": 1,
            "sourceChecksumBlake3": SOURCE_CHECKSUM,
            "createdAtMs": 8_000,
            "bundleDirectory": "bundle",
            "items": manifest_items
        });
        let manifest_bytes = serde_json::to_vec(&manifest).expect("bundle manifest JSON");
        fs::write(root.path().join("bundle-manifest.json"), &manifest_bytes)
            .expect("bundle manifest");
        let manifest_digest = digest(&manifest_bytes);

        let freeze = json!({
            "schemaVersion": 1,
            "sourceChecksumBlake3": SOURCE_CHECKSUM,
            "recoveryBundleDigestBlake3": manifest_digest,
            "frozenAtMs": 9_000,
            "tasks": [{
                "taskName": "\\DBBackup\\orders-nightly",
                "beforeEnabled": true,
                "afterEnabled": false,
                "xmlDigestBlake3": digest(task_xml)
            }]
        });
        fs::write(
            root.path().join("freeze-receipt.json"),
            serde_json::to_vec(&freeze).expect("freeze receipt JSON"),
        )
        .expect("freeze receipt");
        fs::write(
            root.path().join("g1.json"),
            serde_json::to_vec(&valid_g1_evidence()).expect("G1 JSON"),
        )
        .expect("G1 evidence");

        let probe = Arc::new(StaticSchedulerProbe {
            states: BTreeMap::from([("\\DBBackup\\orders-nightly".to_owned(), task_enabled)]),
        });
        Self {
            root,
            database_path,
            store,
            host,
            shadow_import_id,
            relation_digest_blake3,
            probe,
        }
    }

    fn verifier(&self) -> FileCutoverReadinessVerifierV1 {
        FileCutoverReadinessVerifierV1::new(
            self.root.path().to_path_buf(),
            BuildIdentityV1::new(SOURCE_REVISION, SOURCE_TREE_DIGEST, BUILD_ID)
                .expect("build identity"),
            self.host.health_probe(),
            Arc::clone(&self.probe) as Arc<dyn LegacySchedulerProbeV1>,
            REQUIREMENTS_JSON,
            MATRIX_JSON,
        )
        .expect("readiness verifier")
    }

    fn input(&self) -> CutoverReadinessInputV1 {
        CutoverReadinessInputV1::new(
            self.shadow_import_id,
            SOURCE_CHECKSUM,
            self.relation_digest_blake3.clone(),
            "g1.json",
            "bundle-manifest.json",
            "freeze-receipt.json",
        )
    }
}

struct StaticSchedulerProbe {
    states: BTreeMap<String, bool>,
}

impl LegacySchedulerProbeV1 for StaticSchedulerProbe {
    fn task_enabled(&self, task_name: &str) -> Option<bool> {
        self.states.get(task_name).copied()
    }
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

    fn stopped(&self, _reason: dbbackup_service::host::ShutdownReason) {}
}

fn valid_g1_evidence() -> Value {
    let matrix: Value = serde_json::from_str(MATRIX_JSON).expect("G1 matrix");
    let rows = matrix["commands"]
        .as_array()
        .expect("matrix commands")
        .iter()
        .enumerate()
        .map(|(index, command)| {
            json!({
                "id": command["id"],
                "command": command["display"],
                "status": "pass",
                "exitCode": 0,
                "startedAt": format!("2026-07-29T00:00:{index:02}+09:00"),
                "finishedAt": format!("2026-07-29T00:01:{index:02}+09:00"),
                "durationMs": 1.25,
                "sourceRevision": SOURCE_REVISION,
                "sourceTreeDigest": SOURCE_TREE_DIGEST,
                "buildId": BUILD_ID,
                "runnerOs": "windows-test-runner"
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schemaVersion": 1,
        "gateId": "G1",
        "sourceRevision": SOURCE_REVISION,
        "sourceTreeDigest": SOURCE_TREE_DIGEST,
        "buildId": BUILD_ID,
        "startedAt": "2026-07-29T00:00:00+09:00",
        "finishedAt": "2026-07-29T00:02:00+09:00",
        "runner": {
            "os": "windows-test-runner",
            "architecture": "x86_64",
            "powershell": "5.1",
            "rustc": "rustc test",
            "cargo": "cargo test"
        },
        "evidence": rows,
        "failureCount": 0,
        "status": "pass"
    })
}

fn shadow_model() -> CanonicalV1ShadowModel {
    let source_fingerprint = digest(b"source-fingerprint");
    let content_hash = digest(b"plan-content");
    CanonicalV1ShadowModel {
        source_checksum_blake3: SOURCE_CHECKSUM.to_owned(),
        connections: vec![CanonicalConnection {
            key: "postgresql|db.internal|5432|backup".to_owned(),
            display_name: "db.internal".to_owned(),
            engine: "postgresql".to_owned(),
            host: "db.internal".to_owned(),
            port: 5432,
            user: "backup".to_owned(),
            source_resource_key: "legacy:db.internal".to_owned(),
        }],
        targets: vec![CanonicalTarget {
            key: "postgresql|db.internal|5432|orders".to_owned(),
            connection_key: "postgresql|db.internal|5432|backup".to_owned(),
            engine_object_id: "orders".to_owned(),
            display_name: "orders".to_owned(),
            normalized_name: "orders".to_owned(),
            source_fingerprint,
        }],
        destinations: vec![CanonicalDestination {
            key: "local|d:/backup-vault".to_owned(),
            display_name: "local-vault".to_owned(),
            kind: "local".to_owned(),
            endpoint_json: r#"{"path":"D:/backup-vault"}"#.to_owned(),
            declared_capabilities_json: r#"{"write":true}"#.to_owned(),
            immutability_capability: "none".to_owned(),
        }],
        plans: vec![CanonicalPlan {
            key: "orders-nightly".to_owned(),
            revision_key: "orders-nightly|v1".to_owned(),
            display_name: "orders-nightly".to_owned(),
            target_key: "postgresql|db.internal|5432|orders".to_owned(),
            destination_keys: vec!["local|d:/backup-vault".to_owned()],
            enabled: true,
            schedule_expression: "0 2 * * *".to_owned(),
            retention_policy_json: r#"{"keepLast":7}"#.to_owned(),
            verification_policy_json: r#"{"level":"l1"}"#.to_owned(),
            execution_policy_json: r#"{"requiredDestinations":"all"}"#.to_owned(),
            content_hash_blake3: content_hash,
        }],
        secret_refs: Vec::new(),
        secret_bindings: Vec::new(),
    }
}

fn mutation_frame(
    command: ApplicationCommand,
    idempotency_key: &str,
    issued_at_ms: i64,
    payload: Value,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            command,
            Some(idempotency_key.to_owned()),
            issued_at_ms,
            issued_at_ms + 100_000,
            payload,
        )
        .expect("mutation request"),
    )
    .expect("encoded mutation request")
}

fn authorized_at(frame: &[u8], accepted_at_ms: i64) -> dbbackup_service::ipc::AuthorizedRequest {
    authorize_and_decode(&policy(), &owner_identity(), frame, accepted_at_ms)
        .expect("authorized request")
}

fn policy() -> AuthorizationPolicy {
    AuthorizationPolicy::new(
        sid(SERVICE_SID),
        sid(OWNER_SID),
        sid(OWNER_LOGON_SID),
        10,
        Vec::new(),
    )
    .expect("authorization policy")
}

fn owner_identity() -> TokenIdentity {
    TokenIdentity::new(sid(OWNER_SID), sid(OWNER_LOGON_SID), 10, false, true)
}

fn sid(value: &str) -> WindowsSid {
    WindowsSid::parse(value).expect("canonical SID")
}

const fn cutover_authority() -> AuthoritySnapshot {
    AuthoritySnapshot::new(
        ExecutionAuthority::LegacyDirect,
        CutoverMode::RehearsalOnly,
        true,
        false,
    )
}

fn direct(path: &std::path::Path) -> dbbackup_catalog::rusqlite::Connection {
    let connection =
        dbbackup_catalog::rusqlite::Connection::open(path).expect("open direct connection");
    connection
        .execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
        .expect("configure direct connection");
    connection
}

fn database_files(path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths = vec![path.to_path_buf()];
    for suffix in ["-wal", "-shm"] {
        let candidate = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if candidate.exists() {
            paths.push(candidate);
        }
    }
    paths
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}
