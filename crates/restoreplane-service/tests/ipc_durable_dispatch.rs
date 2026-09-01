use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use restoreplane_application::{
    canonical_payload_digest, ApplicationCommand, ApplicationError, ApplicationRequest,
    ApplicationResponse, ApplicationUseCases, AuthoritySnapshot, CutoverMode, ExecutionAuthority,
};
use restoreplane_catalog::{IpcLedgerReservation, IpcRequestSpec, OperationalStore};
use restoreplane_domain::IpcRequestId;
use restoreplane_service::ipc::{
    authorize_and_decode, encode_request, AuthorizationPolicy, DurableIpcDispatcher,
    RequestEnvelope, TokenIdentity, WindowsSid,
};
use serde_json::json;
use tempfile::TempDir;

const SERVICE_SID: &str = "S-1-5-80-1001";
const OWNER_SID: &str = "S-1-5-21-100-200-300-1001";
const OWNER_LOGON_SID: &str = "S-1-5-5-10-20";

#[test]
fn completed_mutation_replays_after_service_restart_without_handler_reexecution() {
    let temp = TempDir::new().expect("temp directory");
    let path = temp.path().join("operational.db");
    let handler = Arc::new(CountingUseCases::default());
    let original_frame = frame(
        ApplicationCommand::ConnectionSave,
        "connection-save-01",
        json!({"displayName": "primary"}),
    );

    let store = OperationalStore::open(path.clone()).expect("open operational store");
    let dispatcher =
        DurableIpcDispatcher::new(store.ipc(), Arc::clone(&handler), rehearsal_authority());
    let first = dispatcher
        .dispatch(authorized(&original_frame))
        .expect("dispatch first mutation");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    drop(dispatcher);
    drop(store);

    let reopened = OperationalStore::open(path).expect("reopen after service restart");
    let dispatcher =
        DurableIpcDispatcher::new(reopened.ipc(), Arc::clone(&handler), rehearsal_authority());
    let replay = dispatcher
        .dispatch(authorized(&original_frame))
        .expect("replay completed mutation");
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(replay.body(), first.body());
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    let conflict = frame(
        ApplicationCommand::ConnectionSave,
        "connection-save-01",
        json!({"displayName": "different"}),
    );
    let error = dispatcher
        .dispatch(authorized(&conflict))
        .expect_err("same key with different payload conflicts");
    assert_eq!(error.code(), "IPC_IDEMPOTENCY_CONFLICT");
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn terminal_failure_replays_and_pre_g1_execution_never_reaches_ledger_or_handler() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("open operational store");
    let handler = Arc::new(CountingUseCases {
        fail: true,
        ..CountingUseCases::default()
    });
    let dispatcher =
        DurableIpcDispatcher::new(store.ipc(), Arc::clone(&handler), rehearsal_authority());
    let failing = frame(
        ApplicationCommand::ConnectionSave,
        "connection-save-failure",
        json!({"displayName": "primary"}),
    );

    let first = dispatcher
        .dispatch(authorized(&failing))
        .expect("terminal application failure is a response");
    assert!(!first.is_success());
    assert_eq!(first.code(), "APPLICATION_INTERNAL");
    let replay = dispatcher
        .dispatch(authorized(&failing))
        .expect("terminal failure replay");
    assert!(!replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    let backup = frame(
        ApplicationCommand::BackupEnqueue,
        "backup-before-g1",
        json!({"planId": "plan-1"}),
    );
    let error = dispatcher
        .dispatch(authorized(&backup))
        .expect_err("G1-before production enqueue is blocked");
    assert_eq!(error.code(), "APPLICATION_REHEARSAL_ONLY");
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    assert_eq!(store.ipc().request_count().expect("ledger count"), 1);
}

#[test]
fn expired_running_mutation_is_safely_finalized_without_handler_reexecution() {
    let temp = TempDir::new().expect("temp directory");
    let path = temp.path().join("operational.db");
    let handler = Arc::new(CountingUseCases::default());
    let original_frame = frame_at(
        ApplicationCommand::ConnectionSave,
        "connection-save-crash-window",
        json!({"displayName": "primary"}),
        1_000,
        2_000,
    );
    let original = authorized_at(&original_frame, 1_500);
    let original_request = original.request();
    let payload_digest =
        canonical_payload_digest(original_request.payload()).expect("canonical payload digest");

    let store = OperationalStore::open(path.clone()).expect("open operational store");
    assert_eq!(
        store
            .ipc()
            .reserve_request(IpcRequestSpec::new(
                original_request.request_id().clone(),
                original_request.command().descriptor().name(),
                original.principal().sid_hash(),
                original_request.idempotency_key().expect("idempotency key"),
                payload_digest,
                original.accepted_at_ms(),
                original_request.expires_at_ms(),
            ))
            .expect("reserve mutation before injected crash"),
        IpcLedgerReservation::New,
    );
    handler
        .execute(ApplicationRequest {
            command: original_request.command(),
            payload: original_request.payload().clone(),
        })
        .expect("mutation committed before injected crash");
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    drop(store);

    let reopened = OperationalStore::open(path.clone()).expect("reopen after injected crash");
    let dispatcher =
        DurableIpcDispatcher::new(reopened.ipc(), Arc::clone(&handler), rehearsal_authority());
    let retry_frame = frame_at(
        ApplicationCommand::ConnectionSave,
        "connection-save-crash-window",
        json!({"displayName": "primary"}),
        2_100,
        3_000,
    );
    let recovered = dispatcher
        .dispatch(authorized_at(&retry_frame, 2_200))
        .expect("expired running request safely finalizes");
    assert!(!recovered.is_success());
    assert!(recovered.deduplicated());
    assert_eq!(recovered.code(), "APPLICATION_OUTCOME_UNKNOWN");
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    let replay = dispatcher
        .dispatch(authorized_at(&retry_frame, 2_300))
        .expect("safe terminal outcome replays");
    assert_eq!(replay.code(), "APPLICATION_OUTCOME_UNKNOWN");
    assert!(replay.deduplicated());
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    let conflict_frame = frame_at(
        ApplicationCommand::ConnectionSave,
        "connection-save-crash-window",
        json!({"displayName": "different"}),
        2_400,
        3_400,
    );
    let conflict = dispatcher
        .dispatch(authorized_at(&conflict_frame, 2_500))
        .expect_err("same key with a different payload still conflicts");
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");
    assert_eq!(handler.calls.load(Ordering::SeqCst), 1);

    let connection =
        restoreplane_catalog::rusqlite::Connection::open(path).expect("open ledger state reader");
    let state: String = connection
        .query_row(
            "SELECT state FROM ipc_request_ledger
             WHERE idempotency_key = 'connection-save-crash-window'",
            [],
            |row| row.get(0),
        )
        .expect("recovered ledger state");
    assert_eq!(state, "failed_terminal");
}

#[derive(Default)]
struct CountingUseCases {
    calls: AtomicUsize,
    fail: bool,
}

impl ApplicationUseCases for CountingUseCases {
    fn execute(
        &self,
        _request: ApplicationRequest,
    ) -> Result<ApplicationResponse, ApplicationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(ApplicationError::Internal);
        }
        Ok(ApplicationResponse {
            deduplicated: false,
            resource: None,
            body: json!({"saved": true}),
        })
    }
}

