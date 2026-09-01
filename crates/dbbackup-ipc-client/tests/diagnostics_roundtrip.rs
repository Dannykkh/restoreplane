#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use dbbackup_application::{AuthoritySnapshot, CutoverMode, ExecutionAuthority};
use dbbackup_ipc_client::{IpcClient, DEFAULT_PIPE_NAME};
use dbbackup_service::application::ServiceApplication;
use dbbackup_service::host::{HostConfig, HostRuntime, ServiceHost, ShutdownReason};
use dbbackup_service::ipc::{
    current_process_identity, ApplicationFrameHandler, AuthorizationPolicy, DurableIpcDispatcher,
    PipeSecurityPolicy, PipeServerConfig, SecurePipeServer, WindowsSid,
};
use tempfile::TempDir;
use tokio::sync::{watch, Barrier};
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_client_reads_live_service_health_and_schema_over_the_real_pipe() {
    assert_eq!(DEFAULT_PIPE_NAME, r"\\.\pipe\DbBackup.control.v1");
    let temp = TempDir::new().expect("temp directory");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("client-test-{}", Uuid::new_v4()),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("service host");
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
    let authorization = AuthorizationPolicy::new(
        service_sid.clone(),
        identity.user_sid().clone(),
        identity.logon_sid().clone(),
        identity.session_id(),
        Vec::new(),
    )
    .expect("authorization");
    let pipe_security =
        PipeSecurityPolicy::new(service_sid, identity.user_sid().clone(), Vec::new())
            .expect("pipe security");
    let dispatcher = DurableIpcDispatcher::new(
        host.operational_store().expect("healthy store").ipc(),
        application,
        authority(),
    );
    let handler = Arc::new(ApplicationFrameHandler::new(authorization, dispatcher));
    let pipe_name = format!(r"\\.\pipe\DbBackup.client.{}", Uuid::new_v4());
    let server = SecurePipeServer::new(
        PipeServerConfig::new(pipe_name.clone(), 4, Duration::from_secs(1)).expect("server config"),
        pipe_security,
        handler,
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_task = tokio::spawn(server.serve_until(shutdown_rx));

    let client = IpcClient::new(pipe_name, Duration::from_secs(2)).expect("shared client");
    let health = client.service_health().await.expect("service health");
    assert!(health.is_success());
    assert_eq!(health.result()["phase"], "rehearsal_ready");
    assert_eq!(health.result()["executionAuthority"], "legacy_direct");

    let schema = client.schema_status().await.expect("schema status");
    assert!(schema.is_success());
    assert_eq!(schema.result()["userVersion"], 4);
    assert_eq!(schema.result()["applicationTableCount"], 67);

    shutdown_tx.send(true).expect("stop IPC");
    server_task
        .await
        .expect("server task")
        .expect("server shutdown");
    host.shutdown(ShutdownReason::Stop).expect("host shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_clients_wait_for_the_next_pipe_instance_when_all_instances_are_busy() {
    const CLIENTS: usize = 12;

    let temp = TempDir::new().expect("temp directory");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("client-busy-test-{}", Uuid::new_v4()),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("service host");
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
    let authorization = AuthorizationPolicy::new(
        service_sid.clone(),
        identity.user_sid().clone(),
        identity.logon_sid().clone(),
        identity.session_id(),
        Vec::new(),
    )
    .expect("authorization");
    let pipe_security =
        PipeSecurityPolicy::new(service_sid, identity.user_sid().clone(), Vec::new())
            .expect("pipe security");
    let dispatcher = DurableIpcDispatcher::new(
        host.operational_store().expect("healthy store").ipc(),
        application,
        authority(),
    );
    let handler = Arc::new(ApplicationFrameHandler::new(authorization, dispatcher));
    let pipe_name = format!(r"\\.\pipe\DbBackup.client.busy.{}", Uuid::new_v4());
    let server = SecurePipeServer::new(
        PipeServerConfig::new(pipe_name.clone(), 2, Duration::from_secs(1)).expect("server config"),
        pipe_security,
        handler,
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let server_task = tokio::spawn(server.serve_until(shutdown_rx));

    let barrier = Arc::new(Barrier::new(CLIENTS));
    let mut requests = Vec::with_capacity(CLIENTS);
    for _ in 0..CLIENTS {
        let client =
            IpcClient::new(pipe_name.clone(), Duration::from_secs(2)).expect("concurrent client");
        let barrier = Arc::clone(&barrier);
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            client.service_health().await
        }));
    }

    for request in requests {
        let health = request
            .await
            .expect("request task")
            .expect("service health");
        assert!(health.is_success());
    }

    shutdown_tx.send(true).expect("stop IPC");
    server_task
        .await
        .expect("server task")
        .expect("server shutdown");
    host.shutdown(ShutdownReason::Stop).expect("host shutdown");
}

fn authority() -> AuthoritySnapshot {
    AuthoritySnapshot::new(
        ExecutionAuthority::LegacyDirect,
        CutoverMode::RehearsalOnly,
        false,
        false,
    )
}

struct NoopRuntime;

impl HostRuntime for NoopRuntime {
    fn started(&self, _diagnostics_only: bool) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stop_accepting_ipc(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stop_claiming_work(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn drain_clients(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn checkpointed(&self) -> Result<(), dbbackup_service::host::HostError> {
        Ok(())
    }

    fn stopped(&self, _reason: ShutdownReason) {}
}
