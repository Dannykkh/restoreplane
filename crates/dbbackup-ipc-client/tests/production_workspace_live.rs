#![cfg(windows)]

use dbbackup_application::ApplicationCommand;
use dbbackup_catalog::DEFAULT_WORKSPACE_ID;
use dbbackup_ipc_client::IpcClient;
use serde_json::json;

#[tokio::test]
#[ignore = "requires the installed DbBackupService production pipe"]
async fn installed_service_lists_the_default_workspace_over_the_production_pipe() {
    let outcome = IpcClient::production()
        .execute(ApplicationCommand::WorkspaceList, None, json!({}))
        .await
        .expect("workspace.list over the installed service pipe");

    assert!(outcome.is_success(), "unexpected code: {}", outcome.code());
    let workspaces = outcome.result().as_array().expect("workspace array");
    assert!(workspaces.iter().any(|workspace| {
        workspace.get("id").and_then(serde_json::Value::as_str) == Some(DEFAULT_WORKSPACE_ID)
            && workspace
                .get("displayName")
                .and_then(serde_json::Value::as_str)
                == Some("Default Workspace")
            && workspace
                .get("isDefault")
                .and_then(serde_json::Value::as_bool)
                == Some(true)
    }));
}
