use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use restoreplane_catalog::{
    ConfigCutoverSpec, ConfirmationSpec, G1EvidenceBindingV1, IpcCompletionSpec, IpcRequestSpec,
    OperationalStore, PersistedCutoverRecoveryDisposition, PersistedExecutionAuthority,
};
use restoreplane_config::{
    CanonicalConnection, CanonicalDestination, CanonicalPlan, CanonicalTarget,
    CanonicalV1ShadowModel,
};
use restoreplane_domain::{ConfirmationTokenId, IpcRequestId};
use restoreplane_service::ipc::{TokenIdentity, WindowsSid};
use restoreplane_service::recovery::{
    LegacySchedulerControlError, LegacySchedulerControllerV1, OfflineCutoverRecoveryRequestV1,
    OfflineCutoverRecoveryV1, WindowsLegacySchedulerControllerV1,
};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

const SOURCE_CHECKSUM: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const CLIENT_SID_HASH: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const PAYLOAD_DIGEST: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const LEGACY_FREEZE_DIGEST: &str =
    "4444444444444444444444444444444444444444444444444444444444444444";
const SERVICE_HEALTH_DIGEST: &str =
    "5555555555555555555555555555555555555555555555555555555555555555";
const G1_SOURCE_REVISION: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const G1_SOURCE_TREE_DIGEST: &str =
    "6666666666666666666666666666666666666666666666666666666666666666";
const G1_BUILD_ID: &str = "7777777777777777777777777777777777777777777777777777777777777777";
const G1_EVIDENCE_DIGEST: &str = "8888888888888888888888888888888888888888888888888888888888888888";
const OWNER_SID: &str = "S-1-5-21-1000-1001-1002-1003";
const OTHER_SID: &str = "S-1-5-21-2000-2001-2002-2003";
const LOGON_SID: &str = "S-1-5-5-1-2";
const TASK_A: &str = r"\RestorePlane\orders-nightly";
const TASK_B: &str = r"\RestorePlane\billing-nightly";

#[test]
fn clean_offline_recovery_commits_legacy_authority_before_task_restore_and_resumes() {
    let fixture = Fixture::new();
    let controller = Arc::new(FakeController::new(
        fixture.store.clone(),
        [(TASK_A, false), (TASK_B, false)],
        Some(TASK_A),
    ));
    let recovery = OfflineCutoverRecoveryV1::open(
        format!("offline-recovery-{}", Uuid::new_v4()),
        fixture.database_path.clone(),
        fixture.root.path().to_path_buf(),
        sid(OWNER_SID),
        Arc::clone(&controller) as Arc<dyn LegacySchedulerControllerV1>,
    )
    .expect("offline recovery coordinator");
    let request = fixture.request();

    let error = recovery
        .execute(&owner_identity(), request.clone())
        .expect_err("injected second task restore failure");
    assert_eq!(error.code(), "CONFIG_CUTOVER_SCHEDULER_RESTORE_INCOMPLETE");
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("legacy authority after committed rollback")
            .execution_authority(),
        PersistedExecutionAuthority::LegacyDirect
    );
    assert_eq!(controller.enabled(TASK_A), Some(false));
    assert_eq!(controller.enabled(TASK_B), Some(true));
    assert!(
        controller
            .authority_observations()
            .iter()
            .all(|authority| *authority == PersistedExecutionAuthority::LegacyDirect),
        "scheduler mutation must only happen after the DB authority commit"
    );

    let resumed = recovery
        .execute(&owner_identity(), request)
        .expect("same rollback request resumes task restoration");
    assert!(!resumed.rollback_created());
    assert!(resumed.resumed());
    assert_eq!(resumed.task_count(), 2);
    assert_eq!(controller.enabled(TASK_A), Some(true));
    assert_eq!(controller.enabled(TASK_B), Some(true));
}

