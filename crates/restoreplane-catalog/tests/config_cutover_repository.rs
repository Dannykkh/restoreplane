use std::str::FromStr;

use restoreplane_catalog::{
    ConfigCutoverConfirmationRequestSpec, ConfigCutoverRollbackReason, ConfigCutoverRollbackSpec,
    ConfigCutoverSpec, ConfirmationSpec, G1EvidenceBindingV1, IpcCompletionSpec,
    IpcLedgerReservation, IpcRequestSpec, OperationalStore, PersistedCutoverMode,
    PersistedCutoverRecoveryDisposition, PersistedExecutionAuthority,
};
use restoreplane_config::{
    CanonicalConnection, CanonicalDestination, CanonicalPlan, CanonicalTarget,
    CanonicalV1ShadowModel,
};
use restoreplane_domain::{ConfirmationTokenId, IpcRequestId};
use tempfile::TempDir;
use uuid::Uuid;

const SOURCE_CHECKSUM: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const CLIENT_SID_HASH: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const PREPARE_PAYLOAD_DIGEST: &str =
    "3333333333333333333333333333333333333333333333333333333333333333";
const PAYLOAD_DIGEST: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const RECOVERY_BUNDLE_DIGEST: &str =
    "5555555555555555555555555555555555555555555555555555555555555555";
const LEGACY_FREEZE_DIGEST: &str =
    "6666666666666666666666666666666666666666666666666666666666666666";
const SERVICE_HEALTH_DIGEST: &str =
    "7777777777777777777777777777777777777777777777777777777777777777";
const G1_SOURCE_REVISION: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const G1_SOURCE_TREE_DIGEST: &str =
    "8888888888888888888888888888888888888888888888888888888888888888";
const G1_BUILD_ID: &str = "9999999999999999999999999999999999999999999999999999999999999999";
const G1_EVIDENCE_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

#[test]
fn cutover_confirmation_prepare_is_atomic_redacted_and_idempotent() {
    let fixture = Fixture::new();
    let cutover_run_id = Uuid::new_v4();
    let spec = fixture.spec(cutover_run_id, 2_001);
    let confirmation_token_id = ConfirmationTokenId::new();
    let confirmation_token_digest = token_digest(&confirmation_token_id);
    let prepare_request = confirmation_prepare_request("cutover-prepare", 1_900);

    let first = fixture
        .store
        .ipc()
        .issue_config_cutover_confirmation_request(
            prepare_request.clone(),
            ConfigCutoverConfirmationRequestSpec::new(
                confirmation_token_id.clone(),
                confirmation_token_digest.clone(),
                cutover_run_id,
                Uuid::from_str(fixture.shadow_import_id()).expect("shadow import UUID"),
                spec.cutover_marker(),
                PAYLOAD_DIGEST,
                11_900,
            ),
        )
        .expect("atomic config cutover confirmation");
    assert!(first.created());
    assert_eq!(first.confirmation_token_id(), &confirmation_token_id);
    assert_eq!(first.cutover_run_id(), cutover_run_id);
    assert_eq!(
        first.shadow_import_id().to_string(),
        fixture.shadow_import_id()
    );
    assert_eq!(first.cutover_marker(), spec.cutover_marker());
    assert_eq!(first.payload_digest_blake3(), PAYLOAD_DIGEST);
    assert_eq!(first.issued_at_ms(), 1_900);
    assert_eq!(first.expires_at_ms(), 11_900);

    let replay = fixture
        .store
        .ipc()
        .issue_config_cutover_confirmation_request(
            prepare_request,
            ConfigCutoverConfirmationRequestSpec::new(
                ConfirmationTokenId::new(),
                digest("discarded-replay-token"),
                cutover_run_id,
                Uuid::from_str(fixture.shadow_import_id()).expect("shadow import UUID"),
                spec.cutover_marker(),
                digest("discarded-replay-execute-payload"),
                12_000,
            ),
        )
        .expect("durable prepare replay");
    assert!(!replay.created());
    assert_eq!(replay.confirmation_token_id(), &confirmation_token_id);
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(&confirmation_token_id)
            .expect("issued confirmation")
            .state(),
        "issued"
    );

    fixture.store.checkpoint_wal().expect("checkpoint prepare");
    let connection = restoreplane_catalog::rusqlite::Connection::open(&fixture.database_path)
        .expect("prepare inspection handle");
    let response_json: String = connection
        .query_row(
            "SELECT response_json
             FROM ipc_request_ledger
             WHERE command_name = 'confirmation.prepare'",
            [],
            |row| row.get(0),
        )
        .expect("stored redacted response");
    let stored: serde_json::Value =
        serde_json::from_str(&response_json).expect("stored response JSON");
    assert_eq!(
        stored.get("token_available"),
        Some(&serde_json::json!(false))
    );
    assert!(stored.get("confirmation_token").is_none());
    assert!(stored.get("token_digest_blake3").is_none());
    assert!(!response_json.contains(&confirmation_token_digest));
}

