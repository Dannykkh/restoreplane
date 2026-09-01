use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::sync::Arc;

use dbbackup_application::ApplicationCommand;
use dbbackup_ipc_client::{
    BackupScopeObjectListRequest, BackupScopeSaveRequest, CommandOutcome, IpcClient,
};
#[cfg(windows)]
use dbbackup_service::identity_config::ServiceIdentityConfig;
#[cfg(windows)]
use dbbackup_service::ipc::WindowsSid;
#[cfg(windows)]
use dbbackup_service::recovery::{
    OfflineCutoverRecoveryRequestV1, OfflineCutoverRecoveryV1, WindowsLegacySchedulerControllerV1,
};

use crate::cli::{
    BackupScopeModeArg, BackupScopeSaveArgs, QualificationRequestArgs, RecoverCutoverArgs,
    RecoveryReasonArg, ServiceAction, ServiceScopeAction,
};

const MAX_QUALIFICATION_PAYLOAD_BYTES: u64 = 1024 * 1024;

pub fn run(action: ServiceAction) -> anyhow::Result<()> {
    match action {
        ServiceAction::RecoverCutover(args) => recover_cutover(args),
        ServiceAction::QualificationRequest(args) => run_qualification_request(args),
        action => run_ipc(action),
    }
}

fn run_qualification_request(args: QualificationRequestArgs) -> anyhow::Result<()> {
    let command = qualification_command(&args.command)?;
    let idempotency_key = args.idempotency_key.map(|key| key.to_string());
    if command.descriptor().requires_idempotency_key() != idempotency_key.is_some() {
        anyhow::bail!("RELEASE_QUALIFICATION_IDEMPOTENCY_INVALID");
    }
    let payload = read_qualification_payload(&args.payload_file)?;
    let runtime = tokio::runtime::Runtime::new()?;
    let outcome = runtime.block_on(async {
        IpcClient::production()
            .execute(command, idempotency_key, payload)
            .await
    })?;
    print_outcome(&outcome)?;
    if !outcome.is_success() {
        anyhow::bail!(outcome.code().to_owned());
    }
    Ok(())
}

fn qualification_command(name: &str) -> anyhow::Result<ApplicationCommand> {
    let command = ApplicationCommand::try_from(name)
        .map_err(|_| anyhow::anyhow!("RELEASE_QUALIFICATION_COMMAND_DENIED"))?;
    if matches!(
        command,
        ApplicationCommand::ServiceHealth
            | ApplicationCommand::WorkspaceCreate
            | ApplicationCommand::ConnectionSave
            | ApplicationCommand::ConnectionProbe
            | ApplicationCommand::DiscoveryRun
            | ApplicationCommand::DiscoveryResultList
            | ApplicationCommand::TargetSelect
            | ApplicationCommand::BackupScopeDiscoveryRun
            | ApplicationCommand::BackupScopeObjectList
            | ApplicationCommand::BackupScopeSave
            | ApplicationCommand::DestinationSave
            | ApplicationCommand::DestinationProbe
            | ApplicationCommand::PlanSave
            | ApplicationCommand::PlanPublish
            | ApplicationCommand::BackupEnqueue
            | ApplicationCommand::WorkGet
            | ApplicationCommand::SnapshotList
            | ApplicationCommand::SnapshotGet
            | ApplicationCommand::RestorePreflight
            | ApplicationCommand::RestoreEnqueueSafe
            | ApplicationCommand::VerificationEnqueue
    ) {
        Ok(command)
    } else {
        anyhow::bail!("RELEASE_QUALIFICATION_COMMAND_DENIED")
    }
}

fn read_qualification_payload(path: &Path) -> anyhow::Result<serde_json::Value> {
    let link_metadata = std::fs::symlink_metadata(path)
        .map_err(|_| anyhow::anyhow!("RELEASE_QUALIFICATION_PAYLOAD_INVALID"))?;
    if !link_metadata.file_type().is_file() || link_metadata.len() > MAX_QUALIFICATION_PAYLOAD_BYTES
    {
        anyhow::bail!("RELEASE_QUALIFICATION_PAYLOAD_INVALID");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if link_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            anyhow::bail!("RELEASE_QUALIFICATION_PAYLOAD_INVALID");
        }
    }

    let file = std::fs::File::open(path)
        .map_err(|_| anyhow::anyhow!("RELEASE_QUALIFICATION_PAYLOAD_INVALID"))?;
    let open_metadata = file
        .metadata()
        .map_err(|_| anyhow::anyhow!("RELEASE_QUALIFICATION_PAYLOAD_INVALID"))?;
    if !open_metadata.is_file() || open_metadata.len() > MAX_QUALIFICATION_PAYLOAD_BYTES {
        anyhow::bail!("RELEASE_QUALIFICATION_PAYLOAD_INVALID");
    }
    let mut bytes = Vec::with_capacity(open_metadata.len() as usize);
    file.take(MAX_QUALIFICATION_PAYLOAD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("RELEASE_QUALIFICATION_PAYLOAD_INVALID"))?;
    if bytes.len() as u64 > MAX_QUALIFICATION_PAYLOAD_BYTES {
        anyhow::bail!("RELEASE_QUALIFICATION_PAYLOAD_INVALID");
    }
    let payload: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("RELEASE_QUALIFICATION_PAYLOAD_INVALID"))?;
    if !payload.is_object() {
        anyhow::bail!("RELEASE_QUALIFICATION_PAYLOAD_INVALID");
    }
    Ok(payload)
}