#[test]
fn offline_recovery_rejects_wrong_owner_confirmation_and_live_service_singleton() {
    let fixture = Fixture::new();
    let controller = Arc::new(FakeController::new(
        fixture.store.clone(),
        [(TASK_A, false), (TASK_B, false)],
        None,
    ));
    let instance_key = format!("offline-recovery-lock-{}", Uuid::new_v4());
    let first = restoreplane_service::host::MaintenanceAuthority::acquire(&instance_key)
        .expect("first singleton owner");
    let error = OfflineCutoverRecoveryV1::open(
        instance_key,
        fixture.database_path.clone(),
        fixture.root.path().to_path_buf(),
        sid(OWNER_SID),
        Arc::clone(&controller) as Arc<dyn LegacySchedulerControllerV1>,
    )
    .expect_err("a live service/sibling recovery owner blocks offline recovery");
    assert_eq!(error.code(), "SERVICE_SINGLETON_ALREADY_RUNNING");
    drop(first);

    let recovery = OfflineCutoverRecoveryV1::open(
        format!("offline-recovery-auth-{}", Uuid::new_v4()),
        fixture.database_path.clone(),
        fixture.root.path().to_path_buf(),
        sid(OWNER_SID),
        Arc::clone(&controller) as Arc<dyn LegacySchedulerControllerV1>,
    )
    .expect("recovery after singleton release");
    let unauthorized = recovery
        .execute(&other_identity(), fixture.request())
        .expect_err("only the elevated local owner may recover");
    assert_eq!(unauthorized.code(), "CONFIG_CUTOVER_RECOVERY_UNAUTHORIZED");

    let wrong_confirmation = fixture
        .request()
        .with_confirmed_cutover_marker("config-cutover-v1:wrong");
    let confirmation_error = recovery
        .execute(&owner_identity(), wrong_confirmation)
        .expect_err("typed marker must match exactly");
    assert_eq!(
        confirmation_error.code(),
        "CONFIG_CUTOVER_RECOVERY_CONFIRMATION_INVALID"
    );
    let path_escape = OfflineCutoverRecoveryRequestV1::new(
        fixture.rollback_id,
        fixture.cutover_marker.clone(),
        "../bundle-manifest.json",
        restoreplane_catalog::ConfigCutoverRollbackReason::PostCutoverHealthFailed,
        2_100,
        fixture.cutover_marker.clone(),
    );
    let evidence_error = recovery
        .execute(&owner_identity(), path_escape)
        .expect_err("recovery evidence cannot escape the protected root");
    assert_eq!(
        evidence_error.code(),
        "CONFIG_CUTOVER_RECOVERY_EVIDENCE_INVALID"
    );
    assert_eq!(controller.mutation_count(), 0);
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("production authority remains")
            .execution_authority(),
        PersistedExecutionAuthority::Service
    );
}

#[test]
fn first_production_write_refuses_offline_rollback_before_scheduler_mutation() {
    let fixture = Fixture::new();
    fixture.mark_forward_fix_required();
    let controller = Arc::new(FakeController::new(
        fixture.store.clone(),
        [(TASK_A, false), (TASK_B, false)],
        None,
    ));
    let recovery = OfflineCutoverRecoveryV1::open(
        format!("offline-recovery-forward-fix-{}", Uuid::new_v4()),
        fixture.database_path.clone(),
        fixture.root.path().to_path_buf(),
        sid(OWNER_SID),
        Arc::clone(&controller) as Arc<dyn LegacySchedulerControllerV1>,
    )
    .expect("offline recovery coordinator");

    let error = recovery
        .execute(&owner_identity(), fixture.request())
        .expect_err("dirty production epoch is forward-fix only");
    assert_eq!(error.code(), "CONFIG_CUTOVER_FORWARD_FIX_REQUIRED");
    assert_eq!(controller.mutation_count(), 0);
    let authority = fixture
        .store
        .cutover()
        .authority()
        .expect("service authority remains");
    assert_eq!(
        authority.execution_authority(),
        PersistedExecutionAuthority::Service
    );
    assert_eq!(
        authority.recovery_disposition(),
        Some(PersistedCutoverRecoveryDisposition::ForwardFixRequired)
    );
}

#[test]
fn scheduler_definition_drift_rejects_recovery_before_catalog_or_task_mutation() {
    let fixture = Fixture::new();
    let controller = Arc::new(FakeController::new(
        fixture.store.clone(),
        [(TASK_A, false), (TASK_B, false)],
        None,
    ));
    controller.invalidate_definition(TASK_A);
    let recovery = OfflineCutoverRecoveryV1::open(
        format!("offline-recovery-task-drift-{}", Uuid::new_v4()),
        fixture.database_path.clone(),
        fixture.root.path().to_path_buf(),
        sid(OWNER_SID),
        Arc::clone(&controller) as Arc<dyn LegacySchedulerControllerV1>,
    )
    .expect("offline recovery coordinator");

    let error = recovery
        .execute(&owner_identity(), fixture.request())
        .expect_err("scheduler definition drift is not an enablement rollback");
    assert_eq!(error.code(), "CONFIG_CUTOVER_RECOVERY_EVIDENCE_INVALID");
    assert_eq!(controller.mutation_count(), 0);
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("production authority remains")
            .execution_authority(),
        PersistedExecutionAuthority::Service
    );
}

