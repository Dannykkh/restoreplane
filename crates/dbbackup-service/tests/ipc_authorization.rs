use dbbackup_application::ApplicationCommand;
use dbbackup_domain::IpcRequestId;
use dbbackup_service::ipc::{
    authorize_and_decode, encode_request, AuthorizationPolicy, IpcError, PrincipalKind,
    RequestEnvelope, TokenIdentity, WindowsSid,
};
use serde_json::json;

const SERVICE_SID: &str = "S-1-5-80-1001";
const OWNER_SID: &str = "S-1-5-21-100-200-300-1001";
const OWNER_LOGON_SID: &str = "S-1-5-5-10-20";
const ADMIN_SID: &str = "S-1-5-21-100-200-300-500";
const OTHER_SID: &str = "S-1-5-21-100-200-300-1002";

#[test]
fn owner_and_elevated_approved_admin_receive_stable_command_authorization() {
    let policy = policy();
    let owner = identity(OWNER_SID, OWNER_LOGON_SID, 10, false);
    let read = frame(ApplicationCommand::ServiceHealth, None, json!({}));
    let authorized =
        authorize_and_decode(&policy, &owner, &read, 1_500).expect("owner read command");
    assert_eq!(authorized.principal().kind(), PrincipalKind::Owner);
    assert_eq!(
        authorized.request().command(),
        ApplicationCommand::ServiceHealth
    );
    assert_eq!(authorized.principal().session_id(), 10);
    assert_eq!(authorized.principal().sid_hash().len(), 64);
    assert!(!authorized.principal().sid_hash().contains(OWNER_SID));

    let admin = identity(ADMIN_SID, "S-1-5-5-30-40", 30, true);
    let destructive = frame(
        ApplicationCommand::RestoreEnqueueOverwrite,
        Some("restore-overwrite-1"),
        json!({"target": "orders-recovered"}),
    );
    let authorized = authorize_and_decode(&policy, &admin, &destructive, 1_500)
        .expect("approved elevated administrator");
    assert_eq!(
        authorized.principal().kind(),
        PrincipalKind::ApprovedAdministrator
    );
}

#[test]
fn unknown_sid_cross_session_and_non_elevated_admin_are_denied_before_body_decode() {
    let policy = policy();
    let mut invalid_json = frame(
        ApplicationCommand::ConnectionSave,
        Some("connection-save-1"),
        json!({"displayName": "primary"}),
    );
    let end = invalid_json.len();
    invalid_json[end - 2..end].copy_from_slice(b"??");

    let cases = [
        identity(OTHER_SID, "S-1-5-5-11-21", 11, false),
        identity(OWNER_SID, "S-1-5-5-11-21", 11, false),
        identity(ADMIN_SID, "S-1-5-5-30-40", 30, false),
    ];
    for identity in cases {
        assert_code(
            authorize_and_decode(&policy, &identity, &invalid_json, 1_500),
            "IPC_UNAUTHORIZED",
        );
    }

    let owner = identity(OWNER_SID, OWNER_LOGON_SID, 10, false);
    assert_code(
        authorize_and_decode(&policy, &owner, &invalid_json, 1_500),
        "IPC_PAYLOAD_INVALID",
    );
}

#[test]
fn remote_or_malformed_token_identity_is_denied_without_resource_detail() {
    let policy = policy();
    let remote = TokenIdentity::new(
        WindowsSid::parse(OWNER_SID).expect("owner SID"),
        WindowsSid::parse(OWNER_LOGON_SID).expect("owner logon SID"),
        10,
        false,
        false,
    );
    let read = frame(ApplicationCommand::ServiceHealth, None, json!({}));
    assert_code(
        authorize_and_decode(&policy, &remote, &read, 1_500),
        "IPC_UNAUTHORIZED",
    );

    let malformed = WindowsSid::parse("owner-user").expect_err("SID must be canonical");
    assert_eq!(malformed.code(), "IPC_IDENTITY_INVALID");
}

#[test]
fn production_owner_session_binds_on_first_connection_and_rejects_other_sessions() {
    let policy = AuthorizationPolicy::new_unbound_owner(
        WindowsSid::parse(SERVICE_SID).expect("service SID"),
        WindowsSid::parse(OWNER_SID).expect("owner SID"),
        vec![WindowsSid::parse(ADMIN_SID).expect("administrator SID")],
    )
    .expect("unbound owner policy");
    let read = frame(ApplicationCommand::ServiceHealth, None, json!({}));

    authorize_and_decode(
        &policy,
        &identity(OWNER_SID, OWNER_LOGON_SID, 10, false),
        &read,
        1_500,
    )
    .expect("first owner session binds");
    assert_code(
        authorize_and_decode(
            &policy,
            &identity(OWNER_SID, "S-1-5-5-11-21", 11, false),
            &read,
            1_500,
        ),
        "IPC_UNAUTHORIZED",
    );
}

fn policy() -> AuthorizationPolicy {
    AuthorizationPolicy::new(
        WindowsSid::parse(SERVICE_SID).expect("service SID"),
        WindowsSid::parse(OWNER_SID).expect("owner SID"),
        WindowsSid::parse(OWNER_LOGON_SID).expect("owner logon SID"),
        10,
        vec![WindowsSid::parse(ADMIN_SID).expect("administrator SID")],
    )
    .expect("valid authorization policy")
}

fn identity(
    user_sid: &str,
    logon_sid: &str,
    session_id: u32,
    elevated_admin: bool,
) -> TokenIdentity {
    TokenIdentity::new(
        WindowsSid::parse(user_sid).expect("user SID"),
        WindowsSid::parse(logon_sid).expect("logon SID"),
        session_id,
        elevated_admin,
        true,
    )
}

fn frame(
    command: ApplicationCommand,
    idempotency_key: Option<&str>,
    payload: serde_json::Value,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            command,
            idempotency_key.map(str::to_owned),
            1_000,
            2_000,
            payload,
        )
        .expect("valid request"),
    )
    .expect("encode request")
}

fn assert_code<T: std::fmt::Debug>(result: Result<T, IpcError>, expected: &str) {
    let error = result.expect_err("request must be denied");
    assert_eq!(error.code(), expected);
    assert_eq!(error.to_string(), expected);
}
