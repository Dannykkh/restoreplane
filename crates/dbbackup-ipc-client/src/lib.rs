// Shared local IPC client for CLI and Tauri callers.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbbackup_application::ApplicationCommand;
use dbbackup_domain::IpcRequestId;
use dbbackup_service::ipc::{
    decode_response, encode_request, IpcError, RequestEnvelope, ResponseEnvelope,
};
use serde::Serialize;
use serde_json::{json, Value};
use thiserror::Error;

#[cfg(windows)]
use dbbackup_service::ipc::{pipe_request, PipeTransportError};

pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\DbBackup.control.v1";
const REQUEST_TTL_MS: i64 = 30_000;
const MAX_CLIENT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Debug)]
pub struct IpcClient {
    pipe_name: String,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct BackupScopeSaveRequest<'a> {
    pub workspace_id: Option<&'a str>,
    pub database_target_id: &'a str,
    pub display_name: &'a str,
    pub mode: &'a str,
    pub discovery_run_id: Option<&'a str>,
    pub engine_object_ids: &'a [String],
    pub idempotency_key: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub struct BackupScopeObjectListRequest<'a> {
    pub workspace_id: Option<&'a str>,
    pub database_target_id: &'a str,
    pub discovery_run_id: &'a str,
    pub cursor: Option<&'a str>,
    pub limit: u32,
    pub search: Option<&'a str>,
    pub schema_name: Option<&'a str>,
    pub eligibility: Option<&'a str>,
}

impl IpcClient {
    pub fn new(pipe_name: impl Into<String>, timeout: Duration) -> Result<Self, ClientError> {
        let pipe_name = pipe_name.into();
        if !pipe_name.starts_with(r"\\.\pipe\DbBackup.")
            || pipe_name.len() > 256
            || pipe_name.chars().any(|character| character == '\0')
            || timeout.is_zero()
            || timeout > MAX_CLIENT_TIMEOUT
        {
            return Err(ClientError::ConfigInvalid);
        }
        Ok(Self { pipe_name, timeout })
    }

    pub fn production() -> Self {
        Self {
            pipe_name: DEFAULT_PIPE_NAME.to_owned(),
            timeout: Duration::from_secs(5),
        }
    }

    pub async fn service_health(&self) -> Result<CommandOutcome, ClientError> {
        self.execute(ApplicationCommand::ServiceHealth, None, json!({}))
            .await
    }

    pub async fn schema_status(&self) -> Result<CommandOutcome, ClientError> {
        self.execute(ApplicationCommand::SchemaStatus, None, json!({}))
            .await
    }

    pub async fn service_diagnostics(&self) -> Result<CommandOutcome, ClientError> {
        self.execute(ApplicationCommand::ServiceDiagnostics, None, json!({}))
            .await
    }

    pub async fn support_bundle_create(&self) -> Result<CommandOutcome, ClientError> {
        let idempotency_key = IpcRequestId::new().as_str().to_owned();
        self.execute(
            ApplicationCommand::SupportBundleCreate,
            Some(idempotency_key),
            json!({}),
        )
        .await
    }

    pub async fn service_drain(&self) -> Result<CommandOutcome, ClientError> {
        let idempotency_key = IpcRequestId::new().as_str().to_owned();
        self.execute(
            ApplicationCommand::ServiceDrain,
            Some(idempotency_key),
            json!({}),
        )
        .await
    }

    pub async fn control_store_backup(&self) -> Result<CommandOutcome, ClientError> {
        let idempotency_key = IpcRequestId::new().as_str().to_owned();
        self.execute(
            ApplicationCommand::ControlStoreBackup,
            Some(idempotency_key),
            json!({}),
        )
        .await
    }

    pub async fn service_update_commit(&self) -> Result<CommandOutcome, ClientError> {
        let idempotency_key = IpcRequestId::new().as_str().to_owned();
        self.execute(
            ApplicationCommand::ServiceUpdateCommit,
            Some(idempotency_key),
            json!({}),
        )
        .await
    }

    pub async fn backup_scope_list(
        &self,
        workspace_id: Option<&str>,
        database_target_id: Option<&str>,
        cursor: Option<&str>,
        limit: u32,
        state: Option<&str>,
        search: Option<&str>,
    ) -> Result<CommandOutcome, ClientError> {
        let payload = backup_scope_list_payload(
            workspace_id,
            database_target_id,
            cursor,
            limit,
            state,
            search,
        );
        self.execute(
            ApplicationCommand::BackupScopeList,
            None,
            Value::Object(payload),
        )
        .await
    }

