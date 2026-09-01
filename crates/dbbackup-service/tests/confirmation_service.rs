use std::fs;

use dbbackup_application::ConfirmationAction;
use dbbackup_catalog::OperationalStore;
use dbbackup_service::ipc::{AuthorizationPolicy, ConfirmationService, TokenIdentity, WindowsSid};
use serde_json::json;
use tempfile::TempDir;

const SERVICE_SID: &str = "S-1-5-80-1001";
const OWNER_SID: &str = "S-1-5-21-100-200-300-1001";
const OWNER_LOGON_SID: &str = "S-1-5-5-10-20";
const ADMIN_SID: &str = "S-1-5-21-100-200-300-500";

#[test]
fn all_destructive_actions_store_only_digest_and_validate_exact_binding() {
    let fixture = Fixture::new();
    let principal = fixture.owner();

    for (index, action) in ConfirmationAction::ALL.into_iter().enumerate() {
        let resource_id = format!("resource-{index}");
        let payload = json!({"operation": action.as_str(), "sequence": index as u64});
        if action == ConfirmationAction::RetentionDelete {
            let error = match fixture.service.prepare(
                &principal,
                action,
                "test_resource",
                &resource_id,
                "fingerprint-v1",
                &payload,
                1_000,
                60_000,
            ) {
                Err(error) => error,
                Ok(_) => panic!("retention confirmation requires authoritative typed IPC"),
            };
            assert_eq!(error.code(), "CONFIRMATION_INPUT_INVALID");
            continue;
        }
        let issued = fixture
            .service
            .prepare(
                &principal,
                action,
                "test_resource",
                &resource_id,
                "fingerprint-v1",
                &payload,
                1_000,
                60_000,
            )
            .expect("issue confirmation");
        assert_eq!(issued.action(), action);
        assert_eq!(issued.expires_at_ms(), 61_000);
        assert_eq!(issued.token().expose().len(), 32);

        fixture
            .service
            .validate(
                &principal,
                issued.id(),
                action,
                "test_resource",
                &resource_id,
                "fingerprint-v1",
                &payload,
                issued.token(),
                2_000,
            )
            .expect("validate exact binding");

        let encoded = issued.token().encode();
        for path in fixture.database_files() {
            let bytes = fs::read(&path).expect("read operational store file");
            assert!(
                !contains(&bytes, encoded.as_bytes()),
                "raw token text leaked into {}",
                path.display()
            );
            assert!(
                !contains(&bytes, issued.token().expose()),
                "raw token bytes leaked into {}",
                path.display()
            );
        }
    }
}

#[test]
fn sid_action_target_payload_token_and_expiry_mismatch_share_one_redacted_error() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let payload = json!({"target": "orders", "overwrite": true});
    let issued = fixture
        .service
        .prepare(
            &owner,
            ConfirmationAction::RestoreOverwrite,
            "restore_target",
            "orders-recovered",
            "fingerprint-v1",
            &payload,
            1_000,
            1_000,
        )
        .expect("issue confirmation");
    let wrong_token = fixture
        .service
        .prepare(
            &owner,
            ConfirmationAction::RestoreOverwrite,
            "restore_target",
            "other",
            "fingerprint-v1",
            &payload,
            1_000,
            1_000,
        )
        .expect("issue another token");

    let cases = [
        fixture.service.validate(
            &fixture.admin(),
            issued.id(),
            ConfirmationAction::RestoreOverwrite,
            "restore_target",
            "orders-recovered",
            "fingerprint-v1",
            &payload,
            issued.token(),
            1_500,
        ),
        fixture.service.validate(
            &owner,
            issued.id(),
            ConfirmationAction::SecretRotate,
            "restore_target",
            "orders-recovered",
            "fingerprint-v1",
            &payload,
            issued.token(),
            1_500,
        ),
        fixture.service.validate(
            &owner,
            issued.id(),
            ConfirmationAction::RestoreOverwrite,
            "restore_target",
            "different",
            "fingerprint-v1",
            &payload,
            issued.token(),
            1_500,
        ),
        fixture.service.validate(
            &owner,
            issued.id(),
            ConfirmationAction::RestoreOverwrite,
            "restore_target",
            "orders-recovered",
            "fingerprint-v1",
            &json!({"target": "different", "overwrite": true}),
            issued.token(),
            1_500,
        ),
        fixture.service.validate(
            &owner,
            issued.id(),
            ConfirmationAction::RestoreOverwrite,
            "restore_target",
            "orders-recovered",
            "fingerprint-v1",
            &payload,
            wrong_token.token(),
            1_500,
        ),
        fixture.service.validate(
            &owner,
            issued.id(),
            ConfirmationAction::RestoreOverwrite,
            "restore_target",
            "orders-recovered",
            "fingerprint-v1",
            &payload,
            issued.token(),
            2_000,
        ),
    ];
    for result in cases {
        let error = result.expect_err("mismatched confirmation is denied");
        assert_eq!(error.code(), "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED");
        assert_eq!(error.to_string(), "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED");
    }
}