#[test]
fn scheduler_definition_drift_after_catalog_commit_never_enables_the_task() {
    let fixture = Fixture::new();
    let controller = Arc::new(FakeController::new(
        fixture.store.clone(),
        [(TASK_A, false), (TASK_B, false)],
        None,
    ));
    controller.drift_after_first_definition_check(TASK_B);
    let recovery = OfflineCutoverRecoveryV1::open(
        format!("offline-recovery-late-task-drift-{}", Uuid::new_v4()),
        fixture.database_path.clone(),
        fixture.root.path().to_path_buf(),
        sid(OWNER_SID),
        Arc::clone(&controller) as Arc<dyn LegacySchedulerControllerV1>,
    )
    .expect("offline recovery coordinator");

    let error = recovery
        .execute(&owner_identity(), fixture.request())
        .expect_err("definition drift at the restore boundary must fail closed");
    assert_eq!(error.code(), "CONFIG_CUTOVER_SCHEDULER_RESTORE_INCOMPLETE");
    assert_eq!(controller.mutation_count(), 0);
    assert_eq!(controller.enabled(TASK_A), Some(false));
    assert_eq!(controller.enabled(TASK_B), Some(false));
    assert_eq!(
        fixture
            .store
            .cutover()
            .authority()
            .expect("catalog rollback is already committed")
            .execution_authority(),
        PersistedExecutionAuthority::LegacyDirect
    );
}