fn run_ipc(action: ServiceAction) -> anyhow::Result<()> {
    if let ServiceAction::Scope { action } = &action {
        validate_scope_action(action)?;
    }
    let runtime = tokio::runtime::Runtime::new()?;
    let client = IpcClient::production();
    let outcome = runtime.block_on(async {
        match action {
            ServiceAction::Status => client.service_health().await,
            ServiceAction::Diagnostics => client.service_diagnostics().await,
            ServiceAction::Schema => client.schema_status().await,
            ServiceAction::SupportBundle => client.support_bundle_create().await,
            ServiceAction::Drain => client.service_drain().await,
            ServiceAction::ControlBackup => client.control_store_backup().await,
            ServiceAction::CommitUpdate => client.service_update_commit().await,
            ServiceAction::Scope { action } => run_scope_action(&client, action).await,
            ServiceAction::QualificationRequest(_) => {
                unreachable!("handled before IPC runtime")
            }
            ServiceAction::RecoverCutover(_) => unreachable!("handled before IPC runtime"),
        }
    })?;
    print_outcome(&outcome)?;
    if !outcome.is_success() {
        anyhow::bail!(outcome.code().to_owned());
    }
    Ok(())
}

async fn run_scope_action(
    client: &IpcClient,
    action: ServiceScopeAction,
) -> Result<CommandOutcome, dbbackup_ipc_client::ClientError> {
    match action {
        ServiceScopeAction::List(args) => {
            let workspace_id = args.workspace_id.map(|id| id.to_string());
            let target_id = args.database_target_id.map(|id| id.to_string());
            client
                .backup_scope_list(
                    workspace_id.as_deref(),
                    target_id.as_deref(),
                    args.cursor.as_deref(),
                    args.limit,
                    args.state.as_deref(),
                    args.search.as_deref(),
                )
                .await
        }
        ServiceScopeAction::Discover(args) => {
            let workspace_id = args.workspace_id.map(|id| id.to_string());
            let target_id = args.database_target_id.to_string();
            let idempotency_key = args.idempotency_key.to_string();
            client
                .backup_scope_discovery_run(workspace_id.as_deref(), &target_id, &idempotency_key)
                .await
        }
        ServiceScopeAction::Objects(args) => {
            let workspace_id = args.workspace_id.map(|id| id.to_string());
            let target_id = args.database_target_id.to_string();
            let discovery_run_id = args.discovery_run_id.to_string();
            client
                .backup_scope_object_list(BackupScopeObjectListRequest {
                    workspace_id: workspace_id.as_deref(),
                    database_target_id: &target_id,
                    discovery_run_id: &discovery_run_id,
                    cursor: args.cursor.as_deref(),
                    limit: args.limit,
                    search: args.search.as_deref(),
                    schema_name: args.schema_name.as_deref(),
                    eligibility: args.eligibility.as_deref(),
                })
                .await
        }
        ServiceScopeAction::Save(args) => {
            let workspace_id = args.workspace_id.map(|id| id.to_string());
            let target_id = args.database_target_id.to_string();
            let discovery_run_id = args.discovery_run_id.map(|id| id.to_string());
            let idempotency_key = args.idempotency_key.to_string();
            let mode = match args.mode {
                BackupScopeModeArg::WholeDatabase => "whole_database",
                BackupScopeModeArg::ExplicitTables => "explicit_tables",
            };
            client
                .backup_scope_save(BackupScopeSaveRequest {
                    workspace_id: workspace_id.as_deref(),
                    database_target_id: &target_id,
                    display_name: &args.display_name,
                    mode,
                    discovery_run_id: discovery_run_id.as_deref(),
                    engine_object_ids: &args.engine_object_ids,
                    idempotency_key: &idempotency_key,
                })
                .await
        }
    }
}

