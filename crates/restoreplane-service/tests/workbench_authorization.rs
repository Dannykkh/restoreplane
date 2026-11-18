use restoreplane_application::{
    ApplicationCommand, ApplicationUseCases, AuthoritySnapshot, CutoverMode, ExecutionAuthority,
};
use restoreplane_catalog::{ConnectionProfileSpec, DatabaseTargetSpec};
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, IpcRequestId, WorkspaceId};
use restoreplane_service::application::ServiceApplication;
use restoreplane_service::host::{HostConfig, HostError, HostRuntime, ServiceHost, ShutdownReason};
use restoreplane_service::ipc::{
    authorize_and_decode, encode_request, AuthorizationPolicy, DurableIpcDispatcher,
    RequestEnvelope, TokenIdentity, WindowsSid,
};
use serde_json::{json, Value};
use std::str::FromStr;
use std::sync::Arc;

const SERVICE: &str = "S-1-5-80-1001";
const OWNER: &str = "S-1-5-21-100-200-300-1001";
const LOGON: &str = "S-1-5-5-10-20";
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

#[test]
fn workbench_authenticated_owner_gets_real_scoped_receipt_and_stale_revision_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let host = ServiceHost::start(
        HostConfig::new(
            format!("workbench-auth-{}", uuid::Uuid::new_v4()),
            temp.path().join("operational.db"),
        )
        .unwrap(),
        Arc::new(Noop),
    )
    .unwrap();
    let store = host.operational_store().unwrap();
    let connection = ConnectionProfileId::new();
    let target = DatabaseTargetId::new();
    store
        .configuration()
        .insert_connection(
            ConnectionProfileSpec::tcp(
                connection.clone(),
                "PG",
                "postgresql",
                "localhost",
                5432,
                100,
            )
            .with_tls_mode("verify_full"),
        )
        .unwrap();
    store
        .configuration()
        .insert_target(DatabaseTargetSpec::new(
            target.clone(),
            connection.clone(),
            "oid:20",
            "orders",
            "orders",
            "fp-orders",
            110,
        ))
        .unwrap();
    let app = Arc::new(ServiceApplication::new(
        host.health_probe(),
        Some(store.clone()),
    ));
    let dispatcher = DurableIpcDispatcher::new(
        store.ipc(),
        app.clone(),
        AuthoritySnapshot::new(
            ExecutionAuthority::LegacyDirect,
            CutoverMode::RehearsalOnly,
            false,
            false,
        ),
    );
    let payload = json!({"workspaceId":WorkspaceId::from_str(restoreplane_catalog::DEFAULT_WORKSPACE_ID).unwrap(),"connectionProfileId":connection,"databaseTargetId":target,"purpose":"connect","expectedDescriptorRevision":null,"appNonce":uuid::Uuid::new_v4()});
    let policy = AuthorizationPolicy::new(
        WindowsSid::parse(SERVICE).unwrap(),
        WindowsSid::parse(OWNER).unwrap(),
        WindowsSid::parse(LOGON).unwrap(),
        10,
        Vec::new(),
    )
    .unwrap();
    let invoke = |payload: Value, elevated: bool| {
        let request = RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::WorkbenchBindingAuthorize,
            None,
            1000,
            2000,
            payload,
        )
        .unwrap();
        let frame = encode_request(&request).unwrap();
        let identity = TokenIdentity::new(
            WindowsSid::parse(OWNER).unwrap(),
            WindowsSid::parse(LOGON).unwrap(),
            10,
            elevated,
            true,
        );
        dispatcher
            .dispatch(authorize_and_decode(&policy, &identity, &frame, 1500).unwrap())
            .unwrap()
    };
    let response = invoke(payload.clone(), false);
    assert!(response.is_success(), "{}", response.code());
    let body = &response.body()["body"];
    assert_eq!(body["descriptor"]["database"]["name"], "orders");
    assert_eq!(body["descriptorRevision"].as_str().unwrap().len(), 64);
    let serialized = body.to_string();
    assert!(!serialized.contains("secretRefId"));
    assert!(!serialized.contains("password"));
    let mut stale = payload.clone();
    stale["expectedDescriptorRevision"] = json!("f".repeat(64));
    assert_eq!(invoke(stale, false).code(), "WB_BINDING_STALE");
    assert_eq!(
        invoke(payload.clone(), true).code(),
        "APPLICATION_FORBIDDEN"
    );
    let mut forged = payload;
    forged["ownerSid"] = json!(OWNER);
    assert_eq!(invoke(forged, false).code(), "APPLICATION_PAYLOAD_INVALID");
    assert!(
        app.execute(restoreplane_application::ApplicationRequest {
            command: ApplicationCommand::WorkbenchBindingAuthorize,
            payload: json!({})
        })
        .is_err(),
        "no unauthenticated application path"
    );
    assert_eq!(store.writer_diagnostics().unwrap().user_version, 4);
}
