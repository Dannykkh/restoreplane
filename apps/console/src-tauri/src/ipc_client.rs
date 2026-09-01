use restoreplane_application::ApplicationCommand;
use restoreplane_ipc_client::{CommandOutcome, IpcClient};
use serde_json::Value;

#[tauri::command]
pub async fn service_request(
    command: String,
    payload: Value,
    idempotency_key: Option<String>,
) -> Result<CommandOutcome, String> {
    let command =
        ApplicationCommand::try_from(command.as_str()).map_err(|error| error.code().to_owned())?;
    IpcClient::production()
        .execute(command, idempotency_key, payload)
        .await
        .map_err(|error| error.code().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_command_names_resolve_only_through_the_application_catalog() {
        for name in [
            "service.health",
            "schema.status",
            "workspace.list",
            "workspace.create",
            "connection.list",
            "destination.list",
            "plan.list",
            "activity.list",
            "snapshot.list",
            "snapshot.get",
            "work.get",
            "settings.read",
        ] {
            assert!(
                ApplicationCommand::try_from(name).is_ok(),
                "{name} must be owned by the service command catalog"
            );
        }
        assert!(ApplicationCommand::try_from("list_databases").is_err());
        assert!(ApplicationCommand::try_from("run_job").is_err());
    }
}