#[test]
fn cutover_confirmation_prepare_completion_failure_rolls_back_ledger_and_token() {
    let fixture = Fixture::new();
    fixture.store.checkpoint_wal().expect("checkpoint schema");
    let connection = restoreplane_catalog::rusqlite::Connection::open(&fixture.database_path)
        .expect("failure injection handle");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_cutover_confirmation_completion
             BEFORE UPDATE OF state ON ipc_request_ledger
             WHEN OLD.command_name = 'confirmation.prepare'
              AND NEW.state = 'succeeded'
             BEGIN
               SELECT RAISE(ABORT, 'injected cutover prepare completion failure');
             END;",
        )
        .expect("install completion failure trigger");
    drop(connection);

    let cutover_run_id = Uuid::new_v4();
    let spec = fixture.spec(cutover_run_id, 3_001);
    let confirmation_token_id = ConfirmationTokenId::new();
    let error = fixture
        .store
        .ipc()
        .issue_config_cutover_confirmation_request(
            confirmation_prepare_request("cutover-prepare-rollback", 2_900),
            ConfigCutoverConfirmationRequestSpec::new(
                confirmation_token_id.clone(),
                token_digest(&confirmation_token_id),
                cutover_run_id,
                Uuid::from_str(fixture.shadow_import_id()).expect("shadow import UUID"),
                spec.cutover_marker(),
                PAYLOAD_DIGEST,
                12_900,
            ),
        )
        .expect_err("late prepare completion failure");
    assert_eq!(error.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    assert_eq!(
        fixture
            .store
            .ipc()
            .request_count()
            .expect("rolled-back request ledger"),
        0
    );
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(&confirmation_token_id)
            .expect_err("rolled-back confirmation token")
            .code(),
        "OPERATIONAL_REPOSITORY_NOT_FOUND"
    );
}

