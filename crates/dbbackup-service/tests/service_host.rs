#![cfg(windows)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use dbbackup_application::{CutoverMode, ExecutionAuthority};
use dbbackup_catalog::{
    rusqlite::Connection, ConfigCutoverSpec, ConfirmationSpec, G1EvidenceBindingV1, IpcRequestSpec,
    OperationalStore, PersistedCutoverRecoveryDisposition, StandaloneAuthorityBootstrapSpec,
};
use dbbackup_config::{
    CanonicalConnection, CanonicalDestination, CanonicalPlan, CanonicalTarget,
    CanonicalV1ShadowModel,
};
use dbbackup_domain::{ConfirmationTokenId, IpcRequestId};
use dbbackup_service::host::{HostConfig, HostPhase, HostRuntime, ServiceHost, ShutdownReason};
use tempfile::TempDir;
use uuid::Uuid;

const SOURCE_CHECKSUM: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const SID_HASH: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const PAYLOAD_DIGEST: &str = "3333333333333333333333333333333333333333333333333333333333333333";

#[test]
fn singleton_host_is_gui_independent_and_pre_g1_is_rehearsal_only() {
    let temp = TempDir::new().expect("temp directory");
    let config = config(&temp, "singleton");
    let runtime = Arc::new(RecordingRuntime::default());
    let host =
        ServiceHost::start(config.clone(), Arc::clone(&runtime)).expect("start first service");

    let health = host.health();
    assert_eq!(health.phase(), HostPhase::RehearsalReady);
    assert_eq!(health.schema_version(), Some(4));
    assert_eq!(health.application_table_count(), Some(67));
    assert_eq!(
        health.execution_authority(),
        ExecutionAuthority::LegacyDirect
    );
    assert_eq!(health.cutover_mode(), CutoverMode::RehearsalOnly);
    assert!(!health.claims_enabled());
    assert!(health.accepts_ipc());

    let duplicate = ServiceHost::start(config.clone(), Arc::new(RecordingRuntime::default()))
        .expect_err("second instance cannot own authority");
    assert_eq!(duplicate.code(), "SERVICE_SINGLETON_ALREADY_RUNNING");

    let gui_client_lifetime = Arc::new(());
    drop(gui_client_lifetime);
    assert_eq!(host.health().phase(), HostPhase::RehearsalReady);

    host.shutdown(ShutdownReason::Stop)
        .expect("ordered service stop");
    assert_eq!(host.health().phase(), HostPhase::Stopped);
    assert_eq!(
        runtime.events(),
        [
            "started",
            "stop_accepting",
            "stop_claiming",
            "drain_clients",
            "checkpointed",
            "stopped",
        ]
    );
    drop(host);

    let restarted = ServiceHost::start(config, Arc::new(RecordingRuntime::default()))
        .expect("lock is released after service exits");
    assert_eq!(restarted.health().phase(), HostPhase::RehearsalReady);
}

#[test]
fn installer_attested_new_store_bootstraps_and_persists_standalone_service_authority() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("new-standalone.db");
    let bootstrap = standalone_bootstrap(1_000);
    let host = ServiceHost::start(
        HostConfig::new("new-standalone", database_path.clone())
            .expect("host config")
            .with_pristine_standalone_bootstrap(bootstrap),
        Arc::new(RecordingRuntime::default()),
    )
    .expect("fresh standalone host");

    assert_eq!(host.health().phase(), HostPhase::ProductionReady);
    assert_eq!(
        host.health().execution_authority(),
        ExecutionAuthority::Service
    );
    assert_eq!(host.health().cutover_mode(), CutoverMode::Production);
    assert!(host.health().claims_enabled());
    let marker = host
        .health()
        .cutover_marker()
        .expect("standalone authority marker")
        .to_owned();
    assert!(marker.starts_with("standalone-bootstrap-v1:"));
    host.shutdown(ShutdownReason::Stop).expect("stop host");
    drop(host);

    let restarted = ServiceHost::start(
        HostConfig::new("new-standalone-restart", database_path).expect("restart config"),
        Arc::new(RecordingRuntime::default()),
    )
    .expect("restart standalone host");
    assert_eq!(restarted.health().phase(), HostPhase::ProductionReady);
    assert!(restarted.health().claims_enabled());
    assert_eq!(restarted.health().cutover_marker(), Some(marker.as_str()));
}

