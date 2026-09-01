#![cfg(windows)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbbackup_application::{
    ApplicationCommand, AuthoritySnapshot, CutoverMode, ExecutionAuthority,
};
use dbbackup_catalog::rusqlite::Connection;
use dbbackup_domain::IpcRequestId;
use dbbackup_service::application::ServiceApplication;
use dbbackup_service::host::{
    ClaimRuntime, HostConfig, HostError, HostPhase, HostRuntime, PipeHostRuntime, ServiceHost,
    ShutdownReason,
};
use dbbackup_service::ipc::{
    current_process_identity, decode_response, encode_request, pipe_request,
    ApplicationFrameHandler, AuthorizationPolicy, DurableIpcDispatcher, PipeSecurityPolicy,
    PipeServerConfig, ReadOnlyApplicationFrameHandler, RequestEnvelope, SecurePipeServer,
    WindowsSid,
};
use serde_json::json;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn host_runtime_publishes_ipc_before_ready_and_drains_it_before_checkpoint() {
    let temp = TempDir::new().expect("temp directory");
    let runtime = Arc::new(PipeHostRuntime::new());
    let host = ServiceHost::start(
        HostConfig::new(
            format!("pipe-runtime-{}", Uuid::new_v4()),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::clone(&runtime),
    )
    .expect("host");
    let application = Arc::new(ServiceApplication::new(
        host.health_probe(),
        host.operational_store(),
    ));
    let identity = current_process_identity().expect("current identity");
    let system = WindowsSid::parse("S-1-5-18").expect("SYSTEM SID");
    let local_service = WindowsSid::parse("S-1-5-19").expect("LOCAL SERVICE SID");
    let service_sid = if identity.user_sid() == &system {
        local_service
    } else {
        system
    };
    let authorization = AuthorizationPolicy::new_unbound_owner(
        service_sid.clone(),
        identity.user_sid().clone(),
        Vec::new(),
    )
    .expect("authorization");
    let security = PipeSecurityPolicy::new(service_sid, identity.user_sid().clone(), Vec::new())
        .expect("pipe security");
    let dispatcher = DurableIpcDispatcher::new(
        host.operational_store().expect("healthy store").ipc(),
        application,
        authority(),
    );
    let handler = Arc::new(ApplicationFrameHandler::new(authorization, dispatcher));
    let pipe_name = format!(r"\\.\pipe\DbBackup.runtime.{}", Uuid::new_v4());
    runtime
        .attach(SecurePipeServer::new(
            PipeServerConfig::new(pipe_name.clone(), 4, Duration::from_millis(250))
                .expect("server config"),
            security,
            handler,
        ))
        .expect("attach ready pipe");

    let tokio = tokio::runtime::Runtime::new().expect("client runtime");
    let issued_at_ms = now_ms();
    let frame = encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::ServiceHealth,
            None,
            issued_at_ms,
            issued_at_ms + 30_000,
            json!({}),
        )
        .expect("request"),
    )
    .expect("frame");
    let response = tokio
        .block_on(pipe_request(&pipe_name, &frame, Duration::from_secs(1)))
        .expect("service responds immediately after attach");
    let response = decode_response(&response).expect("decode");
    assert!(response.is_success());
    assert_eq!(response.body()["body"]["phase"], "rehearsal_ready");

    host.shutdown(ShutdownReason::Preshutdown)
        .expect("host drains IPC");
    let report = runtime.last_report().expect("runtime report");
    assert_eq!(report.accepted_connections(), 1);
    assert_eq!(report.completed_connections(), 1);
    assert_eq!(report.forced_closed_connections(), 0);
}