#[test]
fn cutover_is_atomic_durable_idempotent_and_single_authority() {
    let fixture = Fixture::new();
    let initial = fixture
        .store
        .cutover()
        .authority()
        .expect("initial authority");
    assert_eq!(
        initial.execution_authority(),
        PersistedExecutionAuthority::LegacyDirect
    );
    assert_eq!(initial.cutover_mode(), PersistedCutoverMode::RehearsalOnly);
    assert!(!initial.g1_proven());
    assert_eq!(initial.cutover_marker(), None);
    assert!(fixture
        .store
        .cutover()
        .minimum_reader_version()
        .expect("initial minimum reader")
        .is_none());

    let spec = fixture.spec(Uuid::new_v4(), 2_001);
    let confirmation_token_id = ConfirmationTokenId::new();
    let confirmation_token_digest = token_digest(&confirmation_token_id);
    fixture.issue_confirmation(&confirmation_token_id, &confirmation_token_digest, &spec);
    let primary_request = request("cutover-primary", 2_000);

    let first = fixture
        .store
        .ipc()
        .commit_config_cutover(
            primary_request.clone(),
            &confirmation_token_id,
            &confirmation_token_digest,
            spec.clone(),
        )
        .expect("atomic config cutover");
    assert!(first.created());
    assert_eq!(first.cutover_marker(), spec.cutover_marker());
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(&confirmation_token_id)
            .expect("consumed confirmation")
            .state(),
        "consumed"
    );

    let replay = fixture
        .store
        .ipc()
        .commit_config_cutover(
            primary_request,
            &confirmation_token_id,
            &confirmation_token_digest,
            spec.clone(),
        )
        .expect("durable idempotent replay");
    assert!(!replay.created());
    assert_eq!(replay.cutover_marker(), spec.cutover_marker());

    let authority = fixture
        .store
        .cutover()
        .authority()
        .expect("persisted production authority");
    assert_eq!(
        authority.execution_authority(),
        PersistedExecutionAuthority::Service
    );
    assert_eq!(authority.cutover_mode(), PersistedCutoverMode::Production);
    assert!(authority.g1_proven());
    assert_eq!(authority.cutover_marker(), Some(spec.cutover_marker()));
    assert_eq!(authority.g1_source_revision(), Some(G1_SOURCE_REVISION));
    assert_eq!(
        authority.g1_source_tree_digest(),
        Some(G1_SOURCE_TREE_DIGEST)
    );
    assert_eq!(authority.g1_build_id(), Some(G1_BUILD_ID));
    assert_eq!(
        authority.recovery_disposition(),
        Some(PersistedCutoverRecoveryDisposition::RollbackEligible)
    );
    let minimum_reader = fixture
        .store
        .cutover()
        .minimum_reader_version()
        .expect("minimum reader after cutover")
        .expect("minimum reader record");
    assert_eq!(
        minimum_reader.minimum_reader_version(),
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(minimum_reader.cutover_marker(), spec.cutover_marker());
    assert_eq!(minimum_reader.updated_at_ms(), 2_001);
    assert!(fixture
        .store
        .cutover()
        .advance_minimum_reader_version("0.2.0", spec.cutover_marker(), 2_050)
        .expect("advance minimum reader"));
    assert!(!fixture
        .store
        .cutover()
        .advance_minimum_reader_version("0.2.0", spec.cutover_marker(), 2_051)
        .expect("same minimum reader is idempotent"));
    assert_eq!(
        fixture
            .store
            .cutover()
            .advance_minimum_reader_version("0.1.9", spec.cutover_marker(), 2_060)
            .expect_err("minimum reader cannot regress")
            .code(),
        "MINIMUM_READER_VERSION_REGRESSION"
    );
    let blocked = fixture
        .store
        .cutover()
        .require_legacy_direct_execution()
        .expect_err("legacy direct execution must stop after cutover");
    assert_eq!(blocked.code(), "LEGACY_DIRECT_EXECUTION_BLOCKED");

    let production_request_id = IpcRequestId::new();
    let production_payload_digest = digest("first-production-payload");
    let production_request = IpcRequestSpec::new(
        production_request_id.clone(),
        "settings.update",
        CLIENT_SID_HASH,
        "first-production-write",
        production_payload_digest.clone(),
        2_100,
        12_100,
    );
    assert!(matches!(
        fixture
            .store
            .ipc()
            .reserve_request(production_request)
            .expect("reserve first production mutation"),
        IpcLedgerReservation::New
    ));
    fixture
        .store
        .ipc()
        .complete_request(
            &production_request_id,
            &production_payload_digest,
            IpcCompletionSpec::succeeded(r#"{"schemaVersion":1}"#, None, 2_101),
        )
        .expect("complete first production mutation");
    fixture.store.checkpoint_wal().expect("checkpoint epoch");
    let epoch_connection = restoreplane_catalog::rusqlite::Connection::open(&fixture.database_path)
        .expect("epoch inspection handle");
    let epoch_value: String = epoch_connection
        .query_row(
            "SELECT value_text FROM app_meta WHERE key = 'production_epoch.v1'",
            [],
            |row| row.get(0),
        )
        .expect("production epoch");
    assert!(
        epoch_value.starts_with("forward_fix_required:"),
        "unexpected epoch value: {epoch_value}"
    );
    assert!(
        epoch_connection
            .execute(
                "UPDATE app_meta
                 SET value_text = 'rollback_eligible'
                 WHERE key = 'production_epoch.v1'",
                [],
            )
            .is_err(),
        "forward-fix boundary must never reverse to rollback eligible"
    );
    drop(epoch_connection);
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("forward-fix authority")
            .recovery_disposition(),
        Some(PersistedCutoverRecoveryDisposition::ForwardFixRequired)
    );

    let second_spec = fixture.spec(Uuid::new_v4(), 2_101);
    let second_token_id = ConfirmationTokenId::new();
    let second_token_digest = token_digest(&second_token_id);
    fixture.issue_confirmation(&second_token_id, &second_token_digest, &second_spec);
    let error = fixture
        .store
        .ipc()
        .commit_config_cutover(
            request("cutover-second-authority", 2_100),
            &second_token_id,
            &second_token_digest,
            second_spec,
        )
        .expect_err("a second production authority marker must be rejected");
    assert_eq!(error.code(), "CONFIG_CUTOVER_ALREADY_COMMITTED");
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(&second_token_id)
            .expect("unconsumed second confirmation")
            .state(),
        "issued"
    );

    let database_path = fixture.database_path.clone();
    drop(fixture);
    let reopened = OperationalStore::open(database_path).expect("reopen operational store");
    let durable = reopened
        .cutover()
        .authority()
        .expect("durable authority after restart");
    assert_eq!(
        durable.execution_authority(),
        PersistedExecutionAuthority::Service
    );
    assert_eq!(durable.cutover_marker(), Some(spec.cutover_marker()));
    assert_eq!(
        durable.recovery_disposition(),
        Some(PersistedCutoverRecoveryDisposition::ForwardFixRequired)
    );
}

