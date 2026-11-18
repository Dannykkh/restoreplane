//! Current-user database-management contracts, separate from backup authority.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::{ConnectionProfileId, DatabaseTargetId, WorkspaceId};

/// JSON counters use decimal strings so JavaScript cannot round identities.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct DecimalU64(u64);

impl DecimalU64 {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl From<u64> for DecimalU64 {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl Serialize for DecimalU64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for DecimalU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let value = text.parse::<u64>().map_err(serde::de::Error::custom)?;
        if value.to_string() != text {
            return Err(serde::de::Error::custom("noncanonical unsigned decimal"));
        }
        Ok(Self(value))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Scope {
    pub workspace_id: WorkspaceId,
    pub connection_profile_id: ConnectionProfileId,
    pub database_target_id: DatabaseTargetId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgresql,
    Mysql,
    Mariadb,
    Sqlite,
}

impl Engine {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Postgresql => "postgresql",
            Self::Mysql => "mysql",
            Self::Mariadb => "mariadb",
            Self::Sqlite => "sqlite",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Route {
    Tcp { host: String, port: u16 },
    File { canonical_path: String },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum RootTrust {
    System,
    Custom {
        canonical_path: String,
        content_digest: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Tls {
    pub mode: String,
    pub server_name: Option<String>,
    pub root_trust: Option<RootTrust>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DatabaseIdentity {
    pub engine_object_id: String,
    pub name: String,
    pub source_fingerprint: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectDescriptor {
    pub engine: Engine,
    pub route: Route,
    pub database: DatabaseIdentity,
    pub tls: Tls,
    pub transport_options: BTreeMap<String, String>,
    pub expected_server_identity: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizePurpose {
    Connect,
    Read,
    Mutate,
    Commit,
    Protection,
}

impl AuthorizePurpose {
    pub const fn is_mutating(self) -> bool {
        matches!(self, Self::Mutate | Self::Commit | Self::Protection)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizeRequest {
    pub workspace_id: WorkspaceId,
    pub connection_profile_id: ConnectionProfileId,
    pub database_target_id: DatabaseTargetId,
    pub purpose: AuthorizePurpose,
    pub expected_descriptor_revision: Option<String>,
    pub app_nonce: Uuid,
}

impl AuthorizeRequest {
    pub fn scope(&self) -> Scope {
        Scope {
            workspace_id: self.workspace_id.clone(),
            connection_profile_id: self.connection_profile_id.clone(),
            database_target_id: self.database_target_id.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetConflict {
    pub kind: String,
    pub run_id: String,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingReceipt {
    #[serde(flatten)]
    pub scope: Scope,
    pub descriptor: ConnectDescriptor,
    pub descriptor_revision: String,
    pub authority_epoch: String,
    pub authorized_at_ms: DecimalU64,
    pub read_lease_expires_at_ms: Option<DecimalU64>,
    pub observed_conflicts: Vec<TargetConflict>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    ReadOnly,
    ReadWrite,
    Full,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRef {
    pub session_id: Uuid,
    pub connection_generation: DecimalU64,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ErrorDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicting_operation_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<DecimalU64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_version: Option<DecimalU64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WBError {
    pub code: String,
    pub message_key: String,
    pub retryable: bool,
    pub operation_id: Option<Uuid>,
    pub field: Option<String>,
    pub details: Box<ErrorDetails>,
}

impl WBError {
    pub fn new(code: &str) -> Self {
        Self {
            code: code.to_owned(),
            message_key: code.to_owned(),
            retryable: false,
            operation_id: None,
            field: None,
            details: Box::default(),
        }
    }
    pub fn with_reason(mut self, reason: &str) -> Self {
        self.details.reason = Some(reason.to_owned());
        self
    }
}

impl fmt::Display for WBError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.code)
    }
}
impl std::error::Error for WBError {}

/// Display text is never the canonical editable value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TypedCell {
    Null,
    Bool {
        value: bool,
    },
    I64 {
        value: String,
    },
    U64 {
        value: String,
    },
    Decimal {
        value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        scale: Option<u32>,
    },
    Float {
        value: String,
    },
    Text {
        value: String,
    },
    Bytes {
        base64: String,
    },
    Temporal {
        value: String,
        type_name: String,
        offset: Option<String>,
        precision: u32,
    },
    Unsupported {
        type_name: String,
        reason: String,
    },
    Large {
        value_handle: Uuid,
        type_name: String,
        byte_length: DecimalU64,
        preview: String,
    },
}

pub type Cell = TypedCell;

macro_rules! journal_enum {
    ($name:ident { $($variant:ident => $wire:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
        pub enum $name { $(#[serde(rename = $wire)] $variant),+ }
        impl $name {
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $wire),+ } }
            pub fn parse(value: &str) -> Option<Self> { match value { $($wire => Some(Self::$variant)),+, _ => None } }
        }
    };
}
journal_enum!(OperationKind { Query=>"query", ManualTransaction=>"manual_transaction", VerifiedApply=>"verified_apply", ImportJob=>"import_job", ImportBatch=>"import_batch", ExportJob=>"export_job", ProtectionLink=>"protection_link" });
journal_enum!(OperationState { Accepted=>"accepted", Dispatching=>"dispatching", Running=>"running", CommitRequested=>"commit_requested", Committed=>"committed", RolledBack=>"rolled_back", Failed=>"failed", OutcomeUnknown=>"outcome_unknown", Completed=>"completed", Cancelled=>"cancelled", Interrupted=>"interrupted", Partial=>"partial" });
impl OperationState {
    pub const fn is_terminal(self) -> bool {
        !matches!(
            self,
            Self::Accepted | Self::Dispatching | Self::Running | Self::CommitRequested
        )
    }
}
journal_enum!(EffectSummary { None=>"none", Read=>"read", Transactional=>"transactional", Partial=>"partial", Unknown=>"unknown" });
journal_enum!(TransactionOutcome { None=>"none", Open=>"open", CommitRequested=>"commit_requested", Committed=>"committed", RolledBack=>"rolled_back", OutcomeUnknown=>"outcome_unknown" });
journal_enum!(ResultCompleteness { None=>"none", Complete=>"complete", Capped=>"capped", Cancelled=>"cancelled", Interrupted=>"interrupted", Partial=>"partial" });
journal_enum!(StatementStatus { NotSent=>"not_sent", Sent=>"sent", Succeeded=>"succeeded", Failed=>"failed", Cancelled=>"cancelled", OutcomeUnknown=>"outcome_unknown" });
journal_enum!(EffectClass { Read=>"read", Write=>"write", Ddl=>"ddl", Transaction=>"transaction", Unknown=>"unknown" });
journal_enum!(EffectEvidence { None=>"none", ReadOnly=>"read_only", PendingTransaction=>"pending_transaction", Committed=>"committed", RolledBack=>"rolled_back", OutcomeUnknown=>"outcome_unknown" });
journal_enum!(TransactionState { Opening=>"opening", Active=>"active", FailedNeedsRollback=>"failed_needs_rollback", CommitRequested=>"commit_requested", Committed=>"committed", RollbackRequested=>"rollback_requested", RolledBack=>"rolled_back", OutcomeUnknown=>"outcome_unknown" });

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatementOutcome {
    pub ordinal: u32,
    pub digest: String,
    pub outcome: StatementStatus,
    pub effect_class: EffectClass,
    pub effect_evidence: EffectEvidence,
    pub rows_matched: Option<DecimalU64>,
    pub rows_changed: Option<DecimalU64>,
    pub version: DecimalU64,
    pub error_code: Option<String>,
    pub transaction_id: Option<Uuid>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Operation {
    pub operation_id: Uuid,
    pub version: DecimalU64,
    pub kind: OperationKind,
    pub state: OperationState,
    pub effect_summary: EffectSummary,
    pub transaction_outcome: TransactionOutcome,
    pub result_completeness: ResultCompleteness,
    pub transaction_id: Option<Uuid>,
    pub statements: Vec<StatementOutcome>,
    pub parent_operation_id: Option<Uuid>,
    pub batch_index: Option<u32>,
    pub acknowledged_at_ms: Option<DecimalU64>,
    pub created_at_ms: DecimalU64,
    pub updated_at_ms: DecimalU64,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Transaction {
    pub transaction_id: Uuid,
    pub session: SessionRef,
    pub state: TransactionState,
    pub isolation: String,
    pub read_only: bool,
    pub idle_expires_at_ms: DecimalU64,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedAttempt {
    pub attempt_token: String,
    pub expires_at_ms: DecimalU64,
    pub payload_digest: String,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Admission {
    pub operation_id: Uuid,
    pub version: DecimalU64,
    pub deduplicated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Column {
    pub column_id: Uuid,
    pub name: String,
    pub type_name: String,
    pub nullable: bool,
    pub editable: bool,
    pub readonly_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Connecting,
    Ready,
    Busy,
    Retiring,
    Closed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActualContext {
    pub endpoint: String,
    pub database: String,
    pub role: String,
    pub purpose: TargetPurpose,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetPurpose {
    Source,
    Restored,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetContext {
    #[serde(flatten)]
    pub scope: Scope,
    pub connection_alias: String,
    pub actual_endpoint: String,
    pub database_name: String,
    pub database_role: String,
    pub purpose: TargetPurpose,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Capability {
    pub supported: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    #[serde(flatten)]
    pub session: SessionRef,
    #[serde(flatten)]
    pub scope: Scope,
    pub descriptor_revision: String,
    pub mode: Mode,
    pub execution_epoch: DecimalU64,
    pub state: SessionState,
    pub actual_context: ActualContext,
    pub capabilities: BTreeMap<String, Capability>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_do_not_round_through_javascript_or_accept_noncanonical_forms() {
        let maximum = DecimalU64::from(u64::MAX);
        assert_eq!(
            serde_json::to_string(&maximum).unwrap(),
            "\"18446744073709551615\""
        );
        assert_eq!(
            serde_json::from_str::<DecimalU64>("\"18446744073709551615\"").unwrap(),
            maximum
        );
        for invalid in [
            "1",
            "\"01\"",
            "\"+1\"",
            "\"-1\"",
            "\"18446744073709551616\"",
        ] {
            assert!(serde_json::from_str::<DecimalU64>(invalid).is_err());
        }
    }

    #[test]
    fn authentication_payload_refuses_sql_secrets_and_caller_identity() {
        let scope = Scope {
            workspace_id: WorkspaceId::new(),
            connection_profile_id: ConnectionProfileId::new(),
            database_target_id: DatabaseTargetId::new(),
        };
        let mut value = serde_json::json!({"workspaceId":scope.workspace_id,"connectionProfileId":scope.connection_profile_id,"databaseTargetId":scope.database_target_id,"purpose":"connect","expectedDescriptorRevision":null,"appNonce":Uuid::new_v4()});
        assert!(serde_json::from_value::<AuthorizeRequest>(value.clone()).is_ok());
        for key in ["sql", "password", "ownerSid", "windowId"] {
            value[key] = "forbidden".into();
            assert!(serde_json::from_value::<AuthorizeRequest>(value.clone()).is_err());
            value.as_object_mut().unwrap().remove(key);
        }
    }
}