#[cfg(windows)]
#[test]
#[ignore = "requires a managed Windows Task Scheduler fixture"]
fn real_windows_scheduler_controller_restores_a_throwaway_restoreplane_task() {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let task_name = format!(r"\RestorePlane\RecoveryFixture-{}", Uuid::new_v4().simple());
    let mut guard = ScheduledTaskGuard::new(task_name.clone());
    let created = Command::new("schtasks.exe")
        .args([
            "/Create",
            "/TN",
            &task_name,
            "/SC",
            "MONTHLY",
            "/D",
            "31",
            "/M",
            "DEC",
            "/ST",
            "23:58",
            "/TR",
            "cmd.exe /d /c exit 0",
            "/F",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("create throwaway scheduled task");
    assert!(
        created.status.success(),
        "throwaway task creation failed with status {:?}",
        created.status.code()
    );
    guard.created = true;

    let original_xml = query_task_xml(&task_name);
    let original_digest = digest(original_xml.as_bytes());
    let disabled = Command::new("schtasks.exe")
        .args(["/Change", "/TN", &task_name, "/DISABLE"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("disable throwaway task");
    assert!(disabled.status.success());

    let controller = WindowsLegacySchedulerControllerV1;
    assert_eq!(controller.task_enabled(&task_name), Some(false));
    assert!(controller.task_definition_matches(&task_name, &original_xml, &original_digest));
    controller
        .set_task_enabled(&task_name, true)
        .expect("restore throwaway task enablement");
    assert_eq!(controller.task_enabled(&task_name), Some(true));
    assert!(controller.task_definition_matches(&task_name, &original_xml, &original_digest));
}

struct Fixture {
    root: TempDir,
    database_path: std::path::PathBuf,
    store: OperationalStore,
    cutover_marker: String,
    rollback_id: Uuid,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().expect("temp directory");
        let database_path = root.path().join("operational.db");
        let store = OperationalStore::open(database_path.clone()).expect("operational store");
        let recovery_bundle_digest = write_recovery_bundle(root.path());
        let shadow = store
            .shadow_import()
            .import_v1(shadow_model(), 1_000)
            .expect("shadow import");
        let spec = ConfigCutoverSpec::new(
            Uuid::new_v4(),
            Uuid::from_str(shadow.import_run_id()).expect("shadow import UUID"),
            SOURCE_CHECKSUM,
            shadow.relation_digest_blake3(),
            recovery_bundle_digest,
            LEGACY_FREEZE_DIGEST,
            SERVICE_HEALTH_DIGEST,
            G1EvidenceBindingV1::new(
                G1_SOURCE_REVISION,
                G1_SOURCE_TREE_DIGEST,
                G1_BUILD_ID,
                G1_EVIDENCE_DIGEST,
            ),
            2_001,
        );
        let token_id = ConfirmationTokenId::new();
        let token_digest = "9999999999999999999999999999999999999999999999999999999999999999";
        store
            .ipc()
            .issue_confirmation(ConfirmationSpec::new(
                token_id.clone(),
                token_digest,
                "config_cutover",
                CLIENT_SID_HASH,
                spec.confirmation_resource_type(),
                shadow.import_run_id(),
                spec.cutover_marker(),
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
                    CLIENT_SID_HASH,
                    "offline-recovery-fixture",
                    PAYLOAD_DIGEST,
                    2_000,
                    12_000,
                ),
                &token_id,
                token_digest,
                spec.clone(),
            )
            .expect("production cutover");
        Self {
            root,
            database_path,
            store,
            cutover_marker: spec.cutover_marker().to_owned(),
            rollback_id: Uuid::new_v4(),
        }
    }

    fn request(&self) -> OfflineCutoverRecoveryRequestV1 {
        OfflineCutoverRecoveryRequestV1::new(
            self.rollback_id,
            self.cutover_marker.clone(),
            "bundle-manifest.json",
            restoreplane_catalog::ConfigCutoverRollbackReason::PostCutoverHealthFailed,
            2_100,
            self.cutover_marker.clone(),
        )
    }

    fn mark_forward_fix_required(&self) {
        let request_id = IpcRequestId::new();
        let payload_digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        self.store
            .ipc()
            .reserve_request(IpcRequestSpec::new(
                request_id.clone(),
                "settings.update",
                CLIENT_SID_HASH,
                "offline-recovery-forward-fix",
                payload_digest,
                2_050,
                12_050,
            ))
            .expect("first production request");
        self.store
            .ipc()
            .complete_request(
                &request_id,
                payload_digest,
                IpcCompletionSpec::succeeded(r#"{"schemaVersion":1}"#, None, 2_051),
            )
            .expect("first production mutation");
    }
}

struct FakeController {
    store: OperationalStore,
    states: Mutex<BTreeMap<String, bool>>,
    fail_once: Mutex<Option<String>>,
    invalid_definitions: Mutex<BTreeSet<String>>,
    drift_after_first_definition_check: Mutex<BTreeSet<String>>,
    definition_checks: Mutex<BTreeMap<String, usize>>,
    authority_observations: Mutex<Vec<PersistedExecutionAuthority>>,
    mutation_count: Mutex<usize>,
}

impl FakeController {
    fn new<const N: usize>(
        store: OperationalStore,
        states: [(&str, bool); N],
        fail_once: Option<&str>,
    ) -> Self {
        Self {
            store,
            states: Mutex::new(
                states
                    .into_iter()
                    .map(|(name, enabled)| (name.to_owned(), enabled))
                    .collect(),
            ),
            fail_once: Mutex::new(fail_once.map(str::to_owned)),
            invalid_definitions: Mutex::new(BTreeSet::new()),
            drift_after_first_definition_check: Mutex::new(BTreeSet::new()),
            definition_checks: Mutex::new(BTreeMap::new()),
            authority_observations: Mutex::new(Vec::new()),
            mutation_count: Mutex::new(0),
        }
    }

    fn enabled(&self, task_name: &str) -> Option<bool> {
        self.states.lock().expect("states").get(task_name).copied()
    }

    fn authority_observations(&self) -> Vec<PersistedExecutionAuthority> {
        self.authority_observations
            .lock()
            .expect("observations")
            .clone()
    }

    fn mutation_count(&self) -> usize {
        *self.mutation_count.lock().expect("mutation count")
    }

    fn invalidate_definition(&self, task_name: &str) {
        self.invalid_definitions
            .lock()
            .expect("invalid definitions")
            .insert(task_name.to_owned());
    }

    fn drift_after_first_definition_check(&self, task_name: &str) {
        self.drift_after_first_definition_check
            .lock()
            .expect("late definition drift")
            .insert(task_name.to_owned());
    }
}

impl LegacySchedulerControllerV1 for FakeController {
    fn task_enabled(&self, task_name: &str) -> Option<bool> {
        self.enabled(task_name)
    }

    fn task_definition_matches(
        &self,
        task_name: &str,
        _task_xml: &str,
        _xml_digest_blake3: &str,
    ) -> bool {
        let exists = self.states.lock().expect("states").contains_key(task_name);
        let invalid = self
            .invalid_definitions
            .lock()
            .expect("invalid definitions")
            .contains(task_name);
        let drifts_late = self
            .drift_after_first_definition_check
            .lock()
            .expect("late definition drift")
            .contains(task_name);
        let check_count = {
            let mut checks = self.definition_checks.lock().expect("definition checks");
            let count = checks.entry(task_name.to_owned()).or_default();
            *count += 1;
            *count
        };
        exists && !invalid && !(drifts_late && check_count > 1)
    }

    fn set_task_enabled(
        &self,
        task_name: &str,
        enabled: bool,
    ) -> Result<(), LegacySchedulerControlError> {
        self.authority_observations
            .lock()
            .expect("observations")
            .push(
                self.store
                    .cutover()
                    .authority()
                    .expect("authority during scheduler restore")
                    .execution_authority(),
            );
        *self.mutation_count.lock().expect("mutation count") += 1;
        let mut fail_once = self.fail_once.lock().expect("fail once");
        if fail_once.as_deref() == Some(task_name) {
            *fail_once = None;
            return Err(LegacySchedulerControlError::CommandFailed);
        }
        self.states
            .lock()
            .expect("states")
            .insert(task_name.to_owned(), enabled);
        Ok(())
    }
}

fn write_recovery_bundle(root: &std::path::Path) -> String {
    let bundle = root.join("bundle");
    fs::create_dir(&bundle).expect("bundle directory");
    let task_a_xml = concat!(
        "<Task>",
        "<Triggers><CalendarTrigger><Enabled>true</Enabled></CalendarTrigger></Triggers>",
        "<Settings><Enabled>true</Enabled></Settings>",
        "</Task>"
    );
    let task_b_xml = task_a_xml;
    let inventory = serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "tasks": [
            {
                "taskName": TASK_A,
                "enabled": true,
                "taskXml": task_a_xml,
                "xmlDigestBlake3": digest(task_a_xml.as_bytes())
            },
            {
                "taskName": TASK_B,
                "enabled": true,
                "taskXml": task_b_xml,
                "xmlDigestBlake3": digest(task_b_xml.as_bytes())
            }
        ]
    }))
    .expect("scheduler inventory");
    let files = [
        ("legacy_catalog", "catalog.db", b"catalog".as_slice()),
        ("legacy_keystore", "keystore.bin", b"keystore".as_slice()),
        ("legacy_toml", "restoreplane.toml", b"[config]".as_slice()),
        (
            "scheduler_inventory",
            "scheduler-inventory.json",
            inventory.as_slice(),
        ),
    ];
    let items = files
        .iter()
        .map(|(kind, relative_path, bytes)| {
            fs::write(bundle.join(relative_path), bytes).expect("bundle item");
            json!({
                "kind": kind,
                "relativePath": relative_path,
                "sizeBytes": bytes.len(),
                "digestBlake3": digest(bytes)
            })
        })
        .collect::<Vec<_>>();
    let manifest = serde_json::to_vec(&json!({
        "schemaVersion": 1,
        "sourceChecksumBlake3": SOURCE_CHECKSUM,
        "createdAtMs": 900,
        "bundleDirectory": "bundle",
        "items": items
    }))
    .expect("bundle manifest");
    fs::write(root.join("bundle-manifest.json"), &manifest).expect("bundle manifest");
    digest(&manifest)
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
            source_fingerprint: digest(b"source-fingerprint"),
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
            content_hash_blake3: digest(b"plan-content"),
        }],
        secret_refs: Vec::new(),
        secret_bindings: Vec::new(),
    }
}