#[test]
fn clean_cutover_rollback_is_atomic_audited_idempotent_and_allows_a_new_cutover() {
    let fixture = Fixture::new();
    let first_spec = fixture.spec(Uuid::new_v4(), 5_001);
    let first_token_id = ConfirmationTokenId::new();
    let first_token_digest = token_digest(&first_token_id);
    fixture.issue_confirmation(&first_token_id, &first_token_digest, &first_spec);
    fixture
        .store
        .ipc()
        .commit_config_cutover(
            request("cutover-before-clean-rollback", 5_000),
            &first_token_id,
            &first_token_digest,
            first_spec.clone(),
        )
        .expect("cutover before rollback");

    let rollback = ConfigCutoverRollbackSpec::new(
        Uuid::new_v4(),
        first_spec.cutover_marker(),
        RECOVERY_BUNDLE_DIGEST,
        digest("legacy-scheduler-restore-receipt"),
        CLIENT_SID_HASH,
        ConfigCutoverRollbackReason::PostCutoverHealthFailed,
        5_100,
    );
    let first = fixture
        .store
        .cutover()
        .rollback_if_clean(rollback.clone())
        .expect("clean rollback");
    assert!(first.created());
    assert_eq!(first.rollback_id(), rollback.rollback_id());
    assert_eq!(first.cutover_marker(), first_spec.cutover_marker());

    let authority = fixture
        .store
        .cutover()
        .authority()
        .expect("legacy authority after rollback");
    assert_eq!(
        authority.execution_authority(),
        PersistedExecutionAuthority::LegacyDirect
    );
    assert_eq!(
        authority.cutover_mode(),
        PersistedCutoverMode::RehearsalOnly
    );
    assert_eq!(authority.recovery_disposition(), None);
    fixture
        .store
        .cutover()
        .require_legacy_direct_execution()
        .expect("old path is available only after a completed clean rollback");

    let replay = fixture
        .store
        .cutover()
        .rollback_if_clean(rollback)
        .expect("idempotent rollback replay");
    assert!(!replay.created());
    assert_eq!(replay.rollback_id(), first.rollback_id());

    fixture
        .store
        .checkpoint_wal()
        .expect("checkpoint rollback audit");
    let connection = restoreplane_catalog::rusqlite::Connection::open(&fixture.database_path)
        .expect("rollback audit inspection");
    let active_meta_count: i64 = connection
        .query_row(
            "SELECT COUNT(*)
             FROM app_meta
             WHERE key IN ('execution_authority.v1', 'production_epoch.v1')",
            [],
            |row| row.get(0),
        )
        .expect("active authority metadata count");
    assert_eq!(active_meta_count, 0);
    let rolled_back_row: (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        String,
    ) = connection
        .query_row(
            "SELECT mode, status, cutover_marker, confirmation_token_id, error_code, diff_json
                 FROM config_import_runs
                 WHERE error_code = 'CONFIG_CUTOVER_ROLLED_BACK'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("retained rollback audit row");
    assert_eq!(rolled_back_row.0, "shadow");
    assert_eq!(rolled_back_row.1, "cancelled");
    assert_eq!(rolled_back_row.2, None);
    assert_eq!(rolled_back_row.3, None);
    assert_eq!(rolled_back_row.4, "CONFIG_CUTOVER_ROLLED_BACK");
    let audit: serde_json::Value =
        serde_json::from_str(&rolled_back_row.5).expect("rollback audit JSON");
    assert_eq!(
        audit
            .get("cutoverMarker")
            .and_then(serde_json::Value::as_str),
        Some(first_spec.cutover_marker())
    );
    assert_eq!(
        audit.get("reasonCode").and_then(serde_json::Value::as_str),
        Some("post_cutover_health_failed")
    );
    drop(connection);

    let second_spec = fixture.spec(Uuid::new_v4(), 5_201);
    let second_token_id = ConfirmationTokenId::new();
    let second_token_digest = token_digest(&second_token_id);
    fixture.issue_confirmation(&second_token_id, &second_token_digest, &second_spec);
    fixture
        .store
        .ipc()
        .commit_config_cutover(
            request("cutover-after-clean-rollback", 5_200),
            &second_token_id,
            &second_token_digest,
            second_spec.clone(),
        )
        .expect("new cutover after completed rollback");
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("new production authority")
            .cutover_marker(),
        Some(second_spec.cutover_marker())
    );
}