    pub async fn backup_scope_discovery_run(
        &self,
        workspace_id: Option<&str>,
        database_target_id: &str,
        idempotency_key: &str,
    ) -> Result<CommandOutcome, ClientError> {
        let mut payload = workspace_payload(workspace_id);
        payload.insert(
            "databaseTargetId".to_owned(),
            Value::String(database_target_id.to_owned()),
        );
        self.execute(
            ApplicationCommand::BackupScopeDiscoveryRun,
            Some(idempotency_key.to_owned()),
            Value::Object(payload),
        )
        .await
    }

    pub async fn backup_scope_object_list(
        &self,
        request: BackupScopeObjectListRequest<'_>,
    ) -> Result<CommandOutcome, ClientError> {
        let payload = backup_scope_object_list_payload(request);
        self.execute(
            ApplicationCommand::BackupScopeObjectList,
            None,
            Value::Object(payload),
        )
        .await
    }

    pub async fn backup_scope_save(
        &self,
        request: BackupScopeSaveRequest<'_>,
    ) -> Result<CommandOutcome, ClientError> {
        let mut payload = workspace_payload(request.workspace_id);
        payload.insert(
            "databaseTargetId".to_owned(),
            Value::String(request.database_target_id.to_owned()),
        );
        payload.insert(
            "displayName".to_owned(),
            Value::String(request.display_name.to_owned()),
        );
        payload.insert("mode".to_owned(), Value::String(request.mode.to_owned()));
        payload.insert(
            "engineObjectIds".to_owned(),
            Value::Array(
                request
                    .engine_object_ids
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        if let Some(discovery_run_id) = request.discovery_run_id {
            payload.insert(
                "discoveryRunId".to_owned(),
                Value::String(discovery_run_id.to_owned()),
            );
        }
        self.execute(
            ApplicationCommand::BackupScopeSave,
            Some(request.idempotency_key.to_owned()),
            Value::Object(payload),
        )
        .await
    }

    pub async fn execute(
        &self,
        command: ApplicationCommand,
        idempotency_key: Option<String>,
        payload: Value,
    ) -> Result<CommandOutcome, ClientError> {
        let issued_at_ms = unix_time_ms()?;
        let request = RequestEnvelope::new(
            IpcRequestId::new(),
            command,
            idempotency_key,
            issued_at_ms,
            issued_at_ms.saturating_add(REQUEST_TTL_MS),
            payload,
        )?;
        let frame = encode_request(&request)?;
        let response = send_request(&self.pipe_name, &frame, self.timeout).await?;
        let response = decode_response(&response)?;
        if response.request_id() != request.request_id() {
            return Err(ClientError::ResponseMismatch);
        }
        Ok(CommandOutcome::from_response(response))
    }
}

fn workspace_payload(workspace_id: Option<&str>) -> serde_json::Map<String, Value> {
    let mut payload = serde_json::Map::new();
    if let Some(workspace_id) = workspace_id {
        payload.insert(
            "workspaceId".to_owned(),
            Value::String(workspace_id.to_owned()),
        );
    }
    payload
}

fn backup_scope_list_payload(
    workspace_id: Option<&str>,
    database_target_id: Option<&str>,
    cursor: Option<&str>,
    limit: u32,
    state: Option<&str>,
    search: Option<&str>,
) -> serde_json::Map<String, Value> {
    let mut payload = workspace_payload(workspace_id);
    if let Some(database_target_id) = database_target_id {
        payload.insert(
            "databaseTargetId".to_owned(),
            Value::String(database_target_id.to_owned()),
        );
    }
    payload.insert(
        "cursor".to_owned(),
        cursor.map_or(Value::Null, |value| Value::String(value.to_owned())),
    );
    payload.insert("limit".to_owned(), Value::from(limit));
    if let Some(state) = state {
        payload.insert("state".to_owned(), Value::String(state.to_owned()));
    }
    if let Some(search) = search {
        payload.insert("search".to_owned(), Value::String(search.to_owned()));
    }
    payload
}

fn backup_scope_object_list_payload(
    request: BackupScopeObjectListRequest<'_>,
) -> serde_json::Map<String, Value> {
    let mut payload = workspace_payload(request.workspace_id);
    payload.insert(
        "databaseTargetId".to_owned(),
        Value::String(request.database_target_id.to_owned()),
    );
    payload.insert(
        "discoveryRunId".to_owned(),
        Value::String(request.discovery_run_id.to_owned()),
    );
    payload.insert(
        "cursor".to_owned(),
        request
            .cursor
            .map_or(Value::Null, |value| Value::String(value.to_owned())),
    );
    payload.insert("limit".to_owned(), Value::from(request.limit));
    if let Some(search) = request.search {
        payload.insert("search".to_owned(), Value::String(search.to_owned()));
    }
    if let Some(schema_name) = request.schema_name {
        payload.insert(
            "schemaName".to_owned(),
            Value::String(schema_name.to_owned()),
        );
    }
    if let Some(eligibility) = request.eligibility {
        payload.insert(
            "eligibility".to_owned(),
            Value::String(eligibility.to_owned()),
        );
    }
    payload
}

#[cfg(windows)]
async fn send_request(
    pipe_name: &str,
    frame: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>, ClientError> {
    pipe_request(pipe_name, frame, timeout)
        .await
        .map_err(ClientError::Transport)
}

#[cfg(not(windows))]
async fn send_request(
    _pipe_name: &str,
    _frame: &[u8],
    _timeout: Duration,
) -> Result<Vec<u8>, ClientError> {
    Err(ClientError::PlatformUnsupported)
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandOutcome {
    request_id: String,
    success: bool,
    deduplicated: bool,
    code: String,
    result: Value,
    error: Option<Value>,
}

impl CommandOutcome {
    fn from_response(response: ResponseEnvelope) -> Self {
        let success = response.is_success();
        let result = if success {
            response.body().get("body").cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let error = (!success).then(|| response.body().clone());
        Self {
            request_id: response.request_id().as_str().to_owned(),
            success,
            deduplicated: response.deduplicated(),
            code: response.code().to_owned(),
            result,
            error,
        }
    }

    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    pub const fn is_success(&self) -> bool {
        self.success
    }

    pub const fn deduplicated(&self) -> bool {
        self.deduplicated
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub const fn result(&self) -> &Value {
        &self.result
    }

    pub const fn error(&self) -> Option<&Value> {
        self.error.as_ref()
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("IPC_CLIENT_CONFIG_INVALID")]
    ConfigInvalid,
    #[error("IPC_CLIENT_CLOCK_INVALID")]
    ClockInvalid,
    #[error("{0}")]
    Protocol(#[from] IpcError),
    #[cfg(windows)]
    #[error("{0}")]
    Transport(#[source] PipeTransportError),
    #[error("IPC_CLIENT_RESPONSE_MISMATCH")]
    ResponseMismatch,
    #[error("IPC_CLIENT_PLATFORM_UNSUPPORTED")]
    PlatformUnsupported,
}

impl ClientError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid => "IPC_CLIENT_CONFIG_INVALID",
            Self::ClockInvalid => "IPC_CLIENT_CLOCK_INVALID",
            Self::Protocol(error) => error.code(),
            #[cfg(windows)]
            Self::Transport(error) => error.code(),
            Self::ResponseMismatch => "IPC_CLIENT_RESPONSE_MISMATCH",
            Self::PlatformUnsupported => "IPC_CLIENT_PLATFORM_UNSUPPORTED",
        }
    }
}

fn unix_time_ms() -> Result<i64, ClientError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ClientError::ClockInvalid)?
        .as_millis();
    i64::try_from(millis).map_err(|_| ClientError::ClockInvalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_list_payload_preserves_every_page_and_filter_field() {
        let payload = backup_scope_list_payload(
            Some("workspace-id"),
            Some("target-id"),
            Some("120:scope-id"),
            40,
            Some("active"),
            Some("orders"),
        );
        assert_eq!(payload["workspaceId"], "workspace-id");
        assert_eq!(payload["databaseTargetId"], "target-id");
        assert_eq!(payload["cursor"], "120:scope-id");
        assert_eq!(payload["limit"], 40);
        assert_eq!(payload["state"], "active");
        assert_eq!(payload["search"], "orders");
    }

    #[test]
    fn scope_object_payload_binds_filters_to_an_explicit_discovery_run() {
        let payload = backup_scope_object_list_payload(BackupScopeObjectListRequest {
            workspace_id: Some("workspace-id"),
            database_target_id: "target-id",
            discovery_run_id: "discovery-id",
            cursor: Some("12"),
            limit: 25,
            search: Some("orders"),
            schema_name: Some("public"),
            eligibility: Some("eligible"),
        });
        assert_eq!(payload["workspaceId"], "workspace-id");
        assert_eq!(payload["databaseTargetId"], "target-id");
        assert_eq!(payload["discoveryRunId"], "discovery-id");
        assert_eq!(payload["cursor"], "12");
        assert_eq!(payload["limit"], 25);
        assert_eq!(payload["search"], "orders");
        assert_eq!(payload["schemaName"], "public");
        assert_eq!(payload["eligibility"], "eligible");
    }
}
