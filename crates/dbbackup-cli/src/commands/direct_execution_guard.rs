use std::path::{Path, PathBuf};

use dbbackup_catalog::OperationalStore;

const OPERATIONAL_STORE_FILE: &str = "operational-v2.db";

#[derive(Clone, Debug, PartialEq, Eq)]
enum ServiceConfigLookup {
    NotInstalled,
    Installed,
    QueryFailed,
}

pub fn enforce() -> anyhow::Result<()> {
    let lookup = query_installed_service();
    if lookup == ServiceConfigLookup::NotInstalled {
        let program_data = std::env::var_os("ProgramData")
            .ok_or_else(|| anyhow::anyhow!("LEGACY_DIRECT_GUARD_PATH_UNAVAILABLE"))?;
        return enforce_with_service_lookup(
            &PathBuf::from(program_data)
                .join("DbBackup")
                .join(OPERATIONAL_STORE_FILE),
            lookup,
        );
    }
    enforce_with_service_lookup(Path::new(""), lookup)
}

fn enforce_with_service_lookup(
    default_store_path: &Path,
    lookup: ServiceConfigLookup,
) -> anyhow::Result<()> {
    match lookup {
        ServiceConfigLookup::NotInstalled => enforce_at(default_store_path),
        ServiceConfigLookup::QueryFailed => anyhow::bail!(
            "LEGACY_DIRECT_GUARD_SERVICE_QUERY_FAILED: installed service authority could not be verified"
        ),
        ServiceConfigLookup::Installed => anyhow::bail!(
            "LEGACY_DIRECT_EXECUTION_BLOCKED_SERVICE_INSTALLED: remove the DbBackupService entry through the supported uninstall workflow before using legacy direct execution"
        ),
    }
}

#[cfg(windows)]
fn query_installed_service() -> ServiceConfigLookup {
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    const ERROR_SERVICE_DOES_NOT_EXIST: i32 = 1060;

    let manager = match ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
    {
        Ok(manager) => manager,
        Err(_) => return ServiceConfigLookup::QueryFailed,
    };
    let service = match manager.open_service(
        dbbackup_service::scm::SERVICE_NAME,
        ServiceAccess::QUERY_STATUS,
    ) {
        Ok(service) => service,
        Err(windows_service::Error::Winapi(error))
            if error.raw_os_error() == Some(ERROR_SERVICE_DOES_NOT_EXIST) =>
        {
            return ServiceConfigLookup::NotInstalled;
        }
        Err(_) => return ServiceConfigLookup::QueryFailed,
    };
    drop(service);
    ServiceConfigLookup::Installed
}

#[cfg(not(windows))]
fn query_installed_service() -> ServiceConfigLookup {
    ServiceConfigLookup::NotInstalled
}