fn validate_scope_action(action: &ServiceScopeAction) -> anyhow::Result<()> {
    match action {
        ServiceScopeAction::List(args) => {
            let cursor_invalid = args.cursor.as_deref().is_some_and(|cursor| {
                cursor.is_empty()
                    || cursor.len() > 256
                    || cursor.contains(['\0', '\r', '\n'])
                    || cursor.split_once(':').is_none()
            });
            let state_invalid = args.state.as_deref().is_some_and(|state| {
                !matches!(state, "active" | "attention" | "disabled" | "retired")
            });
            let search_invalid = args.search.as_deref().is_some_and(|search| {
                search.trim().is_empty()
                    || search.len() > 256
                    || search.contains(['\0', '\r', '\n'])
            });
            if cursor_invalid || state_invalid || search_invalid {
                anyhow::bail!("BACKUP_SCOPE_CLI_INPUT_INVALID");
            }
        }
        ServiceScopeAction::Objects(args) => {
            if args.cursor.as_deref().is_some_and(|cursor| {
                cursor.is_empty()
                    || cursor.len() > 32
                    || !cursor.bytes().all(|byte| byte.is_ascii_digit())
            }) {
                anyhow::bail!("BACKUP_SCOPE_CLI_INPUT_INVALID");
            }
            if args.search.as_deref().is_some_and(invalid_scope_filter)
                || args
                    .schema_name
                    .as_deref()
                    .is_some_and(invalid_scope_filter)
                || args
                    .eligibility
                    .as_deref()
                    .is_some_and(|value| !matches!(value, "eligible" | "excluded" | "unsupported"))
            {
                anyhow::bail!("BACKUP_SCOPE_CLI_INPUT_INVALID");
            }
        }
        ServiceScopeAction::Save(args) => validate_scope_save(args)?,
        ServiceScopeAction::Discover(_) => {}
    }
    Ok(())
}

fn invalid_scope_filter(value: &str) -> bool {
    value.trim().is_empty() || value.len() > 256 || value.contains(['\0', '\r', '\n'])
}

fn validate_scope_save(args: &BackupScopeSaveArgs) -> anyhow::Result<()> {
    let name_valid = !args.display_name.trim().is_empty()
        && args.display_name.chars().count() <= 96
        && !args.display_name.chars().any(char::is_control);
    let object_ids_valid = args.engine_object_ids.len() <= 1_000
        && args.engine_object_ids.iter().all(|id| {
            !id.trim().is_empty() && id.len() <= 2_048 && !id.contains(['\0', '\r', '\n'])
        })
        && {
            let distinct = args
                .engine_object_ids
                .iter()
                .collect::<std::collections::HashSet<_>>();
            distinct.len() == args.engine_object_ids.len()
        };
    let mode_valid = match args.mode {
        BackupScopeModeArg::WholeDatabase => {
            args.discovery_run_id.is_none() && args.engine_object_ids.is_empty()
        }
        BackupScopeModeArg::ExplicitTables => {
            args.discovery_run_id.is_some() && !args.engine_object_ids.is_empty()
        }
    };
    if name_valid && object_ids_valid && mode_valid {
        Ok(())
    } else {
        anyhow::bail!("BACKUP_SCOPE_CLI_INPUT_INVALID")
    }
}

#[cfg(windows)]
fn recover_cutover(args: RecoverCutoverArgs) -> anyhow::Result<()> {
    use dbbackup_service::ipc::current_process_identity;

    let program_data_directory = resolve_program_data_directory(args.program_data_directory)?;
    let service_sid =
        WindowsSid::parse(&args.service_sid).map_err(|error| anyhow::anyhow!(error.code()))?;
    let identity_path = program_data_directory.join("service-identity.json");
    let identity = ServiceIdentityConfig::load(&identity_path, &service_sid)
        .map_err(|error| anyhow::anyhow!(error.code()))?;
    let cutover_config = identity
        .cutover_verifier()
        .ok_or_else(|| anyhow::anyhow!("CONFIG_CUTOVER_RECOVERY_EVIDENCE_INVALID"))?;
    let caller = current_process_identity().map_err(|error| anyhow::anyhow!(error.code()))?;
    let recovery = OfflineCutoverRecoveryV1::open(
        "default",
        program_data_directory.join("operational-v2.db"),
        program_data_directory.join(cutover_config.evidence_directory()),
        identity.owner_sid().clone(),
        Arc::new(WindowsLegacySchedulerControllerV1),
    )
    .map_err(|error| anyhow::anyhow!(error.code()))?;
    let result = recovery
        .execute(
            &caller,
            OfflineCutoverRecoveryRequestV1::new(
                args.rollback_id,
                args.cutover_marker,
                args.recovery_bundle_manifest,
                args.reason.into(),
                args.rolled_back_at_ms,
                args.confirm_cutover_marker,
            ),
        )
        .map_err(|error| anyhow::anyhow!(error.code()))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "schemaVersion": 1,
            "rollbackId": result.rollback_id(),
            "rollbackCreated": result.rollback_created(),
            "resumed": result.resumed(),
            "taskCount": result.task_count(),
            "status": "completed"
        }))?
    );
    Ok(())
}

