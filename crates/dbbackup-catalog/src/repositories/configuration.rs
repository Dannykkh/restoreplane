use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use dbbackup_core::{RequestedVerificationLevel, VerificationPolicy};
use dbbackup_domain::{
    BackupPlanId, BackupScopeId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, DomainError, PlanRevisionId, SecretRefId, WorkspaceId,
};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use uuid::Uuid;

use crate::{OperationalStore, DEFAULT_WORKSPACE_ID};

use super::RepositoryError;

#[derive(Clone, Debug)]
pub struct ConnectionProfileSpec {
    id: ConnectionProfileId,
    display_name: String,
    engine: String,
    transport: String,
    host: Option<String>,
    port: Option<u16>,
    file_path: Option<String>,
    tls_mode: String,
    adapter_config_json: String,
    source_resource_key: String,
    created_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionExecutionBinding {
    id: ConnectionProfileId,
    engine: String,
    transport: String,
    host: Option<String>,
    port: Option<u16>,
    file_path: Option<String>,
    tls_mode: String,
    database_auth_secret_ref_id: Option<SecretRefId>,
}

impl ConnectionExecutionBinding {
    pub fn id(&self) -> &ConnectionProfileId {
        &self.id
    }

    pub fn engine(&self) -> &str {
        &self.engine
    }

    pub fn transport(&self) -> &str {
        &self.transport
    }

    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    pub const fn port(&self) -> Option<u16> {
        self.port
    }

    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    pub fn tls_mode(&self) -> &str {
        &self.tls_mode
    }