fn rehearsal_authority() -> AuthoritySnapshot {
    AuthoritySnapshot::new(
        ExecutionAuthority::LegacyDirect,
        CutoverMode::RehearsalOnly,
        false,
        false,
    )
}

fn authorized(frame: &[u8]) -> restoreplane_service::ipc::AuthorizedRequest {
    authorized_at(frame, 1_500)
}

fn authorized_at(
    frame: &[u8],
    accepted_at_ms: i64,
) -> restoreplane_service::ipc::AuthorizedRequest {
    authorize_and_decode(&policy(), &owner_identity(), frame, accepted_at_ms)
        .expect("authorized request")
}

fn policy() -> AuthorizationPolicy {
    AuthorizationPolicy::new(
        WindowsSid::parse(SERVICE_SID).expect("service SID"),
        WindowsSid::parse(OWNER_SID).expect("owner SID"),
        WindowsSid::parse(OWNER_LOGON_SID).expect("owner logon SID"),
        10,
        Vec::new(),
    )
    .expect("authorization policy")
}

fn owner_identity() -> TokenIdentity {
    TokenIdentity::new(
        WindowsSid::parse(OWNER_SID).expect("owner SID"),
        WindowsSid::parse(OWNER_LOGON_SID).expect("owner logon SID"),
        10,
        false,
        true,
    )
}

fn frame(command: ApplicationCommand, key: &str, payload: serde_json::Value) -> Vec<u8> {
    frame_at(command, key, payload, 1_000, 2_000)
}

fn frame_at(
    command: ApplicationCommand,
    key: &str,
    payload: serde_json::Value,
    issued_at_ms: i64,
    expires_at_ms: i64,
) -> Vec<u8> {
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            command,
            Some(key.to_owned()),
            issued_at_ms,
            expires_at_ms,
            payload,
        )
        .expect("request"),
    )
    .expect("encoded request")
}
