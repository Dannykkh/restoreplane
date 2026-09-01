#![cfg(windows)]

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;

use restoreplane_application::{ApplicationCommand, ApplicationRequest, ApplicationUseCases};
use restoreplane_catalog::DestinationSpec;
use restoreplane_domain::DestinationId;
use restoreplane_service::application::ServiceApplication;
use restoreplane_service::host::{HostConfig, HostError, HostRuntime, ServiceHost, ShutdownReason};
use restoreplane_service::ipc::{current_process_identity, WindowsSid};
use restoreplane_service::repair::ServiceCatalogRebuildPreviewer;
use restoreplane_service::secrets::{AclPolicy, ServiceSecretStore};
use serde_json::json;
use tempfile::TempDir;

#[test]
fn local_v2_manifest_preview_classifies_without_mutating_catalog_projection() {
    let temp = TempDir::new().expect("temp directory");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("catalog-rebuild-preview-{}", std::process::id()),
            temp.path().join("operational.db"),
        )
        .expect("host config"),
        Arc::new(NoopRuntime),
    )
    .expect("start host");
    let store = host.operational_store().expect("operational store");
    let root = temp.path().join("destination");
    let manifest = root
        .join("v2")
        .join("occurrence")
        .join("target")
        .join("recovery-point")
        .join("set-manifest.json");
    fs::create_dir_all(manifest.parent().expect("manifest parent"))
        .expect("create manifest parent");
    fs::write(&manifest, br#"{"schemaVersion":2,"malformed":true}"#)
        .expect("write malformed v2 manifest");
    let destination_id = DestinationId::new();
    store
        .configuration()
        .insert_destination(DestinationSpec::local(
            destination_id.clone(),
            "local-rebuild-source",
            json!({ "path": root }).to_string(),
            10,
        ))
        .expect("insert local destination");

    let owner_sid = current_process_identity()
        .expect("current identity")
        .user_sid()
        .clone();
    let service_sid = WindowsSid::parse("S-1-5-18").expect("LocalSystem SID");
    assert_ne!(owner_sid, service_sid, "fixture identities must differ");
    let secrets = Arc::new(
        ServiceSecretStore::open(
            temp.path().join("secret-store"),
            store.secret_refs(),
            AclPolicy::new(service_sid, owner_sid).expect("secret ACL"),
        )
        .expect("secret store"),
    );
    let previewer = Arc::new(ServiceCatalogRebuildPreviewer::new(
        store.configuration(),
        secrets,
        BTreeMap::new(),
    ));
    let application = ServiceApplication::new(host.health_probe(), Some(store.clone()))
        .with_catalog_rebuild_previewer(previewer);

    let result = application
        .execute(ApplicationRequest {
            command: ApplicationCommand::CatalogRebuildPreview,
            payload: json!({
                "destinationId": destination_id.as_str(),
                "jobFilter": "v2",
            }),
        })
        .expect("catalog rebuild preview");

    assert_eq!(result.body["available"], true);
    assert_eq!(result.body["totalCount"], 1);
    assert_eq!(result.body["acceptedCount"], 0);
    assert_eq!(result.body["rejectedCount"], 1);
    assert_eq!(result.body["duplicateCount"], 0);
    assert_eq!(result.body["unknownKeyCount"], 0);
    assert_eq!(result.body["candidates"][0]["disposition"], "rejected");
    assert_eq!(
        result.body["candidates"][0]["reasonCode"],
        "MALFORMED_ENVELOPE"
    );
    assert_eq!(
        store
            .catalog_projection()
            .recovery_point_count()
            .expect("projection count"),
        0,
        "preview must never apply a rebuild plan"
    );

    host.shutdown(ShutdownReason::Stop).expect("stop host");
}

struct NoopRuntime;

impl HostRuntime for NoopRuntime {
    fn started(&self, _diagnostics_only: bool) -> Result<(), HostError> {
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

    fn stopped(&self, _reason: ShutdownReason) {}
}
