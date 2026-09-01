#![cfg(windows)]

use std::fs;
use std::process::Command;

use restoreplane_catalog::OperationalStore;
use restoreplane_service::ipc::WindowsSid;
use restoreplane_service::secrets::{
    audit_acl_sddl, AclPolicy, SecretBuffer, SecretPurpose, ServiceSecretStore,
};
use tempfile::TempDir;

const SERVICE_SID: &str = "S-1-5-80-1001";

#[test]
fn dpapi_store_survives_reopen_rotates_versions_and_never_leaks_plaintext() {
    let fixture = Fixture::new();
    let first_canary = b"db-password-canary-44ca";
    let first = fixture
        .secrets
        .create(
            SecretPurpose::DatabaseAuth,
            SecretBuffer::new(first_canary.to_vec()).expect("secret buffer"),
            1_000,
        )
        .expect("create secret");
    assert_eq!(first.version(), 1);
    assert_eq!(first.state(), "active");
    assert_eq!(first.provider(), "windows_dpapi");
    assert_eq!(first.fingerprint().len(), 64);

    let resolved = fixture
        .secrets
        .resolve(first.id(), SecretPurpose::DatabaseAuth)
        .expect("resolve first secret");
    assert_eq!(resolved.expose(), first_canary);
    drop(resolved);

    fixture.assert_canary_absent(first_canary);
    let metadata_json = serde_json::to_vec(&first).expect("serialize public metadata");
    assert!(!contains(&metadata_json, first_canary));

    let reopened = ServiceSecretStore::open(
        fixture.secret_root.clone(),
        fixture.store.secret_refs(),
        fixture.acl.clone(),
    )
    .expect("reopen service secret store");
    assert_eq!(
        reopened
            .resolve(first.id(), SecretPurpose::DatabaseAuth)
            .expect("unattended decrypt after reopen")
            .expose(),
        first_canary
    );

    let second_canary = b"db-password-canary-rotated-a19e";
    let second = reopened
        .rotate(
            first.id(),
            SecretPurpose::DatabaseAuth,
            SecretBuffer::new(second_canary.to_vec()).expect("rotated buffer"),
            2_000,
        )
        .expect("rotate secret");
    assert_eq!(second.version(), 2);
    assert_eq!(second.provider_locator(), first.provider_locator());
    assert_ne!(second.fingerprint(), first.fingerprint());
    assert_eq!(
        fixture
            .store
            .secret_refs()
            .get(first.id())
            .expect("old metadata")
            .state(),
        "retired"
    );
    assert_eq!(
        reopened
            .resolve(first.id(), SecretPurpose::DatabaseAuth)
            .expect("queued revision may resolve retired version")
            .expose(),
        first_canary
    );
    assert_eq!(
        reopened
            .resolve(second.id(), SecretPurpose::DatabaseAuth)
            .expect("new revision resolves new version")
            .expose(),
        second_canary
    );

    let mismatch = reopened
        .resolve(second.id(), SecretPurpose::RestoreReader)
        .err()
        .expect("role mismatch is denied");
    assert_eq!(mismatch.code(), "SECRET_STORE_PURPOSE_MISMATCH");
    reopened
        .revoke(first.id(), 3_000)
        .expect("revoke old version");
    let revoked = reopened
        .resolve(first.id(), SecretPurpose::DatabaseAuth)
        .err()
        .expect("revoked secret is denied");
    assert_eq!(revoked.code(), "SECRET_STORE_REVOKED");
    fixture.assert_canary_absent(second_canary);
}

#[test]
fn secret_store_acl_is_protected_and_has_no_everyone_or_authenticated_user_ace() {
    let fixture = Fixture::new();
    let metadata = fixture
        .secrets
        .create(
            SecretPurpose::BackupWriter,
            SecretBuffer::new(b"writer-canary".to_vec()).expect("secret buffer"),
            1_000,
        )
        .expect("create secret");

    for path in [
        fixture.secret_root.clone(),
        fixture.secrets.entropy_path().to_path_buf(),
        fixture.secrets.blob_path(&metadata).expect("blob path"),
    ] {
        let sddl = audit_acl_sddl(&path).expect("read protected DACL");
        assert!(sddl.contains("D:P"), "DACL must be protected: {sddl}");
        assert!(sddl.contains(SERVICE_SID), "service SID missing: {sddl}");
        assert!(
            sddl.contains(fixture.owner_sid.as_str()) || sddl.contains(";;;LA)"),
            "owner SID missing: {sddl}"
        );
        assert!(!sddl.contains(";;;WD)"), "Everyone ACE present: {sddl}");
        assert!(
            !sddl.contains(";;;AU)"),
            "Authenticated Users ACE present: {sddl}"
        );
    }
}

#[test]
fn service_key_material_is_created_once_and_reopened_by_purpose() {
    let fixture = Fixture::new();
    let (first_ref, first_key) = fixture
        .secrets
        .load_or_create_service_key(SecretPurpose::SigningKey, 1_000)
        .expect("create signing key");
    assert_eq!(first_key.expose().len(), 32);

    let reopened = ServiceSecretStore::open(
        fixture.secret_root.clone(),
        fixture.store.secret_refs(),
        fixture.acl.clone(),
    )
    .expect("reopen service secret store");
    let (second_ref, second_key) = reopened
        .load_or_create_service_key(SecretPurpose::SigningKey, 2_000)
        .expect("reuse signing key");

    assert_eq!(second_ref.id(), first_ref.id());
    assert_eq!(second_key.expose(), first_key.expose());
    assert_eq!(
        fixture
            .store
            .secret_refs()
            .active_for_purpose(SecretPurpose::SigningKey.as_str())
            .expect("active key metadata")
            .expect("active signing key")
            .id(),
        first_ref.id()
    );
}

struct Fixture {
    _temp: TempDir,
    operational_path: std::path::PathBuf,
    secret_root: std::path::PathBuf,
    store: OperationalStore,
    secrets: ServiceSecretStore,
    acl: AclPolicy,
    owner_sid: WindowsSid,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let operational_path = temp.path().join("operational.db");
        let secret_root = temp.path().join("secret-store");
        let store =
            OperationalStore::open(operational_path.clone()).expect("open operational store");
        let owner_sid = current_user_sid();
        let acl = AclPolicy::new(
            WindowsSid::parse(SERVICE_SID).expect("service SID"),
            owner_sid.clone(),
        )
        .expect("ACL policy");
        let secrets =
            ServiceSecretStore::open(secret_root.clone(), store.secret_refs(), acl.clone())
                .expect("open service secret store");
        Self {
            _temp: temp,
            operational_path,
            secret_root,
            store,
            secrets,
            acl,
            owner_sid,
        }
    }

    fn assert_canary_absent(&self, canary: &[u8]) {
        for path in self.all_files() {
            let bytes = fs::read(&path).expect("read persisted file");
            assert!(
                !contains(&bytes, canary),
                "plaintext canary leaked into {}",
                path.display()
            );
        }
    }

    fn all_files(&self) -> Vec<std::path::PathBuf> {
        let mut files = ["", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| {
                std::path::PathBuf::from(format!("{}{}", self.operational_path.display(), suffix))
            })
            .filter(|path| path.exists())
            .collect::<Vec<_>>();
        collect_files(&self.secret_root, &mut files);
        files
    }
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

fn collect_files(root: &std::path::Path, output: &mut Vec<std::path::PathBuf>) {
    if !root.exists() {
        return;
    }
    for entry in fs::read_dir(root).expect("read secret root") {
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
            .any(|candidate| candidate == needle)
}