#[cfg(not(windows))]
fn recover_cutover(_args: RecoverCutoverArgs) -> anyhow::Result<()> {
    anyhow::bail!("CONFIG_CUTOVER_RECOVERY_PLATFORM_UNSUPPORTED")
}

#[cfg(windows)]
fn resolve_program_data_directory(override_directory: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match override_directory {
        Some(directory) if !directory.as_os_str().is_empty() => Ok(directory),
        Some(_) => anyhow::bail!("CONFIG_CUTOVER_RECOVERY_INPUT_INVALID"),
        None => {
            let program_data = std::env::var_os("ProgramData")
                .ok_or_else(|| anyhow::anyhow!("CONFIG_CUTOVER_RECOVERY_INPUT_INVALID"))?;
            Ok(PathBuf::from(program_data).join("DbBackup"))
        }
    }
}

impl From<RecoveryReasonArg> for dbbackup_catalog::ConfigCutoverRollbackReason {
    fn from(value: RecoveryReasonArg) -> Self {
        match value {
            RecoveryReasonArg::ServiceStartFailed => Self::ServiceStartFailed,
            RecoveryReasonArg::PostCutoverHealthFailed => Self::PostCutoverHealthFailed,
            RecoveryReasonArg::OperatorRequested => Self::OperatorRequested,
        }
    }
}

fn print_outcome(outcome: &CommandOutcome) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(outcome)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn save_args(mode: BackupScopeModeArg) -> BackupScopeSaveArgs {
        BackupScopeSaveArgs {
            workspace_id: None,
            database_target_id: uuid::Uuid::new_v4(),
            idempotency_key: uuid::Uuid::new_v4(),
            display_name: "Orders only".to_owned(),
            mode,
            discovery_run_id: None,
            engine_object_ids: Vec::new(),
        }
    }

    #[test]
    fn whole_and_explicit_scope_inputs_fail_closed_before_ipc() {
        let whole = save_args(BackupScopeModeArg::WholeDatabase);
        validate_scope_save(&whole).expect("canonical whole scope");

        let mut explicit = save_args(BackupScopeModeArg::ExplicitTables);
        assert!(validate_scope_save(&explicit).is_err());
        explicit.discovery_run_id = Some(uuid::Uuid::new_v4());
        explicit.engine_object_ids = vec!["table:orders".to_owned()];
        validate_scope_save(&explicit).expect("canonical explicit scope");
        explicit.engine_object_ids.push("table:orders".to_owned());
        assert!(validate_scope_save(&explicit).is_err());
    }

    #[test]
    fn qualification_surface_is_an_exact_non_destructive_allowlist() {
        assert_eq!(
            qualification_command("backup.enqueue")
                .expect("backup command")
                .descriptor()
                .name(),
            "backup.enqueue"
        );
        assert_eq!(
            qualification_command("restore.enqueue_safe")
                .expect("safe restore command")
                .descriptor()
                .name(),
            "restore.enqueue_safe"
        );
        for denied in [
            "restore.enqueue_overwrite",
            "retention.delete",
            "tool.rescan",
            "config.cutover",
            "unknown.command",
        ] {
            let error = qualification_command(denied).expect_err("command must be denied");
            assert_eq!(error.to_string(), "RELEASE_QUALIFICATION_COMMAND_DENIED");
        }
    }

    #[test]
    fn qualification_payload_must_be_a_bounded_json_object() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let valid = directory.path().join("valid.json");
        std::fs::write(&valid, br#"{"workspaceId":"fixture"}"#).expect("write payload");
        assert!(read_qualification_payload(&valid)
            .expect("valid payload")
            .is_object());

        let array = directory.path().join("array.json");
        std::fs::write(&array, b"[]").expect("write array");
        assert_eq!(
            read_qualification_payload(&array)
                .expect_err("array must fail")
                .to_string(),
            "RELEASE_QUALIFICATION_PAYLOAD_INVALID"
        );

        let oversized = directory.path().join("oversized.json");
        let mut file = std::fs::File::create(&oversized).expect("create oversized payload");
        file.write_all(&vec![b' '; MAX_QUALIFICATION_PAYLOAD_BYTES as usize + 1])
            .expect("write oversized payload");
        assert_eq!(
            read_qualification_payload(&oversized)
                .expect_err("oversized payload must fail")
                .to_string(),
            "RELEASE_QUALIFICATION_PAYLOAD_INVALID"
        );
    }
}