#[test]
fn existing_store_never_uses_pristine_standalone_bootstrap_hint() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("already-existing.db");
    OperationalStore::open(database_path.clone()).expect("pre-existing store");

    let host = ServiceHost::start(
        HostConfig::new("already-existing", database_path)
            .expect("host config")
            .with_pristine_standalone_bootstrap(standalone_bootstrap(1_000)),
        Arc::new(RecordingRuntime::default()),
    )
    .expect("existing host remains available");
    assert_eq!(host.health().phase(), HostPhase::RehearsalReady);
    assert_eq!(
        host.health().execution_authority(),
        ExecutionAuthority::LegacyDirect
    );
    assert!(!host.health().claims_enabled());
}

#[test]
fn preshutdown_is_ordered_idempotent_and_schema_drift_is_diagnostics_only() {
    let temp = TempDir::new().expect("temp directory");
    let runtime = Arc::new(RecordingRuntime::default());
    let host = ServiceHost::start(config(&temp, "preshutdown"), Arc::clone(&runtime))
        .expect("start service");
    host.shutdown(ShutdownReason::Preshutdown)
        .expect("preshutdown");
    host.shutdown(ShutdownReason::Stop)
        .expect("second shutdown is idempotent");
    assert_eq!(
        runtime.events(),
        [
            "started",
            "stop_accepting",
            "stop_claiming",
            "drain_clients",
            "checkpointed",
            "stopped",
        ]
    );

    let drift_path = temp.path().join("drift.db");
    let connection = Connection::open(&drift_path).expect("create drift database");
    connection
        .execute_batch("PRAGMA user_version=99;")
        .expect("write drift marker");
    drop(connection);
    let drift_runtime = Arc::new(RecordingRuntime::default());
    let drift = ServiceHost::start(
        HostConfig::new("diagnostics-only", drift_path).expect("drift config"),
        Arc::clone(&drift_runtime),
    )
    .expect("service shell remains alive for diagnostics");
    assert_eq!(drift.health().phase(), HostPhase::DiagnosticsOnly);
    assert!(!drift.health().claims_enabled());
    assert!(drift.health().accepts_ipc());
    assert_eq!(
        drift.health().diagnostic_code(),
        Some("OPERATIONAL_MIGRATION_VERSION_DRIFT")
    );
}

