use std::sync::Arc;

use dbbackup_application::{ApplicationCommand, ApplicationRequest, ApplicationUseCases};
use dbbackup_service::application::ServiceApplication;
use dbbackup_service::host::{HostConfig, HostRuntime, ServiceHost, ShutdownReason};
use dbbackup_service::ipc::{current_process_identity, WindowsSid};
use dbbackup_service::operations::{NativeConnectionInspector, ServiceOperationWorker, WorkerTick};
use dbbackup_service::secrets::{AclPolicy, ServiceSecretStore};
use serde_json::json;
use tempfile::TempDir;

#[test]
#[ignore = "managed PostgreSQL fixture required"]
fn real_postgresql_probe_and_discovery_use_encrypted_credentials_and_commit_targets() {
    let host_name = std::env::var("DBBACKUP_LIVE_PG_HOST").expect("live PostgreSQL host");
    let port: u16 = std::env::var("DBBACKUP_LIVE_PG_PORT")
        .expect("live PostgreSQL port")
        .parse()
        .expect("numeric PostgreSQL port");
    let username = std::env::var("DBBACKUP_LIVE_PG_USERNAME").expect("live PostgreSQL username");
    let password = std::env::var("DBBACKUP_LIVE_PG_PASSWORD").expect("live PostgreSQL password");

    let temp = TempDir::new().expect("temp directory");
    let host = ServiceHost::start(
        HostConfig::new(
            format!(
                "live-connection-worker-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start service host");
    let store = host.operational_store().expect("operational store");
    let owner_sid = current_process_identity()
        .expect("current identity")
        .user_sid()
        .clone();
    let service_sid = WindowsSid::parse("S-1-5-18").expect("LocalSystem SID");
    let secrets = Arc::new(
        ServiceSecretStore::open(
            temp.path().join("secrets"),
            store.secret_refs(),
            AclPolicy::new(service_sid, owner_sid).expect("secret ACL"),
        )
        .expect("secret store"),
    );
    let application = ServiceApplication::new(host.health_probe(), Some(store.clone()))
        .with_secret_store(Arc::clone(&secrets));

    let saved = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::ConnectionSave,
            payload: json!({
                "displayName": "Chronos live PostgreSQL",
                "engine": "postgresql",
                "transport": "tcp",
                "host": host_name,
                "port": port,
                "filePath": "",
                "tlsMode": "disabled",
                "username": username,
                "password": password,
            }),
        })
        .expect("save encrypted connection");
    let connection_id = saved.body["connectionId"]
        .as_str()
        .expect("connection ID")
        .to_owned();
    let discovery = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::DiscoveryRun,
            payload: json!({"connectionId": connection_id}),
        })
        .expect("enqueue discovery");
    let work_item_id = discovery.body["workItemId"]
        .as_str()
        .expect("work item ID")
        .to_owned();

    let worker = ServiceOperationWorker::new(
        store.clone(),
        Some(secrets),
        Arc::new(NativeConnectionInspector::default()),
        "live-postgresql-worker",
        uuid::Uuid::new_v4().to_string(),
    );
    assert_eq!(
        worker.run_once().expect("run live discovery"),
        WorkerTick::Completed(work_item_id.parse().expect("typed work item ID"))
    );
    let completed = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::WorkGet,
            payload: json!({"workItemId": work_item_id}),
        })
        .expect("read completed work");
    assert_eq!(completed.body["state"], "succeeded");
    assert!(
        completed.body["discovered"]
            .as_array()
            .expect("discovered array")
            .iter()
            .any(|database| database["databaseName"] == "chronos_orders"),
        "the managed fixture database must be discovered"
    );
    let selected = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::TargetSelect,
            payload: json!({
                "connectionId": connection_id,
                "discoveryRunId": discovery.body["discoveryRunId"],
                "engineObjectIds": ["database:chronos_orders"],
            }),
        })
        .expect("select live target");
    assert_eq!(selected.body["targets"][0]["displayName"], "chronos_orders");

    host.shutdown(ShutdownReason::Stop).expect("stop host");
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
