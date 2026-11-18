use restoreplane_application::{ApplicationCommand, ApplicationRequest, ApplicationUseCases};
use restoreplane_catalog::{ConnectionProbeSpec, ConnectionProfileSpec, DatabaseTargetSpec};
use restoreplane_domain::workbench_protection::{WorkbenchSelection, WorkbenchSelectionItem};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, WorkItemId};
use restoreplane_service::application::ServiceApplication;
use restoreplane_service::host::{HostConfig, HostError, HostRuntime, ServiceHost, ShutdownReason};
use restoreplane_service::operations::{
    ConnectionInspection, ConnectionInspector, ConnectionOperationError, DatabaseCredentials,
    SelectedStructureInspection, ServiceOperationWorker, WorkerTick,
};
use serde_json::{json, Value};
use std::str::FromStr;
use std::sync::Arc;

struct Noop;
impl HostRuntime for Noop {
    fn started(&self, _: bool) -> Result<(), HostError> {
        Ok(())
    }
    fn stop_accepting_ipc(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn stop_claiming_work(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn drain_clients(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn checkpointed(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn stopped(&self, _: ShutdownReason) {}
}
struct SelectedInspector;
impl ConnectionInspector for SelectedInspector {
    fn inspect(
        &self,
        _: &restoreplane_catalog::ConnectionExecutionBinding,
        _: Option<&DatabaseCredentials>,
        _: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        panic!("selected discovery cannot fall back to full inspection")
    }
    fn inspect_selected_structure(
        &self,
        _: &restoreplane_catalog::ConnectionExecutionBinding,
        _: Option<&DatabaseCredentials>,
        _: &restoreplane_catalog::SelectedDatabaseTarget,
        items: &[WorkbenchSelectionItem],
    ) -> Result<SelectedStructureInspection, ConnectionOperationError> {
        Ok(SelectedStructureInspection {
            items: items.to_vec(),
            server_identity: "postgresql-system-id:42".into(),
        })
    }
}
fn now() -> i64 {
    1000
}

#[test]
fn authenticated_selected_scope_guards_owner_revision_exact_selection_and_local_atomic_link() {
    let temp = tempfile::tempdir().unwrap();
    let host = ServiceHost::start(
        HostConfig::new(
            format!("wb-protection-{}", uuid::Uuid::new_v4()),
            temp.path().join("service.db"),
        )
        .unwrap(),
        Arc::new(Noop),
    )
    .unwrap();
    let store = host.operational_store().unwrap();
    let profile = ConnectionProfileId::new();
    let target = DatabaseTargetId::new();
    store
        .configuration()
        .insert_connection(
            ConnectionProfileSpec::tcp(profile.clone(), "PG", "postgresql", "localhost", 5432, 1)
                .with_tls_mode("verify_full"),
        )
        .unwrap();
    store
        .configuration()
        .record_connection_probe(ConnectionProbeSpec::passed(
            uuid::Uuid::new_v4().to_string(),
            profile.clone(),
            "17.11",
            json!({"serverIdentity":"postgresql-system-id:42"}).to_string(),
            2,
            3,
        ))
        .unwrap();
    store
        .configuration()
        .insert_target(DatabaseTargetSpec::new(
            target.clone(),
            profile.clone(),
            "database:app",
            "app",
            "app",
            "source-fingerprint",
            4,
        ))
        .unwrap();
    let app = ServiceApplication::with_clock(host.health_probe(), Some(store.clone()), now);
    let owner = "1".repeat(64);
    let workspace = restoreplane_catalog::DEFAULT_WORKSPACE_ID;
    let receipt=app.authorize_workbench_binding(json!({"workspaceId":workspace,"connectionProfileId":profile,"databaseTargetId":target,"purpose":"protection","expectedDescriptorRevision":null,"appNonce":uuid::Uuid::new_v4()}),&owner,1000).unwrap();
    let selection = WorkbenchSelection {
        signature_version: 1,
        expected_descriptor_revision: receipt.body["descriptorRevision"].as_str().unwrap().into(),
        items: vec![WorkbenchSelectionItem {
            engine_object_id: "relation:42".into(),
            schema_name: "public".into(),
            object_name: "accounts".into(),
            structure_signature: "a".repeat(64),
        }],
    };
    let discovery_payload =
        json!({"workspaceId":workspace,"databaseTargetId":target,"workbenchSelection":selection});
    assert!(
        app.execute(ApplicationRequest {
            command: ApplicationCommand::BackupScopeDiscoveryRun,
            payload: discovery_payload.clone()
        })
        .is_err(),
        "JSON cannot provide native owner authority"
    );
    let response = app
        .execute_workbench_protection(
            ApplicationRequest {
                command: ApplicationCommand::BackupScopeDiscoveryRun,
                payload: discovery_payload,
            },
            &owner,
            1000,
        )
        .unwrap();
    let work = WorkItemId::from_str(response.body["workItemId"].as_str().unwrap()).unwrap();
    let worker = ServiceOperationWorker::with_clock(
        store.clone(),
        None,
        Arc::new(SelectedInspector),
        "selected-worker",
        "selected-boot",
        now,
    );
    assert_eq!(
        worker.run_once().unwrap(),
        WorkerTick::Completed(work.clone())
    );
    let projected = store
        .console_query()
        .work_item_in_workspace(workspace, work.as_str())
        .unwrap();
    let result = projected.result.unwrap();
    assert_eq!(result["inventoryCoverage"], "selected");
    let digest = result["workbenchSelection"]["selectionDigest"]
        .as_str()
        .unwrap();
    let discovery = projected.operation_run_id.unwrap();
    let payload = json!({"workspaceId":workspace,"databaseTargetId":target,"mode":"explicit_tables","displayName":"선택 보호","discoveryRunId":discovery,"engineObjectIds":["relation:42"],"expectedWorkbenchSelectionDigest":digest});
    let save = |payload: Value, owner: &str| {
        app.execute_workbench_protection(
            ApplicationRequest {
                command: ApplicationCommand::BackupScopeSave,
                payload,
            },
            owner,
            1000,
        )
    };
    assert!(
        save(payload.clone(), &"2".repeat(64)).is_err(),
        "proof belongs to its authenticated owner"
    );
    let mut changed = payload.clone();
    changed["engineObjectIds"] = json!(["relation:43"]);
    assert!(
        save(changed, &owner).is_err(),
        "exact selection cannot be substituted"
    );
    let saved = save(payload.clone(), &owner).unwrap();
    assert_eq!(
        saved.body["id"], discovery,
        "one discovery owns one Scope primary key"
    );
    let health = store
        .console_query()
        .backup_scopes_in_workspace(workspace, Some(target.as_str()), 1000)
        .unwrap();
    assert_eq!(
        health
            .iter()
            .find(|scope| scope.id == discovery)
            .unwrap()
            .drift_state,
        "current"
    );
    assert!(
        save(payload, &owner).is_err(),
        "a new request cannot create a second Scope from the same proof"
    );
    assert_eq!(store.writer_diagnostics().unwrap().user_version, 4);
    println!("08 service/catalog owner+revision+selection transaction PASS (inspector seam; no native pipe/remote proof claim)");
}