#[test]
fn first_production_write_makes_rollback_permanently_forward_fix_only() {
    let fixture = Fixture::new();
    let spec = fixture.spec(Uuid::new_v4(), 6_001);
    let token_id = ConfirmationTokenId::new();
    let token_digest_value = token_digest(&token_id);
    fixture.issue_confirmation(&token_id, &token_digest_value, &spec);
    fixture
        .store
        .ipc()
        .commit_config_cutover(
            request("cutover-before-forward-fix", 6_000),
            &token_id,
            &token_digest_value,
            spec.clone(),
        )
        .expect("cutover before first production write");

    let request_id = IpcRequestId::new();
    let payload_digest = digest("forward-fix-boundary");
    fixture
        .store
        .ipc()
        .reserve_request(IpcRequestSpec::new(
            request_id.clone(),
            "settings.update",
            CLIENT_SID_HASH,
            "forward-fix-boundary",
            payload_digest.clone(),
            6_100,
            16_100,
        ))
        .expect("reserve production mutation");
    fixture
        .store
        .ipc()
        .complete_request(
            &request_id,
            &payload_digest,
            IpcCompletionSpec::succeeded(r#"{"schemaVersion":1}"#, None, 6_101),
        )
        .expect("complete production mutation");

    let error = fixture
        .store
        .cutover()
        .rollback_if_clean(ConfigCutoverRollbackSpec::new(
            Uuid::new_v4(),
            spec.cutover_marker(),
            RECOVERY_BUNDLE_DIGEST,
            digest("scheduler-restore-must-not-run"),
            CLIENT_SID_HASH,
            ConfigCutoverRollbackReason::OperatorRequested,
            6_200,
        ))
        .expect_err("dirty production epoch must reject rollback");
    assert_eq!(error.code(), "CONFIG_CUTOVER_FORWARD_FIX_REQUIRED");
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("service authority remains after rejected rollback")
            .recovery_disposition(),
        Some(PersistedCutoverRecoveryDisposition::ForwardFixRequired)
    );
    assert_eq!(
        fixture
            .store
            .cutover()
            .require_legacy_direct_execution()
            .expect_err("legacy execution stays blocked")
            .code(),
        "LEGACY_DIRECT_EXECUTION_BLOCKED"
    );
}