#[test]
fn persisted_cutover_is_the_only_restart_authority_and_torn_state_is_diagnostics_only() {
    let temp = TempDir::new().expect("temp directory");
    let production_path = temp.path().join("persisted-production.db");
    commit_cutover(&production_path);

    let production = ServiceHost::start(
        HostConfig::new("persisted-production", production_path.clone())
            .expect("production host config"),
        Arc::new(RecordingRuntime::default()),
    )
    .expect("restart committed production host");
    assert_eq!(production.health().phase(), HostPhase::ProductionReady);
    assert_eq!(
        production.health().execution_authority(),
        ExecutionAuthority::Service
    );
    assert_eq!(production.health().cutover_mode(), CutoverMode::Production);
    assert!(production
        .health()
        .cutover_marker()
        .is_some_and(|marker| marker.starts_with("config-cutover-v1:")));
    assert_eq!(
        production.health().recovery_disposition(),
        Some(PersistedCutoverRecoveryDisposition::RollbackEligible)
    );
    assert!(production.health().claims_enabled());
    assert_eq!(production.health().diagnostic_code(), None);
    production
        .shutdown(ShutdownReason::Stop)
        .expect("stop production host");
    drop(production);

    let restarted = ServiceHost::start(
        HostConfig::new("persisted-production-restart", production_path)
            .expect("restart host config"),
        Arc::new(RecordingRuntime::default()),
    )
    .expect("second production restart");
    assert_eq!(restarted.health().phase(), HostPhase::ProductionReady);
    assert!(restarted.health().claims_enabled());
    restarted
        .shutdown(ShutdownReason::Stop)
        .expect("stop restarted production host");

    let empty_path = temp.path().join("empty-rehearsal.db");
    let empty = ServiceHost::start(
        HostConfig::new("empty-rehearsal", empty_path)
            .expect("empty host config")
            .with_current_build_g1_proven(true),
        Arc::new(RecordingRuntime::default()),
    )
    .expect("empty store remains rehearsal");
    assert_eq!(empty.health().phase(), HostPhase::RehearsalReady);
    assert_eq!(
        empty.health().execution_authority(),
        ExecutionAuthority::LegacyDirect
    );
    assert_eq!(empty.health().cutover_mode(), CutoverMode::RehearsalOnly);
    assert!(!empty.health().claims_enabled());
    empty
        .shutdown(ShutdownReason::Stop)
        .expect("stop empty rehearsal host");

    let torn_path = temp.path().join("torn-authority.db");
    commit_cutover(&torn_path);
    Connection::open(&torn_path)
        .expect("open torn projection")
        .execute(
            "UPDATE app_meta
             SET value_text = '{}'
             WHERE key = 'execution_authority.v1'",
            [],
        )
        .expect("tamper authority projection");
    let torn = ServiceHost::start(
        HostConfig::new("torn-authority", torn_path)
            .expect("torn host config")
            .with_current_build_g1_proven(true),
        Arc::new(RecordingRuntime::default()),
    )
    .expect("service shell remains available for diagnostics");
    assert_eq!(torn.health().phase(), HostPhase::DiagnosticsOnly);
    assert_eq!(
        torn.health().execution_authority(),
        ExecutionAuthority::LegacyDirect
    );
    assert_eq!(torn.health().cutover_mode(), CutoverMode::RehearsalOnly);
    assert!(!torn.health().claims_enabled());
    assert!(torn.health().accepts_ipc());
    assert_eq!(
        torn.health().diagnostic_code(),
        Some("CONFIG_CUTOVER_STATE_INVALID")
    );
    assert!(torn.operational_store().is_none());
}

#[test]
fn minimum_reader_version_blocks_old_binary_writes_but_keeps_diagnostics_available() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("minimum-reader.db");
    commit_cutover(&database_path);
    let store = OperationalStore::open(database_path.clone()).expect("open cutover store");
    let authority = store.cutover().authority().expect("production authority");
    let marker = authority
        .cutover_marker()
        .expect("cutover marker")
        .to_owned();
    store
        .cutover()
        .advance_minimum_reader_version("9.0.0", &marker, 3_000)
        .expect("advance minimum reader");
    store.checkpoint_wal().expect("checkpoint");
    drop(store);

    let old_runtime = Arc::new(RecordingRuntime::default());
    let old_host = ServiceHost::start(
        HostConfig::new("minimum-reader-old", database_path.clone()).expect("old host config"),
        Arc::clone(&old_runtime),
    )
    .expect("old service shell starts in diagnostics mode");
    assert_eq!(old_host.health().phase(), HostPhase::DiagnosticsOnly);
    assert_eq!(
        old_host.health().diagnostic_code(),
        Some("SERVICE_MINIMUM_READER_VERSION_UNMET")
    );
    assert_eq!(old_host.health().minimum_reader_version(), Some("9.0.0"));
    assert!(!old_host.health().claims_enabled());
    assert!(old_host.health().accepts_ipc());
    assert!(old_host.operational_store().is_some());
    old_host
        .shutdown(ShutdownReason::Stop)
        .expect("stop old diagnostics host");
    drop(old_host);

    let compatible = ServiceHost::start(
        HostConfig::new("minimum-reader-compatible", database_path)
            .expect("compatible config")
            .with_current_binary_version("9.0.0"),
        Arc::new(RecordingRuntime::default()),
    )
    .expect("compatible host");
    assert_eq!(compatible.health().phase(), HostPhase::ProductionReady);
    assert!(compatible.health().claims_enabled());
}

