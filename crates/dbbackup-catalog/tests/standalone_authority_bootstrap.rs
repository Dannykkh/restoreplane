use dbbackup_catalog::{
    OperationalStore, PersistedAuthorityOrigin, PersistedCutoverMode,
    PersistedCutoverRecoveryDisposition, PersistedExecutionAuthority, RepositoryError,
    StandaloneAuthorityBootstrapSpec, WorkspaceSpec,
};
use dbbackup_domain::WorkspaceId;
use tempfile::TempDir;
use uuid::Uuid;

const SOURCE_REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const PACKAGE_MANIFEST_DIGEST: &str =
    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const BUILD_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
const SERVICE_BINARY_SHA256: &str =
    "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

#[test]
fn pristine_store_bootstraps_one_persisted_service_authority_and_restarts() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("standalone.db");
    let store = OperationalStore::open(database_path.clone()).expect("pristine store");
    let spec = spec(1_000);

    let result = store
        .cutover()
        .bootstrap_standalone_authority(spec)
        .expect("standalone authority bootstrap");
    assert!(result.created());
    assert!(result
        .authority_marker()
        .starts_with("standalone-bootstrap-v1:"));

    let authority = store.cutover().authority().expect("persisted authority");
    assert_eq!(
        authority.execution_authority(),
        PersistedExecutionAuthority::Service
    );
    assert_eq!(authority.cutover_mode(), PersistedCutoverMode::Production);
    assert_eq!(
        authority.origin(),
        PersistedAuthorityOrigin::StandaloneInstall
    );
    assert!(authority.g1_proven());
    assert_eq!(
        authority.recovery_disposition(),
        Some(PersistedCutoverRecoveryDisposition::ForwardFixRequired)
    );
    assert_eq!(authority.cutover_marker(), Some(result.authority_marker()));
    store.checkpoint_wal().expect("checkpoint authority");
    drop(store);

    let reopened = OperationalStore::open(database_path).expect("reopen store");
    let restarted = reopened.cutover().authority().expect("restart authority");
    assert_eq!(restarted, authority);
}

#[test]
fn any_operator_state_blocks_standalone_bootstrap_without_mutating_authority() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("non-pristine.db")).expect("operational store");
    store
        .workspace()
        .create(WorkspaceSpec::new(
            WorkspaceId::new(),
            "Existing Workspace",
            900,
        ))
        .expect("seed operator-owned state");

    let error = store
        .cutover()
        .bootstrap_standalone_authority(spec(1_000))
        .expect_err("non-pristine store must fail closed");
    assert!(matches!(
        error,
        RepositoryError::StandaloneBootstrapPreconditionFailed
    ));

    let authority = store.cutover().authority().expect("rehearsal authority");
    assert_eq!(authority.origin(), PersistedAuthorityOrigin::Rehearsal);
    assert_eq!(
        authority.execution_authority(),
        PersistedExecutionAuthority::LegacyDirect
    );
    assert_eq!(authority.cutover_marker(), None);
}

fn spec(committed_at_ms: i64) -> StandaloneAuthorityBootstrapSpec {
    StandaloneAuthorityBootstrapSpec::new(
        Uuid::parse_str("01234567-89ab-4cde-8fab-0123456789ab").expect("installation UUID"),
        SOURCE_REVISION,
        PACKAGE_MANIFEST_DIGEST,
        BUILD_ID,
        SERVICE_BINARY_SHA256,
        committed_at_ms,
    )
}
