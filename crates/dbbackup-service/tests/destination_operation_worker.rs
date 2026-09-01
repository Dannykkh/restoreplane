use std::sync::Arc;

use dbbackup_application::{ApplicationCommand, ApplicationRequest, ApplicationUseCases};
use dbbackup_service::application::ServiceApplication;
use dbbackup_service::host::{HostConfig, HostRuntime, ServiceHost, ShutdownReason};
use dbbackup_service::ipc::{current_process_identity, WindowsSid};
use dbbackup_service::operations::{
    ConnectionInspection, ConnectionInspector, ConnectionOperationError, DatabaseCredentials,
    ServiceOperationWorker, WorkerTick,
};
use dbbackup_service::secrets::{AclPolicy, ServiceSecretStore};
use serde_json::json;
use tempfile::TempDir;

struct UnexpectedConnectionInspector;

impl ConnectionInspector for UnexpectedConnectionInspector {
    fn inspect(
        &self,
        _profile: &dbbackup_catalog::ConnectionExecutionBinding,
        _credentials: Option<&DatabaseCredentials>,
        _discover: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        panic!("a Destination operation must not call the Connection inspector")
    }
}

#[test]
fn local_destination_probe_commits_independent_writer_reader_delete_evidence() {
    let temp = TempDir::new().expect("temp directory");
    let destination_root = temp.path().join("local-vault");
    let host = ServiceHost::start(
        HostConfig::new(
            format!(
                "destination-worker-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start host");
    let store = host.operational_store().expect("operational store");
    let owner_sid = current_process_identity()
        .expect("current identity")
        .user_sid()
        .clone();
    let secrets = Arc::new(
        ServiceSecretStore::open(
            temp.path().join("secrets"),
            store.secret_refs(),
            AclPolicy::new(
                WindowsSid::parse("S-1-5-18").expect("LocalSystem SID"),
                owner_sid,
            )
            .expect("secret ACL"),
        )
        .expect("secret store"),
    );
    let application = ServiceApplication::new(host.health_probe(), Some(store.clone()))
        .with_secret_store(Arc::clone(&secrets));
    let saved = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::DestinationSave,
            payload: json!({
                "displayName": "Local recovery vault",
                "kind": "local",
                "endpoint": {"path": destination_root},
                "credentials": {},
            }),
        })
        .expect("save local destination");
    let destination_id = saved.body["destinationId"]
        .as_str()
        .expect("destination ID")
        .to_owned();
    let queued = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::DestinationProbe,
            payload: json!({"destinationId": destination_id}),
        })
        .expect("enqueue destination probe");
    let work_item_id = queued.body["workItemId"]
        .as_str()
        .expect("work item ID")
        .parse()
        .expect("typed work item ID");

    let worker = ServiceOperationWorker::new(
        store.clone(),
        Some(secrets),
        Arc::new(UnexpectedConnectionInspector),
        "destination-worker-test",
        uuid::Uuid::new_v4().to_string(),
    );
    assert_eq!(
        worker.run_once().expect("run destination probe"),
        WorkerTick::Completed(work_item_id)
    );
    let destinations = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::DestinationList,
            payload: json!({"cursor": null, "limit": 25}),
        })
        .expect("destination inventory");
    let destination = &destinations.body["items"][0];
    assert_eq!(destination["id"], destination_id);
    assert_eq!(destination["state"], "ready");
    assert_eq!(destination["lastProbeStatus"], "passed");
    assert_eq!(destination["immutabilityCapability"], "none");
    let roles = destination["roles"].as_array().expect("destination roles");
    assert_eq!(roles.len(), 3);
    assert!(roles.iter().all(|role| role["probeStatus"] == "passed"));
    assert!(
        !destination_root.join(".selftest").exists()
            || std::fs::read_dir(destination_root.join(".selftest"))
                .expect("read probe directory")
                .next()
                .is_none(),
        "the local probe object must be deleted after independent reader verification"
    );

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