#[test]
fn revoke_and_expire_are_terminal_and_never_reissue_a_token() {
    let fixture = Fixture::new();
    let owner = fixture.owner();
    let first = fixture
        .service
        .prepare(
            &owner,
            ConfirmationAction::SecretRotate,
            "secret_ref",
            "db-auth",
            "fingerprint-v1",
            &json!({"rotate": true}),
            100,
            1_000,
        )
        .expect("issue revocable token");
    fixture
        .service
        .revoke(&owner, first.id(), 200)
        .expect("revoke token");
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation_state(first.id())
            .expect("revoked state"),
        "revoked"
    );
    let replay = fixture.service.revoke(&owner, first.id(), 300);
    assert_eq!(
        replay
            .expect_err("terminal token cannot be revoked twice")
            .code(),
        "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED"
    );

    let second = fixture
        .service
        .prepare(
            &owner,
            ConfirmationAction::AclChange,
            "acl",
            "operational-store",
            "fingerprint-v1",
            &json!({"change": "owner"}),
            100,
            100,
        )
        .expect("issue expiring token");
    assert_eq!(
        fixture.service.expire(200).expect("expire issued tokens"),
        1
    );
    assert_eq!(
        fixture
            .store
            .ipc()
            .confirmation_state(second.id())
            .expect("expired state"),
        "expired"
    );
}

struct Fixture {
    _temp: TempDir,
    path: std::path::PathBuf,
    store: OperationalStore,
    service: ConfirmationService,
    policy: AuthorizationPolicy,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let path = temp.path().join("operational.db");
        let store = OperationalStore::open(path.clone()).expect("open operational store");
        let policy = AuthorizationPolicy::new(
            sid(SERVICE_SID),
            sid(OWNER_SID),
            sid(OWNER_LOGON_SID),
            10,
            vec![sid(ADMIN_SID)],
        )
        .expect("authorization policy");
        let service = ConfirmationService::new(store.ipc());
        Self {
            _temp: temp,
            path,
            store,
            service,
            policy,
        }
    }

    fn owner(&self) -> dbbackup_service::ipc::AuthenticatedPrincipal {
        self.policy
            .authenticate(&TokenIdentity::new(
                sid(OWNER_SID),
                sid(OWNER_LOGON_SID),
                10,
                false,
                true,
            ))
            .expect("owner principal")
    }

    fn admin(&self) -> dbbackup_service::ipc::AuthenticatedPrincipal {
        self.policy
            .authenticate(&TokenIdentity::new(
                sid(ADMIN_SID),
                sid("S-1-5-5-30-40"),
                30,
                true,
                true,
            ))
            .expect("admin principal")
    }

    fn database_files(&self) -> Vec<std::path::PathBuf> {
        ["", "-wal", "-shm"]
            .into_iter()
            .map(|suffix| std::path::PathBuf::from(format!("{}{}", self.path.display(), suffix)))
            .filter(|path| path.exists())
            .collect()
    }
}

fn sid(value: &str) -> WindowsSid {
    WindowsSid::parse(value).expect("canonical SID")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|candidate| candidate == needle)
}