fn owner_identity() -> TokenIdentity {
    TokenIdentity::new(sid(OWNER_SID), sid(LOGON_SID), 1, true, true)
}

fn other_identity() -> TokenIdentity {
    TokenIdentity::new(sid(OTHER_SID), sid(LOGON_SID), 1, true, true)
}

fn sid(value: &str) -> WindowsSid {
    WindowsSid::parse(value).expect("canonical SID")
}

fn digest(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(windows)]
struct ScheduledTaskGuard {
    task_name: String,
    created: bool,
}

#[cfg(windows)]
impl ScheduledTaskGuard {
    fn new(task_name: String) -> Self {
        Self {
            task_name,
            created: false,
        }
    }
}

#[cfg(windows)]
impl Drop for ScheduledTaskGuard {
    fn drop(&mut self) {
        use std::os::windows::process::CommandExt;
        use std::process::Command;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        if self.created {
            let _ = Command::new("schtasks.exe")
                .args(["/Delete", "/TN", &self.task_name, "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }
}

#[cfg(windows)]
fn query_task_xml(task_name: &str) -> String {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let output = Command::new("schtasks.exe")
        .args(["/Query", "/TN", task_name, "/XML"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("query throwaway task XML");
    assert!(output.status.success());
    decode_command_text(&output.stdout).expect("task XML encoding")
}

#[cfg(windows)]
fn decode_command_text(bytes: &[u8]) -> Option<String> {
    if let Some(body) = bytes.strip_prefix(&[0xff, 0xfe]) {
        if body.len() % 2 != 0 {
            return None;
        }
        let units = body
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).ok();
    }
    if let Some(body) = bytes.strip_prefix(&[0xfe, 0xff]) {
        if body.len() % 2 != 0 {
            return None;
        }
        let units = body
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).ok();
    }
    String::from_utf8(bytes.to_vec()).ok()
}