pub(crate) fn enforce_at(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let store = OperationalStore::open(path.to_path_buf()).map_err(|error| {
        anyhow::anyhow!(
            "{}: operational authority could not be verified",
            error.code()
        )
    })?;
    store
        .cutover()
        .require_legacy_direct_execution()
        .map_err(|error| {
            anyhow::anyhow!(
                "{}: use DbBackup service IPC after configuration cutover",
                error.code()
            )
        })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use dbbackup_catalog::{
        ConfigCutoverSpec, ConfirmationSpec, G1EvidenceBindingV1, IpcRequestSpec,
        StandaloneAuthorityBootstrapSpec,
    };
    use dbbackup_config::{
        CanonicalConnection, CanonicalDestination, CanonicalPlan, CanonicalTarget,
        CanonicalV1ShadowModel,
    };
    use dbbackup_domain::{ConfirmationTokenId, IpcRequestId};
    use tempfile::TempDir;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn missing_or_rehearsal_store_allows_legacy_execution() {
        let temp = TempDir::new().expect("temp directory");
        let path = temp.path().join("operational.db");
        enforce_at(&path).expect("pre-v2 installation has no authority marker");

        let store = OperationalStore::open(path.clone()).expect("empty operational store");
        drop(store);
        enforce_at(&path).expect("empty v2 store remains legacy authority");
    }

    #[test]
    fn production_authority_blocks_legacy_execution() {
        const SOURCE_CHECKSUM: &str =
            "1111111111111111111111111111111111111111111111111111111111111111";
        const CLIENT_SID_HASH: &str =
            "2222222222222222222222222222222222222222222222222222222222222222";
        const PAYLOAD_DIGEST: &str =
            "3333333333333333333333333333333333333333333333333333333333333333";
        const RECOVERY_BUNDLE_DIGEST: &str =
            "4444444444444444444444444444444444444444444444444444444444444444";
        const LEGACY_FREEZE_DIGEST: &str =
            "5555555555555555555555555555555555555555555555555555555555555555";
        const SERVICE_HEALTH_DIGEST: &str =
            "6666666666666666666666666666666666666666666666666666666666666666";
        const G1_SOURCE_REVISION: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        const G1_SOURCE_TREE_DIGEST: &str =
            "7777777777777777777777777777777777777777777777777777777777777777";
        const G1_BUILD_ID: &str =
            "8888888888888888888888888888888888888888888888888888888888888888";
        const G1_EVIDENCE_DIGEST: &str =
            "9999999999999999999999999999999999999999999999999999999999999999";

        let temp = TempDir::new().expect("temp directory");
        let path = temp.path().join("operational.db");
        let store = OperationalStore::open(path.clone()).expect("operational store");
        let shadow = store
            .shadow_import()
            .import_v1(shadow_model(SOURCE_CHECKSUM), 1_000)
            .expect("shadow import");
        let shadow_import_id = Uuid::from_str(shadow.import_run_id()).expect("shadow import UUID");
        let cutover_run_id = Uuid::new_v4();
        let spec = ConfigCutoverSpec::new(
            cutover_run_id,
            shadow_import_id,
            SOURCE_CHECKSUM,
            shadow.relation_digest_blake3(),
            RECOVERY_BUNDLE_DIGEST,
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
        let token_digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
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
                    "cli-direct-guard",
                    PAYLOAD_DIGEST,
                    2_000,
                    12_000,
                ),
                &token_id,
                token_digest,
                spec,
            )
            .expect("production cutover");

        let error = enforce_at(&path).expect_err("production authority must block legacy CLI");
        assert!(
            error
                .to_string()
                .starts_with("LEGACY_DIRECT_EXECUTION_BLOCKED:"),
            "unexpected error: {error:#}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn installed_service_entry_blocks_legacy_for_a_custom_production_store() {
        let temp = TempDir::new().expect("temp directory");
        let data_root = temp.path().join("custom durable data");
        std::fs::create_dir_all(&data_root).expect("data root");
        bootstrap_service_authority(&data_root.join(OPERATIONAL_STORE_FILE));

        let error = enforce_with_service_lookup(
            &temp.path().join("unused-default-store.db"),
            ServiceConfigLookup::Installed,
        )
        .expect_err("custom DataRoot production authority must block legacy execution");

        assert!(
            error
                .to_string()
                .starts_with("LEGACY_DIRECT_EXECUTION_BLOCKED_SERVICE_INSTALLED:"),
            "unexpected error: {error:#}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn installed_service_entry_blocks_legacy_even_with_a_rehearsal_store() {
        let temp = TempDir::new().expect("temp directory");
        let data_root = temp.path().join("custom durable data");
        std::fs::create_dir_all(&data_root).expect("data root");
        let store_path = data_root.join(OPERATIONAL_STORE_FILE);
        drop(OperationalStore::open(store_path).expect("rehearsal operational store"));

        let error = enforce_with_service_lookup(
            &temp.path().join("unused-default-store.db"),
            ServiceConfigLookup::Installed,
        )
        .expect_err("an installed service entry owns execution regardless of persisted phase");

        assert!(
            error
                .to_string()
                .starts_with("LEGACY_DIRECT_EXECUTION_BLOCKED_SERVICE_INSTALLED"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn service_query_failure_fails_closed_but_absent_service_keeps_legacy_use() {
        let temp = TempDir::new().expect("temp directory");
        let missing_default = temp.path().join("missing-default.db");

        enforce_with_service_lookup(&missing_default, ServiceConfigLookup::NotInstalled)
            .expect("service-free legacy installation remains runnable");
        let error = enforce_with_service_lookup(&missing_default, ServiceConfigLookup::QueryFailed)
            .expect_err("SCM query failure must not permit a second execution authority");
        assert!(
            error
                .to_string()
                .starts_with("LEGACY_DIRECT_GUARD_SERVICE_QUERY_FAILED"),
            "unexpected error: {error:#}"
        );
    }

    fn bootstrap_service_authority(path: &Path) {
        let store = OperationalStore::open(path.to_path_buf()).expect("operational store");
        store
            .cutover()
            .bootstrap_standalone_authority(StandaloneAuthorityBootstrapSpec::new(
                Uuid::new_v4(),
                "a".repeat(40),
                "b".repeat(64),
                "c".repeat(64),
                "d".repeat(64),
                1_000,
            ))
            .expect("standalone service authority");
        store.checkpoint_wal().expect("authority checkpoint");
    }

    fn shadow_model(source_checksum: &str) -> CanonicalV1ShadowModel {
        CanonicalV1ShadowModel {
            source_checksum_blake3: source_checksum.to_owned(),
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
                source_fingerprint:
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
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
                content_hash_blake3:
                    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_owned(),
            }],
            secret_refs: Vec::new(),
            secret_bindings: Vec::new(),
        }
    }
}