#[test]
fn rollback_write_failure_preserves_clean_service_authority() {
    let fixture = Fixture::new();
    let spec = fixture.spec(Uuid::new_v4(), 7_001);
    let token_id = ConfirmationTokenId::new();
    let token_digest_value = token_digest(&token_id);
    fixture.issue_confirmation(&token_id, &token_digest_value, &spec);
    fixture
        .store
        .ipc()
        .commit_config_cutover(
            request("cutover-before-rollback-failure", 7_000),
            &token_id,
            &token_digest_value,
            spec.clone(),
        )
        .expect("cutover before rollback failure");
    fixture.store.checkpoint_wal().expect("checkpoint cutover");
    let connection = restoreplane_catalog::rusqlite::Connection::open(&fixture.database_path)
        .expect("rollback failure injection");
    connection
        .execute_batch(
            "CREATE TRIGGER fail_cutover_rollback_audit
             BEFORE UPDATE OF status ON config_import_runs
             WHEN NEW.error_code = 'CONFIG_CUTOVER_ROLLED_BACK'
             BEGIN
               SELECT RAISE(ABORT, 'injected rollback audit failure');
             END;",
        )
        .expect("install rollback failure trigger");
    drop(connection);

    let error = fixture
        .store
        .cutover()
        .rollback_if_clean(ConfigCutoverRollbackSpec::new(
            Uuid::new_v4(),
            spec.cutover_marker(),
            RECOVERY_BUNDLE_DIGEST,
            digest("scheduler-restore-receipt"),
            CLIENT_SID_HASH,
            ConfigCutoverRollbackReason::ServiceStartFailed,
            7_100,
        ))
        .expect_err("injected rollback failure");
    assert_eq!(error.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    let authority = fixture
        .store
        .cutover()
        .authority()
        .expect("authority rollback is atomic");
    assert_eq!(
        authority.execution_authority(),
        PersistedExecutionAuthority::Service
    );
    assert_eq!(
        authority.recovery_disposition(),
        Some(PersistedCutoverRecoveryDisposition::RollbackEligible)
    );
}

#[test]
fn any_post_confirmation_write_failure_rolls_back_token_ledger_and_authority() {
    let fixture = Fixture::new();
    let colliding_run_id = Uuid::from_str(fixture.shadow_import_id()).expect("shadow import UUID");
    let spec = fixture.spec(colliding_run_id, 3_001);
    let confirmation_token_id = ConfirmationTokenId::new();
    let confirmation_token_digest = token_digest(&confirmation_token_id);
    fixture.issue_confirmation(&confirmation_token_id, &confirmation_token_digest, &spec);

    let error = fixture
        .store
        .ipc()
        .commit_config_cutover(
            request("cutover-injected-write-failure", 3_000),
            &confirmation_token_id,
            &confirmation_token_digest,
            spec,
        )
        .expect_err("config_import_runs primary-key collision injects a late write failure");
    assert_eq!(error.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation(&confirmation_token_id)
            .expect("rolled-back confirmation")
            .state(),
        "issued"
    );
    assert_eq!(
        fixture
            .store
            .ipc()
            .request_count()
            .expect("rolled-back IPC ledger"),
        0
    );

    let authority = fixture
        .store
        .cutover()
        .authority()
        .expect("fail-closed baseline authority");
    assert_eq!(
        authority.execution_authority(),
        PersistedExecutionAuthority::LegacyDirect
    );
    assert_eq!(
        authority.cutover_mode(),
        PersistedCutoverMode::RehearsalOnly
    );
    assert!(!authority.g1_proven());
    assert_eq!(authority.cutover_marker(), None);
}

#[test]
fn forged_or_torn_authority_projection_is_rejected_fail_closed() {
    let fixture = Fixture::new();
    let spec = fixture.spec(Uuid::new_v4(), 4_001);
    let confirmation_token_id = ConfirmationTokenId::new();
    let confirmation_token_digest = token_digest(&confirmation_token_id);
    fixture.issue_confirmation(&confirmation_token_id, &confirmation_token_digest, &spec);
    fixture
        .store
        .ipc()
        .commit_config_cutover(
            request("cutover-before-tamper", 4_000),
            &confirmation_token_id,
            &confirmation_token_digest,
            spec,
        )
        .expect("valid cutover before tamper");
    fixture
        .store
        .checkpoint_wal()
        .expect("checkpoint valid authority");

    let connection = restoreplane_catalog::rusqlite::Connection::open(&fixture.database_path)
        .expect("tamper handle");
    connection
        .execute(
            "UPDATE app_meta
             SET value_text = '{}'
             WHERE key = 'execution_authority.v1'",
            [],
        )
        .expect("inject torn authority projection");
    drop(connection);

    let error = fixture
        .store
        .cutover()
        .authority()
        .expect_err("forged projection must never enable service authority");
    assert_eq!(error.code(), "CONFIG_CUTOVER_STATE_INVALID");
}

struct Fixture {
    _temp: TempDir,
    database_path: std::path::PathBuf,
    store: OperationalStore,
    shadow_import_id: String,
    relation_digest_blake3: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let database_path = temp.path().join("operational.db");
        let store = OperationalStore::open(database_path.clone()).expect("operational store");
        let report = store
            .shadow_import()
            .import_v1(shadow_model(), 1_000)
            .expect("successful shadow import");
        Self {
            _temp: temp,
            database_path,
            store,
            shadow_import_id: report.import_run_id().to_owned(),
            relation_digest_blake3: report.relation_digest_blake3().to_owned(),
        }
    }

    fn shadow_import_id(&self) -> &str {
        &self.shadow_import_id
    }

    fn spec(&self, cutover_run_id: Uuid, committed_at_ms: i64) -> ConfigCutoverSpec {
        ConfigCutoverSpec::new(
            cutover_run_id,
            Uuid::from_str(self.shadow_import_id()).expect("shadow import UUID"),
            SOURCE_CHECKSUM,
            self.relation_digest_blake3.clone(),
            RECOVERY_BUNDLE_DIGEST,
            LEGACY_FREEZE_DIGEST,
            SERVICE_HEALTH_DIGEST,
            G1EvidenceBindingV1::new(
                G1_SOURCE_REVISION,
                G1_SOURCE_TREE_DIGEST,
                G1_BUILD_ID,
                G1_EVIDENCE_DIGEST,
            ),
            committed_at_ms,
        )
    }

    fn issue_confirmation(
        &self,
        confirmation_token_id: &ConfirmationTokenId,
        confirmation_token_digest: &str,
        spec: &ConfigCutoverSpec,
    ) {
        self.store
            .ipc()
            .issue_confirmation(ConfirmationSpec::new(
                confirmation_token_id.clone(),
                confirmation_token_digest,
                "config_cutover",
                CLIENT_SID_HASH,
                spec.confirmation_resource_type(),
                self.shadow_import_id(),
                spec.cutover_marker(),
                PAYLOAD_DIGEST,
                spec.committed_at_ms() - 100,
                spec.committed_at_ms() + 10_000,
            ))
            .expect("config cutover confirmation");
    }
}

fn request(idempotency_key: &str, accepted_at_ms: i64) -> IpcRequestSpec {
    IpcRequestSpec::new(
        IpcRequestId::new(),
        "config.cutover",
        CLIENT_SID_HASH,
        idempotency_key,
        PAYLOAD_DIGEST,
        accepted_at_ms,
        accepted_at_ms + 10_000,
    )
}

fn confirmation_prepare_request(idempotency_key: &str, accepted_at_ms: i64) -> IpcRequestSpec {
    IpcRequestSpec::new(
        IpcRequestId::new(),
        "confirmation.prepare",
        CLIENT_SID_HASH,
        idempotency_key,
        PREPARE_PAYLOAD_DIGEST,
        accepted_at_ms,
        accepted_at_ms + 20_000,
    )
}

fn shadow_model() -> CanonicalV1ShadowModel {
    let source_fingerprint = digest("source-fingerprint");
    let content_hash = digest("plan-content");
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

fn digest(value: &str) -> String {
    blake3::hash(value.as_bytes()).to_hex().to_string()
}

fn token_digest(confirmation_token_id: &ConfirmationTokenId) -> String {
    digest(confirmation_token_id.as_str())
}