#[test]
fn diagnostics_only_pipe_serves_health_and_rejects_mutation_without_a_store() {
    let temp = TempDir::new().expect("temp directory");
    let database_path = temp.path().join("drift.db");
    let connection = Connection::open(&database_path).expect("drift database");
    connection
        .execute_batch("PRAGMA user_version=99;")
        .expect("future schema marker");
    drop(connection);

    let runtime = Arc::new(PipeHostRuntime::new());
    let host = ServiceHost::start(
        HostConfig::new(
            format!("diagnostics-runtime-{}", Uuid::new_v4()),
            database_path,
        )
        .expect("host config"),
        Arc::clone(&runtime),
    )
    .expect("diagnostics host");
    assert_eq!(host.health().phase(), HostPhase::DiagnosticsOnly);
    assert!(host.operational_store().is_none());

    let application = Arc::new(ServiceApplication::new(host.health_probe(), None));
    let identity = current_process_identity().expect("current identity");
    let system = WindowsSid::parse("S-1-5-18").expect("SYSTEM SID");
    let local_service = WindowsSid::parse("S-1-5-19").expect("LOCAL SERVICE SID");
    let service_sid = if identity.user_sid() == &system {
        local_service
    } else {
        system
    };
    let authorization = AuthorizationPolicy::new_unbound_owner(
        service_sid.clone(),
        identity.user_sid().clone(),
        Vec::new(),
    )
    .expect("authorization");
    let security = PipeSecurityPolicy::new(service_sid, identity.user_sid().clone(), Vec::new())
        .expect("pipe security");
    let handler = Arc::new(ReadOnlyApplicationFrameHandler::new(
        authorization,
        application,
    ));
    let pipe_name = format!(r"\\.\pipe\DbBackup.diagnostics.{}", Uuid::new_v4());
    runtime
        .attach(SecurePipeServer::new(
            PipeServerConfig::new(pipe_name.clone(), 4, Duration::from_millis(250))
                .expect("server config"),
            security,
            handler,
        ))
        .expect("attach diagnostics pipe");

    let tokio = tokio::runtime::Runtime::new().expect("client runtime");
    let health = tokio
        .block_on(pipe_request(
            &pipe_name,
            &request_frame(ApplicationCommand::ServiceHealth, None),
            Duration::from_secs(1),
        ))
        .expect("health response");
    let health = decode_response(&health).expect("decode health");
    assert!(health.is_success());
    assert_eq!(health.body()["body"]["phase"], "diagnostics_only");

    let mutation = tokio
        .block_on(pipe_request(
            &pipe_name,
            &request_frame(ApplicationCommand::ConnectionSave, Some("save-1")),
            Duration::from_secs(1),
        ))
        .expect("mutation rejection response");
    let mutation = decode_response(&mutation).expect("decode mutation rejection");
    assert!(!mutation.is_success());
    assert_eq!(mutation.code(), "APPLICATION_FORBIDDEN");

    host.shutdown(ShutdownReason::Stop)
        .expect("drain diagnostics IPC");
}

#[test]
fn pipe_runtime_stops_and_joins_registered_program_owned_claim_runtimes() {
    let runtime = PipeHostRuntime::new();
    let claim_runtime = Arc::new(RecordingClaimRuntime::default());
    runtime
        .attach_claim_runtime(claim_runtime.clone())
        .expect("attach program-owned scheduler");

    runtime
        .stop_claiming_work()
        .expect("request scheduler stop");
    runtime
        .drain_clients()
        .expect("join scheduler before checkpoint");

    assert_eq!(claim_runtime.stop_calls.load(Ordering::SeqCst), 1);
    assert_eq!(claim_runtime.join_calls.load(Ordering::SeqCst), 1);
}

#[derive(Default)]
struct RecordingClaimRuntime {
    stop_calls: AtomicUsize,
    join_calls: AtomicUsize,
}

impl ClaimRuntime for RecordingClaimRuntime {
    fn request_stop(&self) {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        self.join_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn authority() -> AuthoritySnapshot {
    AuthoritySnapshot::new(
        ExecutionAuthority::LegacyDirect,
        CutoverMode::RehearsalOnly,
        false,
        false,
    )
}

fn request_frame(command: ApplicationCommand, idempotency_key: Option<&str>) -> Vec<u8> {
    let issued_at_ms = now_ms();
    encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            command,
            idempotency_key.map(str::to_owned),
            issued_at_ms,
            issued_at_ms + 30_000,
            json!({}),
        )
        .expect("request"),
    )
    .expect("frame")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_millis() as i64
}