    pub fn database_auth_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.database_auth_secret_ref_id.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct ConnectionProbeSpec {
    id: String,
    connection_profile_id: ConnectionProfileId,
    status: &'static str,
    failure_class: Option<&'static str>,
    server_version: Option<String>,
    observed_capabilities_json: String,
    redacted_diagnostics: Option<String>,
    started_at_ms: i64,
    completed_at_ms: i64,
}

impl ConnectionProbeSpec {
    pub fn passed(
        id: impl Into<String>,
        connection_profile_id: ConnectionProfileId,
        server_version: impl Into<String>,
        observed_capabilities_json: impl Into<String>,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            connection_profile_id,
            status: "passed",
            failure_class: None,
            server_version: Some(server_version.into()),
            observed_capabilities_json: observed_capabilities_json.into(),
            redacted_diagnostics: None,
            started_at_ms,
            completed_at_ms,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn failed(
        id: impl Into<String>,
        connection_profile_id: ConnectionProfileId,
        failure_class: &'static str,
        observed_capabilities_json: impl Into<String>,
        redacted_diagnostics: impl Into<String>,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            connection_profile_id,
            status: "failed",
            failure_class: Some(failure_class),
            server_version: None,
            observed_capabilities_json: observed_capabilities_json.into(),
            redacted_diagnostics: Some(redacted_diagnostics.into()),
            started_at_ms,
            completed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_canonical_uuid(&self.id)
            || !matches!(self.status, "passed" | "failed")
            || self.failure_class.is_some_and(|value| {
                !matches!(
                    value,
                    "dns"
                        | "network"
                        | "tls"
                        | "authentication"
                        | "authorization"
                        | "tool_missing"
                        | "tool_untrusted"
                        | "unsupported_version"
                        | "unknown"
                )
            })
            || (self.status == "passed"
                && (self.failure_class.is_some()
                    || self
                        .server_version
                        .as_ref()
                        .is_none_or(|value| value.is_empty() || value.len() > 512)))
            || (self.status == "failed" && self.failure_class.is_none())
            || self.observed_capabilities_json.len() > 64 * 1024
            || serde_json::from_str::<serde_json::Value>(&self.observed_capabilities_json)
                .map(|value| !value.is_object())
                .unwrap_or(true)
            || self.redacted_diagnostics.as_ref().is_some_and(|value| {
                value.is_empty() || value.len() > 4 * 1024 || value.contains(['\0', '\r', '\n'])
            })
            || self.started_at_ms < 0
            || self.completed_at_ms < self.started_at_ms
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

impl ConnectionProfileSpec {
    pub fn tcp(
        id: ConnectionProfileId,
        display_name: impl Into<String>,
        engine: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        created_at_ms: i64,
    ) -> Self {
        let engine = engine.into();
        let host = host.into();
        Self {
            id,
            display_name: display_name.into(),
            source_resource_key: format!("tcp:{engine}:{host}:{port}"),
            engine,
            transport: "tcp".to_owned(),
            host: Some(host),
            port: Some(port),
            file_path: None,
            tls_mode: "disabled".to_owned(),
            adapter_config_json: "{}".to_owned(),
            created_at_ms,
        }
    }

    pub fn file(
        id: ConnectionProfileId,
        display_name: impl Into<String>,
        engine: impl Into<String>,
        file_path: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        let engine = engine.into();
        let file_path = file_path.into();
        Self {
            id,
            display_name: display_name.into(),
            source_resource_key: format!("file:{engine}:{file_path}"),
            engine,
            transport: "file".to_owned(),
            host: None,
            port: None,
            file_path: Some(file_path),
            tls_mode: "disabled".to_owned(),
            adapter_config_json: "{}".to_owned(),
            created_at_ms,
        }
    }

    pub fn with_tls_mode(mut self, tls_mode: impl Into<String>) -> Self {
        self.tls_mode = tls_mode.into();
        self
    }

    pub fn with_restore_test_target(mut self, enabled: bool) -> Self {
        self.adapter_config_json = serde_json::json!({
            "restoreTestTarget": enabled,
        })
        .to_string();
        self
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        let endpoint_valid = match self.transport.as_str() {
            "tcp" => {
                self.host
                    .as_ref()
                    .is_some_and(|host| !host.trim().is_empty() && host.len() <= 1_024)
                    && self.port.is_some()
                    && self.file_path.is_none()
            }
            "file" => {
                self.file_path
                    .as_ref()
                    .is_some_and(|path| !path.trim().is_empty() && path.len() <= 4_096)
                    && self.host.is_none()
                    && self.port.is_none()
            }
            _ => false,
        };
        if self.display_name.trim().is_empty()
            || self.display_name.len() > 256
            || !matches!(
                self.engine.as_str(),
                "postgresql"
                    | "mysql"
                    | "mariadb"
                    | "sqlserver"
                    | "sqlite"
                    | "mongodb"
                    | "firebird"
                    | "redis"
                    | "valkey"
            )
            || !matches!(
                self.tls_mode.as_str(),
                "disabled" | "preferred" | "required" | "verify_ca" | "verify_full"
            )
            || self.adapter_config_json.len() > 64 * 1024
            || serde_json::from_str::<serde_json::Value>(&self.adapter_config_json)
                .map(|value| !value.is_object())
                .unwrap_or(true)
            || !endpoint_valid
            || self.source_resource_key.is_empty()
            || self.source_resource_key.len() > 8_192
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseTargetSpec {
    id: DatabaseTargetId,
    connection_profile_id: ConnectionProfileId,
    engine_object_id: String,
    display_name: String,
    normalized_name: String,
    source_fingerprint: String,
    selected_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectedDatabaseTarget {
    id: DatabaseTargetId,
    connection_profile_id: ConnectionProfileId,
    engine_object_id: String,
    display_name: String,
    normalized_name: String,
    source_fingerprint: String,
}

impl SelectedDatabaseTarget {
    pub fn id(&self) -> &DatabaseTargetId {
        &self.id
    }

    pub fn connection_profile_id(&self) -> &ConnectionProfileId {
        &self.connection_profile_id
    }

    pub fn engine_object_id(&self) -> &str {
        &self.engine_object_id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub fn normalized_name(&self) -> &str {
        &self.normalized_name
    }

    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }
}

impl DatabaseTargetSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: DatabaseTargetId,
        connection_profile_id: ConnectionProfileId,
        engine_object_id: impl Into<String>,
        display_name: impl Into<String>,
        normalized_name: impl Into<String>,
        source_fingerprint: impl Into<String>,
        selected_at_ms: i64,
    ) -> Self {
        Self {
            id,
            connection_profile_id,
            engine_object_id: engine_object_id.into(),
            display_name: display_name.into(),
            normalized_name: normalized_name.into(),
            source_fingerprint: source_fingerprint.into(),
            selected_at_ms,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DestinationSpec {
    id: DestinationId,
    display_name: String,
    kind: String,
    endpoint_json: String,
    created_at_ms: i64,
}

impl DestinationSpec {
    pub fn local(
        id: DestinationId,
        display_name: impl Into<String>,
        endpoint_json: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            kind: "local".to_owned(),
            endpoint_json: endpoint_json.into(),
            created_at_ms,
        }
    }

    pub fn s3(
        id: DestinationId,
        display_name: impl Into<String>,
        endpoint_json: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self::with_kind(id, display_name, "s3", endpoint_json, created_at_ms)
    }

    pub fn ssh_sftp(
        id: DestinationId,
        display_name: impl Into<String>,
        endpoint_json: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self::with_kind(id, display_name, "ssh_sftp", endpoint_json, created_at_ms)
    }

    fn with_kind(
        id: DestinationId,
        display_name: impl Into<String>,
        kind: impl Into<String>,
        endpoint_json: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            kind: kind.into(),
            endpoint_json: endpoint_json.into(),
            created_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        let endpoint = serde_json::from_str::<serde_json::Value>(&self.endpoint_json)
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        if self.display_name.trim().is_empty()
            || self.display_name.len() > 256
            || !matches!(self.kind.as_str(), "local" | "s3" | "ssh_sftp")
            || self.endpoint_json.len() > 64 * 1024
            || !endpoint.is_object()
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DestinationSecretRoleV2 {
    BackupWriter,
    RestoreReader,
    DeleteOperator,
    HostAuth,
}

impl DestinationSecretRoleV2 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BackupWriter => "backup_writer",
            Self::RestoreReader => "restore_reader",
            Self::DeleteOperator => "delete_operator",
            Self::HostAuth => "host_auth",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationProbeBinding {
    destination_id: DestinationId,
    kind: String,
    endpoint_json: String,
    writer_secret_ref_id: Option<SecretRefId>,
    reader_secret_ref_id: Option<SecretRefId>,
    delete_secret_ref_id: Option<SecretRefId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackupDestinationBinding {
    destination: DestinationProbeBinding,
    requirement: DestinationRequirement,
    state: String,
}

impl BackupDestinationBinding {
    pub fn destination_id(&self) -> &DestinationId {
        self.destination.destination_id()
    }

    pub fn fingerprint(&self) -> String {
        self.destination.fingerprint()
    }

    pub fn kind(&self) -> &str {
        self.destination.kind()
    }

    pub const fn requirement(&self) -> DestinationRequirement {
        self.requirement
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub const fn destination(&self) -> &DestinationProbeBinding {
        &self.destination
    }
}

impl DestinationProbeBinding {
    pub fn destination_id(&self) -> &DestinationId {
        &self.destination_id
    }

    pub fn fingerprint(&self) -> String {
        format!("{}:{}", self.kind, self.destination_id)
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn endpoint_json(&self) -> &str {
        &self.endpoint_json
    }

    pub fn writer_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.writer_secret_ref_id.as_ref()
    }

    pub fn reader_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.reader_secret_ref_id.as_ref()
    }

    pub fn delete_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.delete_secret_ref_id.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct DestinationProbeSpec {
    id: String,
    destination_id: DestinationId,
    role: DestinationSecretRoleV2,
    status: &'static str,
    observed_capabilities_json: String,
    free_bytes: Option<i64>,
    latency_ms: i64,
    failure_class: Option<String>,
    redacted_diagnostics: Option<String>,
    started_at_ms: i64,
    completed_at_ms: i64,
}

impl DestinationProbeSpec {
    #[allow(clippy::too_many_arguments)]
    pub fn passed(
        id: impl Into<String>,
        destination_id: DestinationId,
        role: DestinationSecretRoleV2,
        observed_capabilities_json: impl Into<String>,
        free_bytes: Option<i64>,
        latency_ms: i64,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            destination_id,
            role,
            status: "passed",
            observed_capabilities_json: observed_capabilities_json.into(),
            free_bytes,
            latency_ms,
            failure_class: None,
            redacted_diagnostics: None,
            started_at_ms,
            completed_at_ms,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn failed(
        id: impl Into<String>,
        destination_id: DestinationId,
        role: DestinationSecretRoleV2,
        observed_capabilities_json: impl Into<String>,
        failure_class: impl Into<String>,
        redacted_diagnostics: impl Into<String>,
        latency_ms: i64,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            destination_id,
            role,
            status: "failed",
            observed_capabilities_json: observed_capabilities_json.into(),
            free_bytes: None,
            latency_ms,
            failure_class: Some(failure_class.into()),
            redacted_diagnostics: Some(redacted_diagnostics.into()),
            started_at_ms,
            completed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_canonical_uuid(&self.id)
            || !matches!(
                self.role,
                DestinationSecretRoleV2::BackupWriter
                    | DestinationSecretRoleV2::RestoreReader
                    | DestinationSecretRoleV2::DeleteOperator
            )
            || !matches!(self.status, "passed" | "failed")
            || self.observed_capabilities_json.len() > 64 * 1024
            || serde_json::from_str::<serde_json::Value>(&self.observed_capabilities_json)
                .map(|value| !value.is_object())
                .unwrap_or(true)
            || self.free_bytes.is_some_and(|value| value < 0)
            || self.latency_ms < 0
            || (self.status == "passed"
                && (self.failure_class.is_some() || self.redacted_diagnostics.is_some()))
            || (self.status == "failed"
                && (self
                    .failure_class
                    .as_ref()
                    .is_none_or(|value| value.is_empty() || value.len() > 128)
                    || self.redacted_diagnostics.as_ref().is_none_or(|value| {
                        value.is_empty()
                            || value.len() > 4 * 1024
                            || value.contains(['\0', '\r', '\n'])
                    })))
            || self.started_at_ms < 0
            || self.completed_at_ms < self.started_at_ms
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DestinationSecretBindingSpec {
    destination_id: DestinationId,
    role: DestinationSecretRoleV2,
    secret_ref_id: SecretRefId,
    created_at_ms: i64,
}

impl DestinationSecretBindingSpec {
    pub fn new(
        destination_id: DestinationId,
        role: DestinationSecretRoleV2,
        secret_ref_id: SecretRefId,
        created_at_ms: i64,
    ) -> Self {
        Self {
            destination_id,
            role,
            secret_ref_id,
            created_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.created_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationDeleteBindingV2 {
    destination_id: DestinationId,
    fingerprint: String,
    kind: String,
    endpoint_json: String,
    delete_secret_ref_id: Option<SecretRefId>,
}

impl DestinationDeleteBindingV2 {
    pub fn destination_id(&self) -> &DestinationId {
        &self.destination_id
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn endpoint_json(&self) -> &str {
        &self.endpoint_json
    }

    pub fn delete_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.delete_secret_ref_id.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DestinationRepairBindingV2 {
    destination_id: DestinationId,
    fingerprint: String,
    kind: String,
    endpoint_json: String,
    writer_secret_ref_id: Option<SecretRefId>,
    reader_secret_ref_id: Option<SecretRefId>,
}

impl DestinationRepairBindingV2 {
    pub fn destination_id(&self) -> &DestinationId {
        &self.destination_id
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn endpoint_json(&self) -> &str {
        &self.endpoint_json
    }

    pub fn writer_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.writer_secret_ref_id.as_ref()
    }

    pub fn reader_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.reader_secret_ref_id.as_ref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaRepairSecretRefsV2 {
    source_reader_secret_ref_id: Option<SecretRefId>,
    target_writer_secret_ref_id: Option<SecretRefId>,
    target_reader_secret_ref_id: Option<SecretRefId>,
}

impl ReplicaRepairSecretRefsV2 {
    pub const fn new(
        source_reader_secret_ref_id: Option<SecretRefId>,
        target_writer_secret_ref_id: Option<SecretRefId>,
        target_reader_secret_ref_id: Option<SecretRefId>,
    ) -> Self {
        Self {
            source_reader_secret_ref_id,
            target_writer_secret_ref_id,
            target_reader_secret_ref_id,
        }
    }

    pub const fn local() -> Self {
        Self::new(None, None, None)
    }

    pub fn source_reader_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.source_reader_secret_ref_id.as_ref()
    }

    pub fn target_writer_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.target_writer_secret_ref_id.as_ref()
    }

    pub fn target_reader_secret_ref_id(&self) -> Option<&SecretRefId> {
        self.target_reader_secret_ref_id.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct PlanDraftSpec {
    plan_id: BackupPlanId,
    revision_id: PlanRevisionId,
    display_name: String,
    revision_no: i64,
    schedule_kind: String,
    schedule_expression: Option<String>,
    timezone: String,
    catchup_policy: String,
    overlap_policy: String,
    rpo_ms: Option<i64>,
    retention_policy_json: String,
    verification_policy_json: String,
    execution_policy_json: String,
    content_hash_blake3: String,
    created_at_ms: i64,
}

impl PlanDraftSpec {
    pub fn manual(
        plan_id: BackupPlanId,
        revision_id: PlanRevisionId,
        display_name: impl Into<String>,
        revision_no: i64,
        content_hash_blake3: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            plan_id,
            revision_id,
            display_name: display_name.into(),
            revision_no,
            schedule_kind: "manual".to_owned(),
            schedule_expression: None,
            timezone: "UTC".to_owned(),
            catchup_policy: "skip".to_owned(),
            overlap_policy: "coalesce_one".to_owned(),
            rpo_ms: None,
            retention_policy_json: "{}".to_owned(),
            verification_policy_json: "{}".to_owned(),
            execution_policy_json: "{}".to_owned(),
            content_hash_blake3: content_hash_blake3.into(),
            created_at_ms,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn cron(
        plan_id: BackupPlanId,
        revision_id: PlanRevisionId,
        display_name: impl Into<String>,
        revision_no: i64,
        schedule_expression: impl Into<String>,
        timezone: impl Into<String>,
        content_hash_blake3: impl Into<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            plan_id,
            revision_id,
            display_name: display_name.into(),
            revision_no,
            schedule_kind: "cron".to_owned(),
            schedule_expression: Some(schedule_expression.into()),
            timezone: timezone.into(),
            catchup_policy: "bounded_latest_one".to_owned(),
            overlap_policy: "coalesce_one".to_owned(),
            rpo_ms: None,
            retention_policy_json: "{}".to_owned(),
            verification_policy_json: "{}".to_owned(),
            execution_policy_json: "{}".to_owned(),
            content_hash_blake3: content_hash_blake3.into(),
            created_at_ms,
        }
    }

    pub fn with_policies(
        mut self,
        rpo_ms: Option<i64>,
        retention_policy_json: impl Into<String>,
        verification_policy_json: impl Into<String>,
        execution_policy_json: impl Into<String>,
    ) -> Self {
        self.rpo_ms = rpo_ms;
        self.retention_policy_json = retention_policy_json.into();
        self.verification_policy_json = verification_policy_json.into();
        self.execution_policy_json = execution_policy_json.into();
        self
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        let schedule_valid = match self.schedule_kind.as_str() {
            "manual" => self.schedule_expression.is_none(),
            "cron" => self
                .schedule_expression
                .as_deref()
                .is_some_and(|value| !value.is_empty() && value.len() <= 256),
            _ => false,
        };
        if self.display_name.is_empty()
            || self.display_name.len() > 256
            || self.revision_no <= 0
            || !schedule_valid
            || self.timezone.is_empty()
            || self.timezone.len() > 128
            || !matches!(self.catchup_policy.as_str(), "skip" | "bounded_latest_one")
            || !matches!(
                self.overlap_policy.as_str(),
                "coalesce_one" | "queue_all" | "skip"
            )
            || self.rpo_ms.is_some_and(|value| value <= 0)
            || [
                &self.retention_policy_json,
                &self.verification_policy_json,
                &self.execution_policy_json,
            ]
            .into_iter()
            .any(|value| {
                value.len() > 64 * 1024
                    || serde_json::from_str::<serde_json::Value>(value)
                        .map(|document| !document.is_object())
                        .unwrap_or(true)
            })
            || self.content_hash_blake3.len() != 64
            || !self
                .content_hash_blake3
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredDatabaseSpec {
    engine_object_id: String,
    database_name: String,
    normalized_name: String,
    eligibility: String,
    exclusion_reason: Option<String>,
    attributes_json: String,
    fingerprint: String,
}

impl DiscoveredDatabaseSpec {
    pub fn eligible(
        engine_object_id: impl Into<String>,
        database_name: impl Into<String>,
        normalized_name: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            engine_object_id: engine_object_id.into(),
            database_name: database_name.into(),
            normalized_name: normalized_name.into(),
            eligibility: "eligible".to_owned(),
            exclusion_reason: None,
            attributes_json: "{}".to_owned(),
            fingerprint: fingerprint.into(),
        }
    }

    pub fn excluded(
        engine_object_id: impl Into<String>,
        database_name: impl Into<String>,
        normalized_name: impl Into<String>,
        exclusion_reason: impl Into<String>,
        fingerprint: impl Into<String>,
    ) -> Self {
        Self {
            engine_object_id: engine_object_id.into(),
            database_name: database_name.into(),
            normalized_name: normalized_name.into(),
            eligibility: "excluded".to_owned(),
            exclusion_reason: Some(exclusion_reason.into()),
            attributes_json: "{}".to_owned(),
            fingerprint: fingerprint.into(),
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.engine_object_id.is_empty()
            || self.database_name.is_empty()
            || self.normalized_name.is_empty()
            || self.fingerprint.is_empty()
            || !matches!(
                self.eligibility.as_str(),
                "eligible" | "excluded" | "offline" | "unknown"
            )
            || serde_json::from_str::<serde_json::Value>(&self.attributes_json).is_err()
            || (self.eligibility == "excluded" && self.exclusion_reason.is_none())
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct DiscoverySnapshotSpec {
    id: String,
    connection_profile_id: ConnectionProfileId,
    observations: Vec<DiscoveredDatabaseSpec>,
    started_at_ms: i64,
    completed_at_ms: i64,
}

impl DiscoverySnapshotSpec {
    pub fn successful(
        id: impl Into<String>,
        connection_profile_id: ConnectionProfileId,
        observations: Vec<DiscoveredDatabaseSpec>,
        started_at_ms: i64,
        completed_at_ms: i64,
    ) -> Self {
        Self {
            id: id.into(),
            connection_profile_id,
            observations,
            started_at_ms,
            completed_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if !is_canonical_uuid(&self.id)
            || self.started_at_ms < 0
            || self.completed_at_ms < self.started_at_ms
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let mut object_ids = BTreeSet::new();
        for observation in &self.observations {
            observation.validate()?;
            if !object_ids.insert(observation.engine_object_id.as_str()) {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoverySnapshotReport {
    added_count: usize,
    missing_count: usize,
    changed_count: usize,
}

impl DiscoverySnapshotReport {
    pub const fn added_count(&self) -> usize {
        self.added_count
    }

    pub const fn missing_count(&self) -> usize {
        self.missing_count
    }

    pub const fn changed_count(&self) -> usize {
        self.changed_count
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetDiscoveryStatus {
    state: String,
    missing_since_ms: Option<i64>,
    last_seen_discovery_run_id: Option<String>,
    source_fingerprint: String,
}

impl TargetDiscoveryStatus {
    pub fn state(&self) -> &str {
        &self.state
    }

    pub const fn missing_since_ms(&self) -> Option<i64> {
        self.missing_since_ms
    }

    pub fn last_seen_discovery_run_id(&self) -> Option<&str> {
        self.last_seen_discovery_run_id.as_deref()
    }

    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }
}

#[derive(Clone)]
pub struct ConfigurationRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn configuration(&self) -> ConfigurationRepository {
        ConfigurationRepository {
            store: self.clone(),
        }
    }
}

impl ConfigurationRepository {
    pub fn insert_connection(&self, spec: ConnectionProfileSpec) -> Result<(), RepositoryError> {
        self.insert_connection_with_database_auth_in_workspace(&default_workspace_id(), spec, None)
    }

    pub fn insert_connection_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        spec: ConnectionProfileSpec,
    ) -> Result<(), RepositoryError> {
        self.insert_connection_with_database_auth_in_workspace(workspace_id, spec, None)
    }

    pub fn insert_connection_with_database_auth(
        &self,
        spec: ConnectionProfileSpec,
        database_auth_secret_ref_id: Option<SecretRefId>,
    ) -> Result<(), RepositoryError> {
        self.insert_connection_with_database_auth_in_workspace(
            &default_workspace_id(),
            spec,
            database_auth_secret_ref_id,
        )
    }

    pub fn insert_connection_with_database_auth_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        spec: ConnectionProfileSpec,
        database_auth_secret_ref_id: Option<SecretRefId>,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        let workspace_id = workspace_id.as_str().to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure_active_workspace(&transaction, &workspace_id)?;
            ensure_workspace_resource_name_available(
                &transaction,
                "connection",
                &workspace_id,
                &spec.display_name,
            )?;
            if let Some(secret_ref_id) = database_auth_secret_ref_id.as_ref() {
                let valid: bool = transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM secret_refs
                         WHERE id = ?1
                           AND purpose = 'database_auth'
                           AND state = 'active'
                     )",
                    [secret_ref_id.as_str()],
                    |row| row.get(0),
                )?;
                if !valid {
                    return Err(RepositoryError::SecurityInputInvalid);
                }
            }
            let requested_display_name = spec.display_name.trim().to_owned();
            let insert_display_name =
                temporary_display_name(&workspace_id, spec.id.as_str(), &requested_display_name);
            transaction.execute(
                "INSERT INTO connection_profiles(
                     id, display_name, engine, transport, host, port, file_path,
                     tls_mode, adapter_config_json, state, source_resource_key,
                     created_at_ms, updated_at_ms
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5, ?6, ?7,
                     ?8, ?9, 'draft', ?10, ?11, ?11
                 )",
                params![
                    spec.id.as_str(),
                    insert_display_name,
                    spec.engine,
                    spec.transport,
                    spec.host,
                    spec.port,
                    spec.file_path,
                    spec.tls_mode,
                    spec.adapter_config_json,
                    spec.source_resource_key,
                    spec.created_at_ms
                ],
            )?;
            assign_connection_workspace(
                &transaction,
                &workspace_id,
                spec.id.as_str(),
                &requested_display_name,
            )?;
            if let Some(secret_ref_id) = database_auth_secret_ref_id {
                transaction.execute(
                    "INSERT INTO connection_secret_bindings(
                         connection_profile_id, secret_role, secret_ref_id, created_at_ms
                     ) VALUES (?1, 'database_auth', ?2, ?3)",
                    params![spec.id.as_str(), secret_ref_id.as_str(), spec.created_at_ms],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn connection_execution_binding(
        &self,
        connection_profile_id: &ConnectionProfileId,
    ) -> Result<ConnectionExecutionBinding, RepositoryError> {
        let reader = self.store.open_reader()?;
        let row = reader
            .connection()
            .query_row(
                "SELECT profile.id, profile.engine, profile.transport,
                        profile.host, profile.port, profile.file_path, profile.tls_mode,
                        binding.secret_ref_id
                 FROM connection_profiles AS profile
                 LEFT JOIN connection_secret_bindings AS binding
                   ON binding.connection_profile_id = profile.id
                  AND binding.secret_role = 'database_auth'
                 WHERE profile.id = ?1
                   AND profile.deleted_at_ms IS NULL",
                [connection_profile_id.as_str()],
                |row| {
                    let port = row
                        .get::<_, Option<i64>>(4)?
                        .map(u16::try_from)
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?;
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        port,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                    ))
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)?;
        Ok(ConnectionExecutionBinding {
            id: ConnectionProfileId::from_str(&row.0)?,
            engine: row.1,
            transport: row.2,
            host: row.3,
            port: row.4,
            file_path: row.5,
            tls_mode: row.6,
            database_auth_secret_ref_id: row
                .7
                .map(|value| SecretRefId::from_str(&value))
                .transpose()?,
        })
    }

    pub fn record_connection_probe(
        &self,
        spec: ConnectionProbeSpec,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let existing: Option<(String, String)> = transaction
                .query_row(
                    "SELECT connection_profile_id, status
                     FROM connection_probe_runs
                     WHERE id = ?1",
                    [spec.id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((connection_profile_id, status)) = existing {
                if connection_profile_id != spec.connection_profile_id.as_str()
                    || status != spec.status
                {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                transaction.commit()?;
                return Ok(());
            }
            transaction.execute(
                "INSERT INTO connection_probe_runs(
                     id, connection_profile_id, trigger_kind, status,
                     failure_class, server_version, tool_inventory_id,
                     certification_id, observed_capabilities_json,
                     redacted_diagnostics, started_at_ms, completed_at_ms,
                     created_at_ms
                 ) VALUES (
                     ?1, ?2, 'manual', ?3,
                     ?4, ?5, NULL,
                     NULL, ?6,
                     ?7, ?8, ?9,
                     ?8
                 )",
                params![
                    spec.id,
                    spec.connection_profile_id.as_str(),
                    spec.status,
                    spec.failure_class,
                    spec.server_version,
                    spec.observed_capabilities_json,
                    spec.redacted_diagnostics,
                    spec.started_at_ms,
                    spec.completed_at_ms
                ],
            )?;
            let affected = transaction.execute(
                "UPDATE connection_profiles
                 SET state = CASE
                         WHEN ?2 = 'passed' THEN 'ready'
                         WHEN state = 'ready' THEN 'attention'
                         ELSE state
                     END,
                     last_probe_status = ?2,
                     last_probe_at_ms = ?3,
                     updated_at_ms = MAX(updated_at_ms, ?3)
                 WHERE id = ?1
                   AND deleted_at_ms IS NULL",
                params![
                    spec.connection_profile_id.as_str(),
                    spec.status,
                    spec.completed_at_ms
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::NotFound);
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn insert_target(&self, spec: DatabaseTargetSpec) -> Result<(), RepositoryError> {
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO database_targets(
                     id, connection_profile_id, engine_object_id, display_name,
                     normalized_name, source_fingerprint, state, selected_at_ms,
                     created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7, ?7, ?7)",
                params![
                    spec.id.as_str(),
                    spec.connection_profile_id.as_str(),
                    spec.engine_object_id,
                    spec.display_name,
                    spec.normalized_name,
                    spec.source_fingerprint,
                    spec.selected_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn selected_database_target(
        &self,
        database_target_id: &DatabaseTargetId,
    ) -> Result<SelectedDatabaseTarget, RepositoryError> {
        let reader = self.store.open_reader()?;
        let row = reader
            .connection()
            .query_row(
                "SELECT id, connection_profile_id, engine_object_id, display_name,
                        normalized_name, source_fingerprint
                 FROM database_targets
                 WHERE id = ?1 AND state = 'active'",
                [database_target_id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)?;
        Ok(SelectedDatabaseTarget {
            id: DatabaseTargetId::from_str(&row.0)?,
            connection_profile_id: ConnectionProfileId::from_str(&row.1)?,
            engine_object_id: row.2,
            display_name: row.3,
            normalized_name: row.4,
            source_fingerprint: row.5,
        })
    }

    pub fn select_discovered_targets(
        &self,
        connection_profile_id: &ConnectionProfileId,
        discovery_run_id: &str,
        engine_object_ids: &[String],
        selected_at_ms: i64,
    ) -> Result<Vec<SelectedDatabaseTarget>, RepositoryError> {
        if discovery_run_id.is_empty()
            || discovery_run_id.len() > 128
            || engine_object_ids.is_empty()
            || engine_object_ids.len() > 1_000
            || engine_object_ids.iter().any(|value| {
                value.is_empty() || value.len() > 2_048 || value.contains(['\0', '\r', '\n'])
            })
            || engine_object_ids.iter().collect::<BTreeSet<_>>().len() != engine_object_ids.len()
            || selected_at_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let connection_profile_id = connection_profile_id.clone();
        let discovery_run_id = discovery_run_id.to_owned();
        let engine_object_ids = engine_object_ids.to_vec();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let discovery_valid: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM discovery_runs
                     WHERE id = ?1
                       AND connection_profile_id = ?2
                       AND status = 'succeeded'
                       AND completed_at_ms IS NOT NULL
                 )",
                params![discovery_run_id, connection_profile_id.as_str()],
                |row| row.get(0),
            )?;
            if !discovery_valid {
                return Err(RepositoryError::NotFound);
            }

            let mut selected = Vec::with_capacity(engine_object_ids.len());
            for engine_object_id in engine_object_ids {
                let observation: Option<(String, String, String)> = transaction
                    .query_row(
                        "SELECT database_name, normalized_name, fingerprint
                         FROM discovered_databases
                         WHERE discovery_run_id = ?1
                           AND connection_profile_id = ?2
                           AND engine_object_id = ?3
                           AND eligibility = 'eligible'",
                        params![
                            discovery_run_id,
                            connection_profile_id.as_str(),
                            engine_object_id
                        ],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let Some((display_name, normalized_name, source_fingerprint)) = observation else {
                    return Err(RepositoryError::NotFound);
                };
                let target_id = DatabaseTargetId::from_stable_name(
                    connection_profile_id.as_uuid(),
                    engine_object_id.as_bytes(),
                );
                transaction.execute(
                    "INSERT INTO database_targets(
                         id, connection_profile_id, engine_object_id, display_name,
                         normalized_name, source_fingerprint, state,
                         last_seen_discovery_run_id, selected_at_ms,
                         created_at_ms, updated_at_ms
                     ) VALUES (
                         ?1, ?2, ?3, ?4,
                         ?5, ?6, 'active',
                         ?7, ?8,
                         ?8, ?8
                     )
                     ON CONFLICT(connection_profile_id, engine_object_id) DO UPDATE SET
                         display_name = excluded.display_name,
                         normalized_name = excluded.normalized_name,
                         source_fingerprint = excluded.source_fingerprint,
                         state = CASE
                             WHEN database_targets.state = 'disabled' THEN 'disabled'
                             ELSE 'active'
                         END,
                         missing_since_ms = NULL,
                         last_seen_discovery_run_id = excluded.last_seen_discovery_run_id,
                         updated_at_ms = excluded.updated_at_ms",
                    params![
                        target_id.as_str(),
                        connection_profile_id.as_str(),
                        engine_object_id,
                        display_name,
                        normalized_name,
                        source_fingerprint,
                        discovery_run_id,
                        selected_at_ms
                    ],
                )?;
                let persisted_id: String = transaction.query_row(
                    "SELECT id FROM database_targets
                     WHERE connection_profile_id = ?1 AND engine_object_id = ?2",
                    params![connection_profile_id.as_str(), engine_object_id],
                    |row| row.get(0),
                )?;
                selected.push(SelectedDatabaseTarget {
                    id: DatabaseTargetId::from_str(&persisted_id).map_err(RepositoryError::from)?,
                    connection_profile_id: connection_profile_id.clone(),
                    engine_object_id,
                    display_name,
                    normalized_name,
                    source_fingerprint,
                });
            }
            transaction.execute(
                "UPDATE connection_profiles
                 SET state = 'ready', updated_at_ms = ?2
                 WHERE id = ?1 AND deleted_at_ms IS NULL",
                params![connection_profile_id.as_str(), selected_at_ms],
            )?;
            transaction.commit()?;
            Ok(selected)
        })
    }

    pub fn insert_destination(&self, spec: DestinationSpec) -> Result<(), RepositoryError> {
        self.insert_destination_with_secret_bindings_in_workspace(
            &default_workspace_id(),
            spec,
            Vec::new(),
        )
    }

    pub fn insert_destination_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        spec: DestinationSpec,
    ) -> Result<(), RepositoryError> {
        self.insert_destination_with_secret_bindings_in_workspace(workspace_id, spec, Vec::new())
    }

    pub fn insert_destination_with_secret_bindings(
        &self,
        spec: DestinationSpec,
        bindings: Vec<DestinationSecretBindingSpec>,
    ) -> Result<(), RepositoryError> {
        self.insert_destination_with_secret_bindings_in_workspace(
            &default_workspace_id(),
            spec,
            bindings,
        )
    }

    pub fn insert_destination_with_secret_bindings_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        spec: DestinationSpec,
        bindings: Vec<DestinationSecretBindingSpec>,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        if bindings.len() > 4
            || bindings
                .iter()
                .any(|binding| binding.destination_id != spec.id || binding.validate().is_err())
            || bindings
                .iter()
                .map(|binding| binding.role)
                .collect::<BTreeSet<_>>()
                .len()
                != bindings.len()
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let workspace_id = workspace_id.as_str().to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure_active_workspace(&transaction, &workspace_id)?;
            ensure_workspace_resource_name_available(
                &transaction,
                "destination",
                &workspace_id,
                &spec.display_name,
            )?;
            for binding in &bindings {
                let valid: bool = transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM secret_refs
                         WHERE id = ?1
                           AND purpose = ?2
                           AND state = 'active'
                     )",
                    params![
                        binding.secret_ref_id.as_str(),
                        destination_secret_purpose(binding.role)
                    ],
                    |row| row.get(0),
                )?;
                if !valid {
                    return Err(RepositoryError::SecurityInputInvalid);
                }
            }
            let requested_display_name = spec.display_name.trim().to_owned();
            let insert_display_name =
                temporary_display_name(&workspace_id, spec.id.as_str(), &requested_display_name);
            transaction.execute(
                "INSERT INTO destinations(
                     id, display_name, kind, endpoint_json, state,
                     declared_capabilities_json, observed_capabilities_json,
                     immutability_capability, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, 'draft', '{}', '{}', 'none', ?5, ?5)",
                params![
                    spec.id.as_str(),
                    insert_display_name,
                    spec.kind,
                    spec.endpoint_json,
                    spec.created_at_ms
                ],
            )?;
            assign_destination_workspace(
                &transaction,
                &workspace_id,
                spec.id.as_str(),
                &requested_display_name,
            )?;
            for binding in bindings {
                transaction.execute(
                    "INSERT INTO destination_secret_bindings(
                         destination_id, secret_role, secret_ref_id, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        binding.destination_id.as_str(),
                        binding.role.as_str(),
                        binding.secret_ref_id.as_str(),
                        binding.created_at_ms
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn bind_destination_secret(
        &self,
        spec: DestinationSecretBindingSpec,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute(
                "INSERT INTO destination_secret_bindings(
                     destination_id, secret_role, secret_ref_id, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    spec.destination_id.as_str(),
                    spec.role.as_str(),
                    spec.secret_ref_id.as_str(),
                    spec.created_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn destination_delete_binding(
        &self,
        fingerprint: &str,
    ) -> Result<DestinationDeleteBindingV2, RepositoryError> {
        let (expected_kind, destination_id) = fingerprint
            .split_once(':')
            .ok_or(RepositoryError::RuntimeStateInvalid)?;
        if !matches!(expected_kind, "local" | "s3" | "ssh_sftp") || fingerprint.len() > 512 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let destination_id =
            DestinationId::from_str(destination_id).map_err(RepositoryError::from)?;
        let reader = self.store.open_reader()?;
        type DestinationDeleteRow = (String, String, String, Option<i64>, Option<String>);
        let row: DestinationDeleteRow = reader
            .connection()
            .query_row(
                "SELECT destinations.kind, destinations.endpoint_json,
                        destinations.state, destinations.deleted_at_ms,
                        bindings.secret_ref_id
                 FROM destinations
                 LEFT JOIN destination_secret_bindings AS bindings
                   ON bindings.destination_id = destinations.id
                  AND bindings.secret_role = 'delete_operator'
                 WHERE destinations.id = ?1",
                [destination_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)?;
        let endpoint = serde_json::from_str::<serde_json::Value>(&row.1)
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        if row.0 != expected_kind
            || !matches!(row.2.as_str(), "ready" | "attention" | "disabled")
            || row.3.is_some()
            || row.1.len() > 64 * 1024
            || !endpoint.is_object()
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let delete_secret_ref_id = row
            .4
            .map(|value| SecretRefId::from_str(&value).map_err(RepositoryError::from))
            .transpose()?;
        Ok(DestinationDeleteBindingV2 {
            destination_id,
            fingerprint: fingerprint.to_owned(),
            kind: row.0,
            endpoint_json: row.1,
            delete_secret_ref_id,
        })
    }

    pub fn destination_probe_binding(
        &self,
        destination_id: &DestinationId,
    ) -> Result<DestinationProbeBinding, RepositoryError> {
        type Row = (
            String,
            String,
            String,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let reader = self.store.open_reader()?;
        let row: Row = reader
            .connection()
            .query_row(
                "SELECT destination.kind, destination.endpoint_json,
                        destination.state, destination.deleted_at_ms,
                        writer.secret_ref_id, reader.secret_ref_id,
                        deleter.secret_ref_id
                 FROM destinations AS destination
                 LEFT JOIN destination_secret_bindings AS writer
                   ON writer.destination_id = destination.id
                  AND writer.secret_role = 'backup_writer'
                 LEFT JOIN destination_secret_bindings AS reader
                   ON reader.destination_id = destination.id
                  AND reader.secret_role = 'restore_reader'
                 LEFT JOIN destination_secret_bindings AS deleter
                   ON deleter.destination_id = destination.id
                  AND deleter.secret_role = 'delete_operator'
                 WHERE destination.id = ?1",
                [destination_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)?;
        if !matches!(row.0.as_str(), "local" | "s3" | "ssh_sftp")
            || !matches!(row.2.as_str(), "draft" | "ready" | "attention" | "disabled")
            || row.3.is_some()
            || row.1.len() > 64 * 1024
            || serde_json::from_str::<serde_json::Value>(&row.1)
                .map(|value| !value.is_object())
                .unwrap_or(true)
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(DestinationProbeBinding {
            destination_id: destination_id.clone(),
            kind: row.0,
            endpoint_json: row.1,
            writer_secret_ref_id: row
                .4
                .map(|value| SecretRefId::from_str(&value))
                .transpose()?,
            reader_secret_ref_id: row
                .5
                .map(|value| SecretRefId::from_str(&value))
                .transpose()?,
            delete_secret_ref_id: row
                .6
                .map(|value| SecretRefId::from_str(&value))
                .transpose()?,
        })
    }

    pub fn backup_destination_bindings(
        &self,
        revision_id: &PlanRevisionId,
    ) -> Result<Vec<BackupDestinationBinding>, RepositoryError> {
        type Row = (
            String,
            String,
            String,
            String,
            String,
            Option<i64>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let reader = self.store.open_reader()?;
        let mut statement = reader.connection().prepare(
            "SELECT destination.id, membership.requirement,
                    destination.kind, destination.endpoint_json,
                    destination.state, destination.deleted_at_ms,
                    writer.secret_ref_id, reader.secret_ref_id,
                    deleter.secret_ref_id
             FROM plan_revision_destinations AS membership
             JOIN backup_plan_revisions AS revision
               ON revision.id = membership.plan_revision_id
             JOIN destinations AS destination
               ON destination.id = membership.destination_id
             LEFT JOIN destination_secret_bindings AS writer
               ON writer.destination_id = destination.id
              AND writer.secret_role = 'backup_writer'
             LEFT JOIN destination_secret_bindings AS reader
               ON reader.destination_id = destination.id
              AND reader.secret_role = 'restore_reader'
             LEFT JOIN destination_secret_bindings AS deleter
               ON deleter.destination_id = destination.id
              AND deleter.secret_role = 'delete_operator'
             WHERE membership.plan_revision_id = ?1
               AND revision.published_at_ms IS NOT NULL
             ORDER BY destination.id",
        )?;
        let rows = statement
            .query_map([revision_id.as_str()], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            })?
            .collect::<Result<Vec<Row>, _>>()?;
        if rows.is_empty() {
            return Err(RepositoryError::NotFound);
        }
        rows.into_iter()
            .map(|row| {
                if !matches!(row.2.as_str(), "local" | "s3" | "ssh_sftp")
                    || !matches!(row.4.as_str(), "draft" | "ready" | "attention" | "disabled")
                    || row.5.is_some()
                    || row.3.len() > 64 * 1024
                    || serde_json::from_str::<serde_json::Value>(&row.3)
                        .map(|value| !value.is_object())
                        .unwrap_or(true)
                {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                Ok(BackupDestinationBinding {
                    destination: DestinationProbeBinding {
                        destination_id: DestinationId::from_str(&row.0)?,
                        kind: row.2,
                        endpoint_json: row.3,
                        writer_secret_ref_id: row
                            .6
                            .map(|value| SecretRefId::from_str(&value))
                            .transpose()?,
                        reader_secret_ref_id: row
                            .7
                            .map(|value| SecretRefId::from_str(&value))
                            .transpose()?,
                        delete_secret_ref_id: row
                            .8
                            .map(|value| SecretRefId::from_str(&value))
                            .transpose()?,
                    },
                    requirement: DestinationRequirement::from_str(&row.1)?,
                    state: row.4,
                })
            })
            .collect()
    }

    pub fn record_destination_probes(
        &self,
        probes: Vec<DestinationProbeSpec>,
    ) -> Result<(), RepositoryError> {
        if probes.is_empty() || probes.len() > 3 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let destination_id = probes[0].destination_id.clone();
        let mut roles = BTreeSet::new();
        for probe in &probes {
            probe.validate()?;
            if probe.destination_id != destination_id || !roles.insert(probe.role) {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
        }
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for probe in &probes {
                let existing: Option<(String, String)> = transaction
                    .query_row(
                        "SELECT destination_id, status
                         FROM destination_probe_runs
                         WHERE id = ?1",
                        [probe.id.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((existing_destination_id, status)) = existing {
                    if existing_destination_id != destination_id.as_str() || status != probe.status
                    {
                        return Err(RepositoryError::RuntimeStateInvalid);
                    }
                    continue;
                }
                transaction.execute(
                    "INSERT INTO destination_probe_runs(
                         id, destination_id, credential_role, status,
                         observed_capabilities_json, free_bytes, latency_ms,
                         failure_class, redacted_diagnostics,
                         started_at_ms, completed_at_ms, created_at_ms
                     ) VALUES (
                         ?1, ?2, ?3, ?4,
                         ?5, ?6, ?7,
                         ?8, ?9,
                         ?10, ?11, ?10
                     )",
                    params![
                        probe.id,
                        destination_id.as_str(),
                        probe.role.as_str(),
                        probe.status,
                        probe.observed_capabilities_json,
                        probe.free_bytes,
                        probe.latency_ms,
                        probe.failure_class,
                        probe.redacted_diagnostics,
                        probe.started_at_ms,
                        probe.completed_at_ms
                    ],
                )?;
            }
            let latest = |role: &str| -> Result<Option<String>, rusqlite::Error> {
                transaction
                    .query_row(
                        "SELECT status
                         FROM destination_probe_runs
                         WHERE destination_id = ?1 AND credential_role = ?2
                         ORDER BY started_at_ms DESC, id DESC
                         LIMIT 1",
                        params![destination_id.as_str(), role],
                        |row| row.get(0),
                    )
                    .optional()
            };
            let writer = latest("backup_writer")?;
            let reader = latest("restore_reader")?;
            let aggregate = if writer.as_deref() == Some("passed")
                && reader.as_deref() == Some("passed")
            {
                "passed"
            } else if writer.as_deref() == Some("failed") || reader.as_deref() == Some("failed") {
                "failed"
            } else {
                "running"
            };
            let latest_at_ms = probes
                .iter()
                .map(|probe| probe.completed_at_ms)
                .max()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let immutability = probes
                .iter()
                .find(|probe| probe.role == DestinationSecretRoleV2::BackupWriter)
                .and_then(|probe| {
                    serde_json::from_str::<serde_json::Value>(&probe.observed_capabilities_json)
                        .ok()
                })
                .and_then(|value| {
                    value
                        .get("immutability")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| "none".to_owned());
            if !matches!(
                immutability.as_str(),
                "none" | "governance" | "compliance" | "server_append_only"
            ) {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let affected = transaction.execute(
                "UPDATE destinations
                 SET state = CASE
                         WHEN ?2 = 'passed' THEN 'ready'
                         WHEN ?2 = 'failed' THEN 'attention'
                         ELSE state
                     END,
                     immutability_capability = ?3,
                     last_probe_status = ?2,
                     last_probe_at_ms = ?4,
                     updated_at_ms = MAX(updated_at_ms, ?4)
                 WHERE id = ?1
                   AND deleted_at_ms IS NULL",
                params![
                    destination_id.as_str(),
                    aggregate,
                    immutability,
                    latest_at_ms
                ],
            )?;
            if affected != 1 {
                return Err(RepositoryError::NotFound);
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn destination_repair_binding(
        &self,
        fingerprint: &str,
    ) -> Result<DestinationRepairBindingV2, RepositoryError> {
        let (expected_kind, destination_id) = parse_destination_fingerprint(fingerprint)?;
        let reader = self.store.open_reader()?;
        load_destination_repair_binding(
            reader.connection(),
            &destination_id,
            expected_kind,
            fingerprint,
        )
    }

    pub fn replica_repair_secret_refs(
        &self,
        source_fingerprint: &str,
        target_fingerprint: &str,
    ) -> Result<ReplicaRepairSecretRefsV2, RepositoryError> {
        let source_fingerprint = source_fingerprint.to_owned();
        let target_fingerprint = target_fingerprint.to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let result = resolve_replica_repair_secret_refs_in_transaction(
                &transaction,
                &source_fingerprint,
                &target_fingerprint,
            )?;
            transaction.commit()?;
            Ok(result)
        })
    }

    pub fn insert_plan_draft(&self, spec: PlanDraftSpec) -> Result<(), RepositoryError> {
        self.insert_plan_draft_in_workspace(&default_workspace_id(), spec)
    }

    pub fn insert_plan_draft_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        spec: PlanDraftSpec,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        let workspace_id = workspace_id.as_str().to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            insert_plan_draft_in_workspace(&transaction, &workspace_id, &spec)?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn insert_plan_draft_with_membership(
        &self,
        spec: PlanDraftSpec,
        target_ids: Vec<DatabaseTargetId>,
        destinations: Vec<(DestinationId, DestinationRequirement)>,
    ) -> Result<(), RepositoryError> {
        self.insert_plan_draft_with_membership_in_workspace(
            &default_workspace_id(),
            spec,
            target_ids,
            destinations,
        )
    }

    pub fn insert_plan_draft_with_membership_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        spec: PlanDraftSpec,
        target_ids: Vec<DatabaseTargetId>,
        destinations: Vec<(DestinationId, DestinationRequirement)>,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        if target_ids.is_empty()
            || target_ids.len() > 1_000
            || target_ids.iter().collect::<BTreeSet<_>>().len() != target_ids.len()
            || destinations.is_empty()
            || destinations.len() > 128
            || destinations
                .iter()
                .map(|(id, _)| id)
                .collect::<BTreeSet<_>>()
                .len()
                != destinations.len()
            || !destinations
                .iter()
                .any(|(_, requirement)| *requirement == DestinationRequirement::Required)
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let workspace_id = workspace_id.as_str().to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            insert_plan_draft_in_workspace(&transaction, &workspace_id, &spec)?;
            for target_id in target_ids {
                transaction.execute(
                    "INSERT INTO plan_revision_targets(
                         plan_revision_id, database_target_id, target_options_json, created_at_ms
                     ) VALUES (?1, ?2, '{}', ?3)",
                    params![
                        spec.revision_id.as_str(),
                        target_id.as_str(),
                        spec.created_at_ms
                    ],
                )?;
            }
            for (destination_id, requirement) in destinations {
                transaction.execute(
                    "INSERT INTO plan_revision_destinations(
                         plan_revision_id, destination_id, requirement, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        spec.revision_id.as_str(),
                        destination_id.as_str(),
                        requirement.as_str(),
                        spec.created_at_ms
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn insert_plan_draft_with_scope_membership_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
        spec: PlanDraftSpec,
        scope_ids: Vec<BackupScopeId>,
        destinations: Vec<(DestinationId, DestinationRequirement)>,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        if scope_ids.is_empty()
            || scope_ids.len() > 1_000
            || scope_ids.iter().collect::<BTreeSet<_>>().len() != scope_ids.len()
            || destinations.is_empty()
            || destinations.len() > 128
            || destinations
                .iter()
                .map(|(id, _)| id)
                .collect::<BTreeSet<_>>()
                .len()
                != destinations.len()
            || !destinations
                .iter()
                .any(|(_, requirement)| *requirement == DestinationRequirement::Required)
        {
            return Err(RepositoryError::BackupScopeInputInvalid);
        }
        let workspace_id = workspace_id.as_str().to_owned();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            insert_plan_draft_in_workspace(&transaction, &workspace_id, &spec)?;
            let mut target_ids = BTreeSet::new();
            for scope_id in scope_ids {
                let scope: Option<(String, String, String, String, i64, String, String)> =
                    transaction
                        .query_row(
                            "SELECT scope.database_target_id, scope.mode,
                                    scope.source_fingerprint, scope.database_source_fingerprint,
                                    scope.resolved_object_count, scope.selector_json,
                                    scope.selector_hash_blake3
                             FROM backup_scopes AS scope
                             JOIN database_targets AS target ON target.id = scope.database_target_id
                             JOIN workspace_connection_profiles AS ownership
                               ON ownership.connection_profile_id = target.connection_profile_id
                              AND ownership.workspace_id = ?2
                             WHERE scope.id = ?1
                               AND scope.state = 'active'
                               AND target.state = 'active'
                               AND target.source_fingerprint = scope.database_source_fingerprint",
                            params![scope_id.as_str(), workspace_id],
                            |row| {
                                Ok((
                                    row.get(0)?,
                                    row.get(1)?,
                                    row.get(2)?,
                                    row.get(3)?,
                                    row.get(4)?,
                                    row.get(5)?,
                                    row.get(6)?,
                                ))
                            },
                        )
                        .optional()?;
                let Some((
                    target_id,
                    mode,
                    source_fingerprint,
                    database_source_fingerprint,
                    resolved_object_count,
                    selector_json,
                    selector_hash,
                )) = scope
                else {
                    return Err(RepositoryError::BackupScopeDiscoveryInvalid);
                };
                if !target_ids.insert(target_id.clone()) {
                    return Err(RepositoryError::BackupScopeConflict);
                }
                transaction.execute(
                    "INSERT INTO plan_revision_targets(
                         plan_revision_id, database_target_id, target_options_json, created_at_ms
                     ) VALUES (?1, ?2, '{}', ?3)",
                    params![spec.revision_id.as_str(), target_id, spec.created_at_ms],
                )?;
                transaction.execute(
                    "DELETE FROM plan_revision_scopes
                     WHERE plan_revision_id = ?1 AND database_target_id = ?2",
                    params![spec.revision_id.as_str(), target_id],
                )?;
                transaction.execute(
                    "INSERT INTO plan_revision_scopes(
                         plan_revision_id, database_target_id, backup_scope_id,
                         scope_mode, source_fingerprint, database_source_fingerprint,
                         resolved_object_count, scope_snapshot_json,
                         scope_snapshot_hash_blake3, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        spec.revision_id.as_str(),
                        target_id,
                        scope_id.as_str(),
                        mode,
                        source_fingerprint,
                        database_source_fingerprint,
                        resolved_object_count,
                        selector_json,
                        selector_hash,
                        spec.created_at_ms
                    ],
                )?;
            }
            for (destination_id, requirement) in destinations {
                transaction.execute(
                    "INSERT INTO plan_revision_destinations(
                         plan_revision_id, destination_id, requirement, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        spec.revision_id.as_str(),
                        destination_id.as_str(),
                        requirement.as_str(),
                        spec.created_at_ms
                    ],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn add_revision_target(
        &self,
        revision_id: &PlanRevisionId,
        target_id: &DatabaseTargetId,
        created_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        let revision_id = revision_id.clone();
        let target_id = target_id.clone();
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            ensure_revision_draft(&transaction, revision_id.as_str())?;
            transaction.execute(
                "INSERT INTO plan_revision_targets(
                     plan_revision_id, database_target_id, target_options_json, created_at_ms
                 ) VALUES (?1, ?2, '{}', ?3)",
                params![revision_id.as_str(), target_id.as_str(), created_at_ms],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn add_revision_destination(
        &self,
        revision_id: &PlanRevisionId,
        destination_id: &DestinationId,
        requirement: DestinationRequirement,
        created_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        let revision_id = revision_id.clone();
        let destination_id = destination_id.clone();
        self.write(move |connection| {
            let transaction = connection.transaction()?;
            ensure_revision_draft(&transaction, revision_id.as_str())?;
            transaction.execute(
                "INSERT INTO plan_revision_destinations(
                     plan_revision_id, destination_id, requirement, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    revision_id.as_str(),
                    destination_id.as_str(),
                    requirement.as_str(),
                    created_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn publish_revision(
        &self,
        revision_id: &PlanRevisionId,
        published_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        self.publish_revision_internal(None, revision_id, published_at_ms)
    }

    pub fn publish_plan_revision(
        &self,
        plan_id: &BackupPlanId,
        revision_id: &PlanRevisionId,
        published_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        self.publish_revision_internal(Some(plan_id), revision_id, published_at_ms)
    }

    fn publish_revision_internal(
        &self,
        expected_plan_id: Option<&BackupPlanId>,
        revision_id: &PlanRevisionId,
        published_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if published_at_ms < 0 {
            return Err(DomainError::PlanRevisionPublishedAtInvalid.into());
        }
        let expected_plan_id = expected_plan_id.cloned();
        let revision_id = revision_id.clone();
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            ensure_revision_draft(&transaction, revision_id.as_str())?;
            let target_count: i64 = transaction.query_row(
                "SELECT COUNT(*)
                 FROM plan_revision_targets
                 WHERE plan_revision_id = ?1",
                [revision_id.as_str()],
                |row| row.get(0),
            )?;
            if target_count == 0 {
                return Err(DomainError::PlanRevisionTargetRequired.into());
            }
            let verification_policy_json: String = transaction.query_row(
                "SELECT verification_policy_json
                 FROM backup_plan_revisions
                 WHERE id = ?1",
                [revision_id.as_str()],
                |row| row.get(0),
            )?;
            let requested_verification_level =
                VerificationPolicy::from_json(&verification_policy_json)
                    .ok()
                    .map(|policy| match policy.requested_level() {
                        RequestedVerificationLevel::L1 => 1,
                        RequestedVerificationLevel::L2 => 2,
                        RequestedVerificationLevel::L3 => 3,
                    });
            if requested_verification_level == Some(2) {
                let unsupported_target_exists: bool = transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1
                         FROM plan_revision_targets AS membership
                         JOIN database_targets AS target
                           ON target.id = membership.database_target_id
                         JOIN connection_profiles AS connection
                           ON connection.id = target.connection_profile_id
                         WHERE membership.plan_revision_id = ?1
                           AND connection.engine NOT IN ('postgresql', 'sqlite')
                     )",
                    [revision_id.as_str()],
                    |row| row.get(0),
                )?;
                if unsupported_target_exists {
                    return Err(DomainError::PlanRevisionVerificationUnsupported.into());
                }
            }
            let (destination_count, required_count): (i64, i64) = transaction.query_row(
                "SELECT COUNT(*), COALESCE(SUM(requirement = 'required'), 0)
                 FROM plan_revision_destinations
                 WHERE plan_revision_id = ?1",
                [revision_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if destination_count == 0 {
                return Err(DomainError::PlanRevisionDestinationRequired.into());
            }
            if required_count == 0 {
                return Err(DomainError::PlanRevisionRequiredDestinationRequired.into());
            }
            let plan_id: String = transaction.query_row(
                "SELECT backup_plan_id
                 FROM backup_plan_revisions
                 WHERE id = ?1",
                [revision_id.as_str()],
                |row| row.get(0),
            )?;
            if expected_plan_id
                .as_ref()
                .is_some_and(|expected| expected.as_str() != plan_id)
            {
                return Err(RepositoryError::NotFound);
            }
            if expected_plan_id.is_some() {
                ensure_plan_runtime_readiness(
                    &transaction,
                    revision_id.as_str(),
                    requested_verification_level,
                    published_at_ms,
                )?;
            }
            transaction.execute(
                "UPDATE backup_plan_revisions
                 SET published_at_ms = ?2
                 WHERE id = ?1 AND published_at_ms IS NULL",
                params![revision_id.as_str(), published_at_ms],
            )?;
            transaction.execute(
                "UPDATE backup_plans
                 SET current_revision_id = ?2, state = 'enabled', updated_at_ms = ?3
                 WHERE id = ?1",
                params![plan_id, revision_id.as_str(), published_at_ms],
            )?;
            transaction.commit()?;
            Ok(())
        })
    }

    pub fn record_discovery_snapshot(
        &self,
        spec: DiscoverySnapshotSpec,
    ) -> Result<DiscoverySnapshotReport, RepositoryError> {
        spec.validate()?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let connection_exists: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM connection_profiles
                     WHERE id = ?1 AND deleted_at_ms IS NULL
                 )",
                [spec.connection_profile_id.as_str()],
                |row| row.get(0),
            )?;
            if !connection_exists {
                return Err(RepositoryError::NotFound);
            }
            let existing: Option<(String, String, i64, i64, i64)> = transaction
                .query_row(
                    "SELECT connection_profile_id, status,
                            added_count, missing_count, changed_count
                     FROM discovery_runs
                     WHERE id = ?1",
                    [spec.id.as_str()],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            if let Some((connection_profile_id, status, added, missing, changed)) = existing {
                if connection_profile_id != spec.connection_profile_id.as_str()
                    || status != "succeeded"
                {
                    return Err(RepositoryError::RuntimeStateInvalid);
                }
                transaction.commit()?;
                return Ok(DiscoverySnapshotReport {
                    added_count: usize::try_from(added)
                        .map_err(|_| RepositoryError::RuntimeStateInvalid)?,
                    missing_count: usize::try_from(missing)
                        .map_err(|_| RepositoryError::RuntimeStateInvalid)?,
                    changed_count: usize::try_from(changed)
                        .map_err(|_| RepositoryError::RuntimeStateInvalid)?,
                });
            }
            transaction.execute(
                "INSERT INTO discovery_runs(
                     id, connection_profile_id, probe_run_id, status,
                     added_count, missing_count, changed_count, error_code,
                     started_at_ms, completed_at_ms, created_at_ms
                 ) VALUES (
                     ?1, ?2, NULL, 'running',
                     0, 0, 0, NULL,
                     ?3, NULL, ?3
                 )",
                params![
                    spec.id,
                    spec.connection_profile_id.as_str(),
                    spec.started_at_ms
                ],
            )?;
            for observation in &spec.observations {
                let observation_id =
                    stable_discovery_observation_id(&spec.id, &observation.engine_object_id);
                transaction.execute(
                    "INSERT INTO discovered_databases(
                         id, discovery_run_id, connection_profile_id,
                         engine_object_id, database_name, normalized_name,
                         eligibility, exclusion_reason, attributes_json,
                         fingerprint, observed_at_ms, created_at_ms
                     ) VALUES (
                         ?1, ?2, ?3,
                         ?4, ?5, ?6,
                         ?7, ?8, ?9,
                         ?10, ?11, ?11
                     )",
                    params![
                        observation_id,
                        spec.id,
                        spec.connection_profile_id.as_str(),
                        observation.engine_object_id,
                        observation.database_name,
                        observation.normalized_name,
                        observation.eligibility,
                        observation.exclusion_reason,
                        observation.attributes_json,
                        observation.fingerprint,
                        spec.completed_at_ms
                    ],
                )?;
            }

            let mut statement = transaction.prepare(
                "SELECT id, engine_object_id, normalized_name, source_fingerprint, state
                 FROM database_targets
                 WHERE connection_profile_id = ?1
                   AND retired_at_ms IS NULL
                 ORDER BY engine_object_id, id",
            )?;
            let existing_targets = statement
                .query_map([spec.connection_profile_id.as_str()], |row| {
                    Ok(ExistingTarget {
                        id: row.get(0)?,
                        engine_object_id: row.get(1)?,
                        normalized_name: row.get(2)?,
                        source_fingerprint: row.get(3)?,
                        state: row.get(4)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);

            let observation_by_object = spec
                .observations
                .iter()
                .map(|observation| (observation.engine_object_id.as_str(), observation))
                .collect::<BTreeMap<_, _>>();
            let target_object_ids = existing_targets
                .iter()
                .map(|target| target.engine_object_id.as_str())
                .collect::<BTreeSet<_>>();
            let added_count = observation_by_object
                .keys()
                .filter(|object_id| !target_object_ids.contains(**object_id))
                .count();
            let mut missing_count = 0_usize;
            let mut changed_count = 0_usize;

            for target in &existing_targets {
                if let Some(observation) =
                    observation_by_object.get(target.engine_object_id.as_str())
                {
                    let changed = target.normalized_name != observation.normalized_name
                        || target.source_fingerprint != observation.fingerprint;
                    if changed {
                        changed_count += 1;
                    }
                    let state = match target.state.as_str() {
                        "disabled" | "retired" => target.state.as_str(),
                        _ if changed => "attention",
                        _ => "active",
                    };
                    transaction.execute(
                        "UPDATE database_targets
                         SET state = ?2, missing_since_ms = NULL,
                             last_seen_discovery_run_id = ?3, updated_at_ms = ?4
                         WHERE id = ?1",
                        params![target.id, state, spec.id, spec.completed_at_ms],
                    )?;
                } else {
                    missing_count += 1;
                    transaction.execute(
                        "UPDATE database_targets
                         SET state = CASE
                                 WHEN state IN ('active', 'attention') THEN 'attention'
                                 ELSE state
                             END,
                             missing_since_ms = COALESCE(missing_since_ms, ?2),
                             updated_at_ms = ?2
                         WHERE id = ?1",
                        params![target.id, spec.completed_at_ms],
                    )?;
                }
            }

            transaction.execute(
                "UPDATE discovery_runs
                 SET status = 'succeeded', added_count = ?2, missing_count = ?3,
                     changed_count = ?4, completed_at_ms = ?5
                 WHERE id = ?1 AND status = 'running'",
                params![
                    spec.id,
                    added_count,
                    missing_count,
                    changed_count,
                    spec.completed_at_ms
                ],
            )?;
            transaction.commit()?;
            Ok(DiscoverySnapshotReport {
                added_count,
                missing_count,
                changed_count,
            })
        })
    }

    pub fn target_count(
        &self,
        connection_id: &ConnectionProfileId,
    ) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT COUNT(*)
                 FROM database_targets
                 WHERE connection_profile_id = ?1",
                [connection_id.as_str()],
                |row| row.get(0),
            )
            .map_err(RepositoryError::from)
    }

    pub fn current_revision(
        &self,
        plan_id: &BackupPlanId,
    ) -> Result<Option<PlanRevisionId>, RepositoryError> {
        let reader = self.store.open_reader()?;
        let value: Option<String> = reader
            .connection()
            .query_row(
                "SELECT current_revision_id
                 FROM backup_plans
                 WHERE id = ?1",
                [plan_id.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        value
            .map(|raw| PlanRevisionId::from_str(&raw).map_err(RepositoryError::from))
            .transpose()
    }

    pub fn target_discovery_status(
        &self,
        target_id: &DatabaseTargetId,
    ) -> Result<TargetDiscoveryStatus, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT
                     state, missing_since_ms, last_seen_discovery_run_id,
                     source_fingerprint
                 FROM database_targets
                 WHERE id = ?1",
                [target_id.as_str()],
                |row| {
                    Ok(TargetDiscoveryStatus {
                        state: row.get(0)?,
                        missing_since_ms: row.get(1)?,
                        last_seen_discovery_run_id: row.get(2)?,
                        source_fingerprint: row.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

fn ensure_plan_runtime_readiness(
    transaction: &rusqlite::Transaction<'_>,
    revision_id: &str,
    requested_verification_level: Option<i64>,
    observed_at_ms: i64,
) -> Result<(), RepositoryError> {
    let target_not_ready: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM plan_revision_targets AS membership
             JOIN database_targets AS target
               ON target.id = membership.database_target_id
             JOIN connection_profiles AS connection
               ON connection.id = target.connection_profile_id
             WHERE membership.plan_revision_id = ?1
               AND (
                   target.state <> 'active'
                   OR target.retired_at_ms IS NOT NULL
                   OR connection.state <> 'ready'
                   OR connection.deleted_at_ms IS NOT NULL
                   OR connection.last_probe_status <> 'passed'
                   OR connection.last_probe_at_ms IS NULL
               )
         )",
        [revision_id],
        |row| row.get(0),
    )?;
    if target_not_ready {
        return Err(DomainError::PlanRevisionTargetNotReady.into());
    }

    let required_verification_capability = match requested_verification_level {
        Some(1) => Some("verify_l1"),
        Some(2) => Some("verify_l2"),
        Some(3) => Some("restore_l3"),
        None => None,
        Some(_) => return Err(DomainError::PlanRevisionVerificationUnsupported.into()),
    };
    let engine_capability_missing: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM (
                 SELECT DISTINCT connection.engine AS engine
                 FROM plan_revision_targets AS membership
                 JOIN database_targets AS target
                   ON target.id = membership.database_target_id
                 JOIN connection_profiles AS connection
                   ON connection.id = target.connection_profile_id
                 WHERE membership.plan_revision_id = ?1
             ) AS required_engine
             WHERE NOT EXISTS (
                 SELECT 1
                 FROM engine_certifications AS certification
                 JOIN engine_certification_capabilities AS backup
                   ON backup.certification_id = certification.id
                  AND backup.capability = 'backup'
                  AND backup.support_status = 'passed'
                 WHERE certification.engine = required_engine.engine
                   AND certification.status = 'certified'
                   AND certification.certified_at_ms <= ?2
                   AND (
                       certification.expires_at_ms IS NULL
                       OR certification.expires_at_ms > ?2
                   )
                   AND (
                       ?3 IS NULL
                       OR EXISTS (
                           SELECT 1
                           FROM engine_certification_capabilities AS verification
                           WHERE verification.certification_id = certification.id
                             AND verification.capability = ?3
                             AND verification.support_status = 'passed'
                       )
                   )
             )
         )",
        params![
            revision_id,
            observed_at_ms,
            required_verification_capability
        ],
        |row| row.get(0),
    )?;
    if engine_capability_missing {
        return Err(DomainError::PlanRevisionEngineCapabilityUnavailable.into());
    }

    let destination_not_ready: bool = transaction.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM plan_revision_destinations AS membership
             JOIN destinations AS destination
               ON destination.id = membership.destination_id
             WHERE membership.plan_revision_id = ?1
               AND (
                   destination.state <> 'ready'
                   OR destination.deleted_at_ms IS NOT NULL
                   OR destination.last_probe_status <> 'passed'
                   OR destination.last_probe_at_ms IS NULL
                   OR NOT EXISTS (
                       SELECT 1
                       FROM destination_probe_runs AS writer
                       WHERE writer.destination_id = destination.id
                         AND writer.credential_role = 'backup_writer'
                         AND writer.status = 'passed'
                         AND NOT EXISTS (
                             SELECT 1
                             FROM destination_probe_runs AS newer
                             WHERE newer.destination_id = writer.destination_id
                               AND newer.credential_role = writer.credential_role
                               AND (
                                   newer.started_at_ms > writer.started_at_ms
                                   OR (
                                       newer.started_at_ms = writer.started_at_ms
                                       AND newer.id > writer.id
                                   )
                               )
                         )
                   )
                   OR NOT EXISTS (
                       SELECT 1
                       FROM destination_probe_runs AS reader
                       WHERE reader.destination_id = destination.id
                         AND reader.credential_role = 'restore_reader'
                         AND reader.status = 'passed'
                         AND NOT EXISTS (
                             SELECT 1
                             FROM destination_probe_runs AS newer
                             WHERE newer.destination_id = reader.destination_id
                               AND newer.credential_role = reader.credential_role
                               AND (
                                   newer.started_at_ms > reader.started_at_ms
                                   OR (
                                       newer.started_at_ms = reader.started_at_ms
                                       AND newer.id > reader.id
                                   )
                               )
                         )
                   )
               )
         )",
        [revision_id],
        |row| row.get(0),
    )?;
    if destination_not_ready {
        return Err(DomainError::PlanRevisionDestinationNotReady.into());
    }
    Ok(())
}

struct ExistingTarget {
    id: String,
    engine_object_id: String,
    normalized_name: String,
    source_fingerprint: String,
    state: String,
}

fn stable_discovery_observation_id(discovery_run_id: &str, engine_object_id: &str) -> String {
    const DISCOVERY_NAMESPACE: Uuid = Uuid::from_u128(0x930e_9bc4_77e6_5e3f_8d74_f80c_9724_c35c);
    Uuid::new_v5(
        &DISCOVERY_NAMESPACE,
        format!("{discovery_run_id}|{engine_object_id}").as_bytes(),
    )
    .to_string()
}

fn is_canonical_uuid(value: &str) -> bool {
    Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
}

fn parse_destination_fingerprint(
    fingerprint: &str,
) -> Result<(&str, DestinationId), RepositoryError> {
    let (kind, destination_id) = fingerprint
        .split_once(':')
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    if !matches!(kind, "local" | "s3" | "ssh_sftp") || fingerprint.len() > 512 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let destination_id = DestinationId::from_str(destination_id).map_err(RepositoryError::from)?;
    Ok((kind, destination_id))
}

const fn destination_secret_purpose(role: DestinationSecretRoleV2) -> &'static str {
    match role {
        DestinationSecretRoleV2::BackupWriter => "backup_writer",
        DestinationSecretRoleV2::RestoreReader => "restore_reader",
        DestinationSecretRoleV2::DeleteOperator => "retention_delete",
        DestinationSecretRoleV2::HostAuth => "tunnel_auth",
    }
}

fn load_destination_repair_binding(
    connection: &rusqlite::Connection,
    destination_id: &DestinationId,
    expected_kind: &str,
    fingerprint: &str,
) -> Result<DestinationRepairBindingV2, RepositoryError> {
    type DestinationRepairRow = (
        String,
        String,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
    );
    let row: DestinationRepairRow = connection
        .query_row(
            "SELECT destinations.kind, destinations.endpoint_json,
                    destinations.state, destinations.deleted_at_ms,
                    writer.secret_ref_id, reader.secret_ref_id
             FROM destinations
             LEFT JOIN destination_secret_bindings AS writer
               ON writer.destination_id = destinations.id
              AND writer.secret_role = 'backup_writer'
             LEFT JOIN destination_secret_bindings AS reader
               ON reader.destination_id = destinations.id
              AND reader.secret_role = 'restore_reader'
             WHERE destinations.id = ?1",
            [destination_id.as_str()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?
        .ok_or(RepositoryError::NotFound)?;
    let endpoint = serde_json::from_str::<serde_json::Value>(&row.1)
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if row.0 != expected_kind
        || !matches!(row.2.as_str(), "ready" | "attention" | "disabled")
        || row.3.is_some()
        || row.1.len() > 64 * 1024
        || !endpoint.is_object()
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let writer_secret_ref_id = row
        .4
        .map(|value| SecretRefId::from_str(&value).map_err(RepositoryError::from))
        .transpose()?;
    let reader_secret_ref_id = row
        .5
        .map(|value| SecretRefId::from_str(&value).map_err(RepositoryError::from))
        .transpose()?;
    Ok(DestinationRepairBindingV2 {
        destination_id: destination_id.clone(),
        fingerprint: fingerprint.to_owned(),
        kind: row.0,
        endpoint_json: row.1,
        writer_secret_ref_id,
        reader_secret_ref_id,
    })
}

pub(super) fn resolve_replica_repair_secret_refs_in_transaction(
    transaction: &rusqlite::Transaction<'_>,
    source_fingerprint: &str,
    target_fingerprint: &str,
) -> Result<ReplicaRepairSecretRefsV2, RepositoryError> {
    if source_fingerprint == target_fingerprint {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let (source_kind, source_id) = parse_destination_fingerprint(source_fingerprint)?;
    let (target_kind, target_id) = parse_destination_fingerprint(target_fingerprint)?;
    let source =
        load_destination_repair_binding(transaction, &source_id, source_kind, source_fingerprint)?;
    let target =
        load_destination_repair_binding(transaction, &target_id, target_kind, target_fingerprint)?;
    let source_reader_secret_ref_id =
        required_repair_secret(&source, DestinationSecretRoleV2::RestoreReader)?;
    let target_writer_secret_ref_id =
        required_repair_secret(&target, DestinationSecretRoleV2::BackupWriter)?;
    let target_reader_secret_ref_id =
        required_repair_secret(&target, DestinationSecretRoleV2::RestoreReader)?;
    validate_active_repair_secret(
        transaction,
        source_reader_secret_ref_id.as_ref(),
        "restore_reader",
    )?;
    validate_active_repair_secret(
        transaction,
        target_writer_secret_ref_id.as_ref(),
        "backup_writer",
    )?;
    validate_active_repair_secret(
        transaction,
        target_reader_secret_ref_id.as_ref(),
        "restore_reader",
    )?;
    Ok(ReplicaRepairSecretRefsV2 {
        source_reader_secret_ref_id,
        target_writer_secret_ref_id,
        target_reader_secret_ref_id,
    })
}

fn required_repair_secret(
    binding: &DestinationRepairBindingV2,
    role: DestinationSecretRoleV2,
) -> Result<Option<SecretRefId>, RepositoryError> {
    if binding.kind == "local" {
        return Ok(None);
    }
    let value = match role {
        DestinationSecretRoleV2::BackupWriter => binding.writer_secret_ref_id.clone(),
        DestinationSecretRoleV2::RestoreReader => binding.reader_secret_ref_id.clone(),
        DestinationSecretRoleV2::DeleteOperator | DestinationSecretRoleV2::HostAuth => None,
    };
    value.map(Some).ok_or(RepositoryError::RuntimeStateInvalid)
}

fn validate_active_repair_secret(
    connection: &rusqlite::Connection,
    secret_ref_id: Option<&SecretRefId>,
    expected_purpose: &str,
) -> Result<(), RepositoryError> {
    let Some(secret_ref_id) = secret_ref_id else {
        return Ok(());
    };
    let valid: bool = connection.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM secret_refs
             WHERE id = ?1
               AND purpose = ?2
               AND state = 'active'
         )",
        params![secret_ref_id.as_str(), expected_purpose],
        |row| row.get(0),
    )?;
    if valid {
        Ok(())
    } else {
        Err(RepositoryError::SecurityInputInvalid)
    }
}

fn insert_plan_draft_in_workspace(
    transaction: &Transaction<'_>,
    workspace_id: &str,
    spec: &PlanDraftSpec,
) -> Result<(), RepositoryError> {
    ensure_active_workspace(transaction, workspace_id)?;
    ensure_workspace_resource_name_available(
        transaction,
        "plan",
        workspace_id,
        &spec.display_name,
    )?;
    let requested_display_name = spec.display_name.trim().to_owned();
    let insert_display_name =
        temporary_display_name(workspace_id, spec.plan_id.as_str(), &requested_display_name);
    transaction.execute(
        "INSERT INTO backup_plans(
             id, display_name, state, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, 'draft', ?3, ?3)",
        params![
            spec.plan_id.as_str(),
            insert_display_name,
            spec.created_at_ms
        ],
    )?;
    assign_plan_workspace(
        transaction,
        workspace_id,
        spec.plan_id.as_str(),
        &requested_display_name,
    )?;
    transaction.execute(
        "INSERT INTO backup_plan_revisions(
             id, backup_plan_id, revision_no, schedule_kind,
             schedule_expression, timezone, catchup_policy, overlap_policy,
             rpo_ms, retention_policy_json, verification_policy_json,
             execution_policy_json, content_hash_blake3, created_at_ms
         ) VALUES (
             ?1, ?2, ?3, ?4,
             ?5, ?6, ?7, ?8,
             ?9, ?10, ?11,
             ?12, ?13, ?14
         )",
        params![
            spec.revision_id.as_str(),
            spec.plan_id.as_str(),
            spec.revision_no,
            spec.schedule_kind,
            spec.schedule_expression,
            spec.timezone,
            spec.catchup_policy,
            spec.overlap_policy,
            spec.rpo_ms,
            spec.retention_policy_json,
            spec.verification_policy_json,
            spec.execution_policy_json,
            spec.content_hash_blake3,
            spec.created_at_ms
        ],
    )?;
    Ok(())
}

fn default_workspace_id() -> WorkspaceId {
    WorkspaceId::from_str(DEFAULT_WORKSPACE_ID).expect("default Workspace ID is canonical")
}

fn ensure_active_workspace(
    transaction: &Transaction<'_>,
    workspace_id: &str,
) -> Result<(), RepositoryError> {
    let state = transaction
        .query_row(
            "SELECT state FROM workspaces WHERE id = ?1",
            [workspace_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    match state.as_deref() {
        Some("active") => Ok(()),
        Some(_) => Err(RepositoryError::WorkspaceScopeMismatch),
        None => Err(RepositoryError::NotFound),
    }
}

fn ensure_workspace_resource_name_available(
    transaction: &Transaction<'_>,
    resource_kind: &str,
    workspace_id: &str,
    display_name: &str,
) -> Result<(), RepositoryError> {
    let display_name_key = display_name.trim().to_lowercase();
    let sql = match resource_kind {
        "connection" => {
            "SELECT EXISTS(
                 SELECT 1 FROM workspace_connection_profiles
                 WHERE workspace_id = ?1 AND display_name_key = ?2
             )"
        }
        "destination" => {
            "SELECT EXISTS(
                 SELECT 1 FROM workspace_destinations
                 WHERE workspace_id = ?1 AND display_name_key = ?2
             )"
        }
        "plan" => {
            "SELECT EXISTS(
                 SELECT 1 FROM workspace_backup_plans
                 WHERE workspace_id = ?1 AND display_name_key = ?2
             )"
        }
        _ => return Err(RepositoryError::WorkspaceInputInvalid),
    };
    let exists: bool =
        transaction.query_row(sql, params![workspace_id, display_name_key], |row| {
            row.get(0)
        })?;
    if exists {
        Err(RepositoryError::WorkspaceNameConflict)
    } else {
        Ok(())
    }
}

fn temporary_display_name(workspace_id: &str, resource_id: &str, requested: &str) -> String {
    if workspace_id == DEFAULT_WORKSPACE_ID {
        requested.to_owned()
    } else {
        format!("{resource_id}:{requested}")
    }
}

fn assign_connection_workspace(
    transaction: &Transaction<'_>,
    workspace_id: &str,
    resource_id: &str,
    requested_display_name: &str,
) -> Result<(), RepositoryError> {
    if workspace_id != DEFAULT_WORKSPACE_ID {
        transaction.execute(
            "UPDATE workspace_connection_profiles SET workspace_id = ?1
             WHERE connection_profile_id = ?2",
            params![workspace_id, resource_id],
        )?;
        transaction.execute(
            "UPDATE connection_profiles SET display_name = ?2 WHERE id = ?1",
            params![resource_id, requested_display_name],
        )?;
    }
    Ok(())
}

fn assign_destination_workspace(
    transaction: &Transaction<'_>,
    workspace_id: &str,
    resource_id: &str,
    requested_display_name: &str,
) -> Result<(), RepositoryError> {
    if workspace_id != DEFAULT_WORKSPACE_ID {
        transaction.execute(
            "UPDATE workspace_destinations SET workspace_id = ?1 WHERE destination_id = ?2",
            params![workspace_id, resource_id],
        )?;
        transaction.execute(
            "UPDATE destinations SET display_name = ?2 WHERE id = ?1",
            params![resource_id, requested_display_name],
        )?;
    }
    Ok(())
}

fn assign_plan_workspace(
    transaction: &Transaction<'_>,
    workspace_id: &str,
    resource_id: &str,
    requested_display_name: &str,
) -> Result<(), RepositoryError> {
    if workspace_id != DEFAULT_WORKSPACE_ID {
        transaction.execute(
            "UPDATE workspace_backup_plans SET workspace_id = ?1 WHERE backup_plan_id = ?2",
            params![workspace_id, resource_id],
        )?;
        transaction.execute(
            "UPDATE backup_plans SET display_name = ?2 WHERE id = ?1",
            params![resource_id, requested_display_name],
        )?;
    }
    Ok(())
}

fn ensure_revision_draft(
    transaction: &Transaction<'_>,
    revision_id: &str,
) -> Result<(), RepositoryError> {
    let published_at: Option<Option<i64>> = transaction
        .query_row(
            "SELECT published_at_ms
             FROM backup_plan_revisions
             WHERE id = ?1",
            [revision_id],
            |row| row.get(0),
        )
        .optional()?;
    match published_at {
        None => Err(RepositoryError::NotFound),
        Some(Some(_)) => Err(DomainError::PlanRevisionImmutable.into()),
        Some(None) => Ok(()),
    }
}
