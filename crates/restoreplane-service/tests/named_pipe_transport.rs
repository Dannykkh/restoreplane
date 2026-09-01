#![cfg(windows)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use restoreplane_application::{
    ApplicationCommand, ApplicationError, ApplicationRequest, ApplicationResponse,
    ApplicationUseCases, AuthoritySnapshot, CutoverMode, ExecutionAuthority,
};
use restoreplane_catalog::OperationalStore;
use restoreplane_domain::IpcRequestId;
use restoreplane_service::ipc::{
    current_process_identity, decode_response, encode_request, pipe_request,
    ApplicationFrameHandler, AuthorizationPolicy, DurableIpcDispatcher, PipeSecurityPolicy,
    PipeServerConfig, RequestEnvelope, SecurePipeServer, WindowsSid,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::windows::named_pipe::ClientOptions;
use tokio::sync::watch;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_pipe_roundtrip_is_authorized_bounded_durable_and_drains() {
    let temp = TempDir::new().expect("temp directory");
    let store =
        OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
    let identity = current_process_identity().expect("current identity");
    let system = WindowsSid::parse("S-1-5-18").expect("SYSTEM SID");
    let local_service = WindowsSid::parse("S-1-5-19").expect("LOCAL SERVICE SID");
    let service_sid = if identity.user_sid() == &system {
        local_service
    } else {
        system
    };
    let authorization = AuthorizationPolicy::new(
        service_sid.clone(),
        identity.user_sid().clone(),
        identity.logon_sid().clone(),
        identity.session_id(),
        Vec::new(),
    )
    .expect("authorization policy");
    let pipe_security =
        PipeSecurityPolicy::new(service_sid, identity.user_sid().clone(), Vec::new())
            .expect("pipe security");
    let use_cases = Arc::new(CountingUseCases::default());
    let dispatcher = DurableIpcDispatcher::new(
        store.ipc(),
        Arc::clone(&use_cases),
        AuthoritySnapshot::new(
            ExecutionAuthority::LegacyDirect,
            CutoverMode::RehearsalOnly,
            false,
            false,
        ),
    );
    let handler = Arc::new(ApplicationFrameHandler::new(authorization, dispatcher));
    let pipe_name = format!(r"\\.\pipe\RestorePlane.transport.{}", Uuid::new_v4());
    let config = PipeServerConfig::new(pipe_name.clone(), 2, Duration::from_millis(200))
        .expect("server config");
    let server = SecurePipeServer::new(config, pipe_security, handler);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_task = tokio::spawn(server.serve_until(shutdown_rx));

    let read = request(ApplicationCommand::ServiceHealth, None, json!({}));
    let read_response = decode_response(
        &pipe_request(&pipe_name, &read, Duration::from_secs(2))
            .await
            .expect("read roundtrip"),
    )
    .expect("decode read response");
    assert!(read_response.is_success());
    assert_eq!(read_response.body()["body"]["handled"], true);

    let mutation = request(
        ApplicationCommand::ConnectionSave,
        Some("pipe-mutation-1"),
        json!({"displayName": "primary"}),
    );
    let first = decode_response(
        &pipe_request(&pipe_name, &mutation, Duration::from_secs(2))
            .await
            .expect("first mutation roundtrip"),
    )
    .expect("decode first mutation");
    let replay = decode_response(
        &pipe_request(&pipe_name, &mutation, Duration::from_secs(2))
            .await
            .expect("replay roundtrip"),
    )
    .expect("decode replay");
    assert!(first.is_success());
    assert!(!first.deduplicated());
    assert!(replay.is_success());
    assert!(replay.deduplicated());
    assert_eq!(use_cases.calls.load(Ordering::SeqCst), 2);

    let conflict = request(
        ApplicationCommand::ConnectionSave,
        Some("pipe-mutation-1"),
        json!({"displayName": "different"}),
    );
    let conflict = decode_response(
        &pipe_request(&pipe_name, &conflict, Duration::from_secs(2))
            .await
            .expect("conflict roundtrip"),
    )
    .expect("decode conflict");
    assert!(!conflict.is_success());
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");
    assert_eq!(use_cases.calls.load(Ordering::SeqCst), 2);

    let mut partial = ClientOptions::new()
        .open(&pipe_name)
        .expect("open partial client");
    partial
        .write_all(&(128_u32).to_le_bytes())
        .await
        .expect("write only frame prefix");

    shutdown_tx.send(true).expect("signal shutdown");
    let report = server_task
        .await
        .expect("server task")
        .expect("server drain");
    assert_eq!(report.accepted_connections(), 5);
    assert_eq!(report.completed_connections(), 4);
    assert_eq!(report.failed_connections(), 1);
    assert!(report.peak_active_connections() <= 2);
    assert_eq!(report.forced_closed_connections(), 0);
}

#[derive(Default)]
struct CountingUseCases {
    calls: AtomicUsize,
}

impl ApplicationUseCases for CountingUseCases {
    fn execute(
        &self,
        _request: ApplicationRequest,
    ) -> Result<ApplicationResponse, ApplicationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ApplicationResponse {
            deduplicated: false,
            resource: None,
            body: json!({"handled": true}),
        })
    }
}

fn request(
    command: ApplicationCommand,
    idempotency_key: Option<&str>,
    payload: serde_json::Value,
) -> Vec<u8> {
    let issued_at_ms = now_ms();
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            command,
            idempotency_key.map(ToOwned::to_owned),
            issued_at_ms,
            issued_at_ms + 30_000,
            payload,
        )
        .expect("request envelope"),
    )
    .expect("encode request")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_millis() as i64
}