#[derive(Default)]
struct RecordingRuntime {
    events: Mutex<Vec<&'static str>>,
}

impl RecordingRuntime {
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().expect("events lock").clone()
    }

    fn push(&self, event: &'static str) {
        self.events.lock().expect("events lock").push(event);
    }
}

impl HostRuntime for RecordingRuntime {
    fn started(&self, _diagnostics_only: bool) -> Result<(), dbbackup_service::host::HostError> {
        self.push("started");
        Ok(())
    }

    fn stop_accepting_ipc(&self) -> Result<(), dbbackup_service::host::HostError> {
        self.push("stop_accepting");
        Ok(())
    }

    fn stop_claiming_work(&self) -> Result<(), dbbackup_service::host::HostError> {
        self.push("stop_claiming");
        Ok(())
    }

    fn drain_clients(&self) -> Result<(), dbbackup_service::host::HostError> {
        self.push("drain_clients");
        Ok(())
    }

    fn checkpointed(&self) -> Result<(), dbbackup_service::host::HostError> {
        self.push("checkpointed");
        Ok(())
    }

    fn stopped(&self, _reason: ShutdownReason) {
        self.push("stopped");
    }
}

fn config(temp: &TempDir, suffix: &str) -> HostConfig {
    HostConfig::new(
        format!("test-{}-{suffix}", std::process::id()),
        PathBuf::from(temp.path()).join(format!("{suffix}.db")),
    )
    .expect("host config")
}

fn commit_cutover(path: &std::path::Path) {
    let store = OperationalStore::open(path.to_path_buf()).expect("operational store");
    let report = store
        .shadow_import()
        .import_v1(shadow_model(), 1_000)
        .expect("shadow import");
    let cutover = ConfigCutoverSpec::new(
        Uuid::new_v4(),
        Uuid::parse_str(report.import_run_id()).expect("shadow import UUID"),
        SOURCE_CHECKSUM,
        report.relation_digest_blake3(),
        digest("recovery-bundle"),
        digest("scheduler-freeze"),
        digest("service-health"),
        G1EvidenceBindingV1::new(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            digest("source-tree"),
            digest("build"),
            digest("g1-evidence"),
        ),
        2_001,
    );
    let token_id = ConfirmationTokenId::new();
    let token_digest = digest(token_id.as_str());
    store
        .ipc()
        .issue_confirmation(ConfirmationSpec::new(
            token_id.clone(),
            token_digest.clone(),
            "config_cutover",
            SID_HASH,
            cutover.confirmation_resource_type(),
            report.import_run_id(),
            cutover.cutover_marker(),
            PAYLOAD_DIGEST,
            1_900,
            12_000,
        ))
        .expect("cutover confirmation");
    store
        .ipc()
        .commit_config_cutover(
            IpcRequestSpec::new(
                IpcRequestId::new(),
                "config.cutover",
                SID_HASH,
                "host-persisted-cutover",
                PAYLOAD_DIGEST,
                2_000,
                12_000,
            ),
            &token_id,
            &token_digest,
            cutover,
        )
        .expect("committed cutover");
    store.checkpoint_wal().expect("checkpoint committed store");
}

fn shadow_model() -> CanonicalV1ShadowModel {
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
            source_fingerprint: digest("source-fingerprint"),
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
            content_hash_blake3: digest("plan-content"),
        }],
        secret_refs: Vec::new(),
        secret_bindings: Vec::new(),
    }
}

fn digest(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn standalone_bootstrap(committed_at_ms: i64) -> StandaloneAuthorityBootstrapSpec {
    StandaloneAuthorityBootstrapSpec::new(
        Uuid::parse_str("01234567-89ab-4cde-8fab-0123456789ab").expect("installation UUID"),
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        committed_at_ms,
    )
}
