#![cfg(windows)]

use std::fs;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;

use restoreplane_catalog::{
    rusqlite::Connection, BackupWorkSpec, ConnectionProfileSpec, DatabaseTargetSpec,
    DestinationSpec, OccurrenceSpec, OperationalStore, PlanDraftSpec, DEFAULT_WORKSPACE_ID,
};
use restoreplane_domain::{
    BackupPlanId, BackupRunId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, OccurrenceIdentity, OccurrenceOrigin, PlanRevisionId, SecretRefId,
    WorkItemId,
};
use restoreplane_pipeline::{decrypt_and_decompress, SignedSetManifestV2};
use restoreplane_secrets::{unwrap_dek_with_kek, Dek, WrapSet};
use restoreplane_service::backup::{
    BackupWorkerTick, NativeBackupExecutor, NativeBackupLimits, ServiceBackupWorker,
};
use restoreplane_service::ipc::WindowsSid;
use restoreplane_service::secrets::{AclPolicy, SecretPurpose, ServiceSecretStore};
use tempfile::TempDir;
use uuid::Uuid;

const SERVICE_SID: &str = "S-1-5-80-1001";

#[test]
fn sqlite_work_item_publishes_recoverable_signed_recovery_point() {
    let temp = TempDir::new().expect("temp directory");
    let source_path = temp.path().join("source.sqlite");
    let source = Connection::open(&source_path).expect("create source SQLite database");
    source
        .execute_batch(
            "CREATE TABLE orders(id INTEGER PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO orders(value) VALUES ('alpha'), ('beta');",
        )
        .expect("seed source rows");
    drop(source);

    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("open operational store");
    let acl = AclPolicy::new(
        WindowsSid::parse(SERVICE_SID).expect("service SID"),
        current_user_sid(),
    )
    .expect("secret ACL");
    let secrets = Arc::new(
        ServiceSecretStore::open(temp.path().join("secrets"), store.secret_refs(), acl)
            .expect("open Secret Store"),
    );
    let destination_root = temp.path().join("destination");
    let spool_root = temp.path().join("spool");
    fs::create_dir_all(&destination_root).expect("destination root");
    fs::create_dir_all(&spool_root).expect("spool root");

    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    let destination_id = DestinationId::new();
    let plan_id = BackupPlanId::new();
    let revision_id = PlanRevisionId::new();
    let occurrence_id =
        OccurrenceIdentity::scheduled(revision_id.clone(), OccurrenceOrigin::Scheduled, 5_000)
            .expect("occurrence identity")
            .deterministic_id(Uuid::NAMESPACE_URL);
    let backup_run_id = BackupRunId::new();
    let work_item_id = WorkItemId::new();
    let configuration = store.configuration();
    configuration
        .insert_connection(ConnectionProfileSpec::file(
            connection_id.clone(),
            "local-sqlite",
            "sqlite",
            source_path.to_string_lossy(),
            1_000,
        ))
        .expect("insert SQLite connection");
    configuration
        .insert_target(DatabaseTargetSpec::new(
            target_id.clone(),
            connection_id,
            "file:source.sqlite",
            "source.sqlite",
            "source.sqlite",
            "source-fingerprint",
            1_010,
        ))
        .expect("insert SQLite target");
    configuration
        .insert_destination(DestinationSpec::local(
            destination_id.clone(),
            "local-vault",
            serde_json::json!({ "path": destination_root }).to_string(),
            1_020,
        ))
        .expect("insert local destination");
    configuration
        .insert_plan_draft(PlanDraftSpec::manual(
            plan_id,
            revision_id.clone(),
            "sqlite-plan",
            1,
            "a".repeat(64),
            1_030,
        ))
        .expect("insert plan");
    configuration
        .add_revision_target(&revision_id, &target_id, 1_031)
        .expect("add target");
    configuration
        .add_revision_destination(
            &revision_id,
            &destination_id,
            DestinationRequirement::Required,
            1_032,
        )
        .expect("add destination");
    configuration
        .publish_revision(&revision_id, 1_040)
        .expect("publish plan");

    let runtime = store.runtime();
    runtime
        .upsert_occurrence(OccurrenceSpec::scheduled(
            occurrence_id.clone(),
            revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            5_000,
            4_900,
        ))
        .expect("insert occurrence");
    runtime
        .enqueue_backup_work(BackupWorkSpec::new(
            backup_run_id,
            work_item_id.clone(),
            occurrence_id,
            revision_id.clone(),
            target_id.clone(),
            "sqlite",
            "source-fingerprint",
            5_100,
        ))
        .expect("enqueue backup work");

    let executor = Arc::new(
        NativeBackupExecutor::with_clock_and_limits(
            store.clone(),
            Arc::clone(&secrets),
            temp.path().to_path_buf(),
            spool_root.clone(),
            "test-instance",
            fixed_now,
            NativeBackupLimits::new(60_000, 1_024 * 1_024, 64 * 1_024, 2, 1),
        )
        .expect("native Backup executor"),
    );
    let worker = ServiceBackupWorker::with_clock(
        store.clone(),
        executor,
        "sqlite-worker",
        "sqlite-worker-boot",
        fixed_now,
    );
    assert_eq!(
        worker.run_once().expect("run one Backup Work Item"),
        BackupWorkerTick::Completed(work_item_id.clone())
    );

    let operational =
        Connection::open(temp.path().join("operational.db")).expect("open operational probe");
    let state: (String, String, Option<String>, Option<i64>) = operational
        .query_row(
            "SELECT work_items.state, backup_runs.state,
                    backup_runs.recovery_point_id, backup_runs.plaintext_bytes
             FROM work_items
             JOIN backup_runs ON backup_runs.id = work_items.backup_run_id
             WHERE work_items.id = ?1",
            [work_item_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query terminal state");
    assert_eq!(state.0, "succeeded");
    assert_eq!(state.1, "succeeded");
    assert!(state.2.is_some());
    assert!(state.3.is_some_and(|bytes| bytes > 0));
    let capacity: (i64, i64, i64, i64) = operational
        .query_row(
            "SELECT
                 COUNT(DISTINCT allocations.id),
                 COUNT(DISTINCT CASE
                     WHEN allocations.released_at_ms IS NOT NULL
                      AND allocations.release_reason = 'completed'
                     THEN allocations.id END),
                 COUNT(DISTINCT reservations.id),
                 COUNT(DISTINCT CASE
                     WHEN reservations.released_at_ms IS NOT NULL
                      AND reservations.state = 'released'
                     THEN reservations.id END)
             FROM work_attempts AS attempts
             LEFT JOIN resource_allocations AS allocations
               ON allocations.work_attempt_id = attempts.id
             LEFT JOIN spool_reservations AS reservations
               ON reservations.work_attempt_id = attempts.id
             WHERE attempts.work_item_id = ?1",
            [work_item_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query released backup capacity");
    assert_eq!(capacity, (3, 3, 1, 1));
    assert!(!spool_root.join(work_item_id.as_str()).exists());

    let manifest_path =
        find_file_named(&destination_root, "set-manifest.json").expect("published Set Manifest");
    let signed: SignedSetManifestV2 =
        serde_json::from_slice(&fs::read(&manifest_path).expect("read Set Manifest"))
            .expect("parse Set Manifest");
    let signing_key_id =
        SecretRefId::from_str(&signed.manifest.signing_key_id).expect("signing key Secret Ref");
    let signing_seed = secrets
        .resolve(&signing_key_id, SecretPurpose::SigningKey)
        .expect("resolve signing seed");
    let signing_seed: [u8; 32] = signing_seed
        .expose()
        .try_into()
        .expect("32-byte signing seed");
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&signing_seed);
    signed
        .verify(&signing_key.verifying_key())
        .expect("verify published signature");
    assert_eq!(
        signed
            .manifest
            .options
            .get("workspaceId")
            .map(String::as_str),
        Some(DEFAULT_WORKSPACE_ID)
    );
    assert_eq!(
        signed
            .manifest
            .options
            .get("workspaceName")
            .map(String::as_str),
        Some("Default Workspace")
    );
    let projected_workspace: String = operational
        .query_row(
            "SELECT workspace_id
             FROM workspace_recovery_points
             WHERE recovery_point_id = ?1",
            [state.2.as_deref().expect("recovery point ID")],
            |row| row.get(0),
        )
        .expect("query recovery point workspace");
    assert_eq!(projected_workspace, DEFAULT_WORKSPACE_ID);
    assert_eq!(
        signed.manifest.options.get("tlsMode").map(String::as_str),
        Some("disabled")
    );
    assert!(signed
        .manifest
        .options
        .keys()
        .any(|key| key.starts_with("dumpFlags.")));

    let encryption_key_id = SecretRefId::from_str(
        signed
            .manifest
            .options
            .get("encryptionKeyRef")
            .expect("encryption key reference"),
    )
    .expect("encryption key Secret Ref");
    let master_key = secrets
        .resolve(&encryption_key_id, SecretPurpose::EncryptionKey)
        .expect("resolve encryption key");
    let master_key: [u8; 32] = master_key
        .expose()
        .try_into()
        .expect("32-byte encryption key");
    let master_key = Dek::from_bytes(master_key);
    let part = signed.manifest.parts.first().expect("database part");
    let wrap_set: WrapSet = serde_json::from_str(
        signed
            .manifest
            .options
            .get(&format!("dekWrap.{}", part.part_id))
            .expect("part DEK wrap"),
    )
    .expect("parse DEK wrap");
    let dek = unwrap_dek_with_kek(&master_key, &wrap_set).expect("unwrap part DEK");
    let ciphertext_path = destination_root.join(&part.object_key);
    let recovered = decrypt_and_decompress(&ciphertext_path, &dek).expect("decrypt backup");
    let recovered_path = temp.path().join("recovered.sqlite");
    fs::write(&recovered_path, recovered).expect("write recovered SQLite database");
    let recovered = Connection::open(recovered_path).expect("open recovered SQLite database");
    let count: i64 = recovered
        .query_row("SELECT COUNT(*) FROM orders", [], |row| row.get(0))
        .expect("query recovered rows");
    assert_eq!(count, 2);

    let blocked_occurrence_id =
        OccurrenceIdentity::scheduled(revision_id.clone(), OccurrenceOrigin::Scheduled, 7_000)
            .expect("blocked occurrence identity")
            .deterministic_id(Uuid::NAMESPACE_URL);
    let blocked_work_item_id = WorkItemId::new();
    runtime
        .upsert_occurrence(OccurrenceSpec::scheduled(
            blocked_occurrence_id.clone(),
            revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            7_000,
            5_900,
        ))
        .expect("insert disk-floor occurrence");
    runtime
        .enqueue_backup_work(BackupWorkSpec::new(
            BackupRunId::new(),
            blocked_work_item_id.clone(),
            blocked_occurrence_id,
            revision_id,
            target_id,
            "sqlite",
            "source-fingerprint",
            5_900,
        ))
        .expect("enqueue disk-floor backup");
    let blocked_executor = Arc::new(
        NativeBackupExecutor::with_clock_and_limits(
            store.clone(),
            secrets,
            temp.path().to_path_buf(),
            spool_root.clone(),
            "disk-floor-instance",
            fixed_now,
            NativeBackupLimits::new(60_000, i64::MAX as u64, 64 * 1_024, 2, 1),
        )
        .expect("disk-floor executor"),
    );
    let blocked_worker = ServiceBackupWorker::with_clock(
        store,
        blocked_executor,
        "disk-floor-worker",
        "disk-floor-worker-boot",
        fixed_now,
    );
    assert_eq!(
        blocked_worker
            .run_once()
            .expect("disk-floor admission tick"),
        BackupWorkerTick::Deferred(blocked_work_item_id.clone())
    );
    let blocked: (String, Option<String>, i64, i64) = operational
        .query_row(
            "SELECT items.state, items.wait_reason,
                    (SELECT COUNT(*) FROM resource_allocations AS allocations
                     JOIN work_attempts AS attempts
                       ON attempts.id = allocations.work_attempt_id
                     WHERE attempts.work_item_id = items.id),
                    (SELECT COUNT(*) FROM spool_reservations AS reservations
                     JOIN work_attempts AS attempts
                       ON attempts.id = reservations.work_attempt_id
                     WHERE attempts.work_item_id = items.id)
             FROM work_items AS items
             WHERE items.id = ?1",
            [blocked_work_item_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query disk-floor admission result");
    assert_eq!(blocked.0, "delayed");
    assert!(
        blocked
            .1
            .as_deref()
            .is_some_and(|reason| reason.starts_with("spool_state:") && reason.ends_with(":full")),
        "unexpected disk-floor wait reason: {:?}",
        blocked.1
    );
    assert_eq!((blocked.2, blocked.3), (0, 0));
}

fn fixed_now() -> i64 {
    6_000
}

fn current_user_sid() -> WindowsSid {
    let output = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .expect("run whoami");
    assert!(output.status.success());
    let line = String::from_utf8(output.stdout).expect("whoami UTF-8");
    let sid = line
        .trim()
        .trim_matches('"')
        .rsplit("\",\"")
        .next()
        .expect("SID column")
        .trim_matches('"');
    WindowsSid::parse(sid).expect("current user SID")
}

fn find_file_named(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    for entry in fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file_named(&path, name) {
                return Some(found);
            }
        } else if path.file_name().and_then(|value| value.to_str()) == Some(name) {
            return Some(path);
        }
    }
    None
}
