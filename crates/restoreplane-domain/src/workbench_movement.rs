//! File movement DTOs. Paths in durable metadata originate in native dialogs;
//! they are never deserialized from a frontend path authority claim.
use crate::workbench::{DecimalU64, Operation, OperationState, SessionRef};
use crate::workbench_metadata::{ObjectRef, Page};
use crate::workbench_stream::{Completion, StreamIdentity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MovementFormat {
    Csv,
    Tsv,
    Json,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FormulaPolicy {
    Escape,
    RawConfirmed,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PartialPolicy {
    #[default]
    Retain,
    RemoveOnCancel,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileArtifactState {
    NotStarted,
    Partial,
    Complete,
    Unconfirmed,
    Removed,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovementSourceColumn {
    pub source_key: String,
    pub ordinal: u32,
    pub name: String,
    pub type_hint: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovementSource {
    pub source_handle: Uuid,
    pub display_name: String,
    pub byte_length: DecimalU64,
    pub columns: Vec<MovementSourceColumn>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovementDestination {
    pub destination_handle: Uuid,
    pub display_name: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShownRange {
    pub start_row: DecimalU64,
    pub row_count: u32,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ExportSource {
    Shown {
        identity: StreamIdentity,
        range: ShownRange,
    },
    Fresh {
        review_id: Uuid,
        snapshot_id: Option<Uuid>,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportStartInput {
    pub session: SessionRef,
    pub source: ExportSource,
    pub destination_handle: Uuid,
    pub format: MovementFormat,
    pub formula_policy: FormulaPolicy,
    #[serde(default)]
    pub partial_policy: PartialPolicy,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportStartInput {
    pub session: SessionRef,
    pub preview_id: Uuid,
    pub confirmed_warnings: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportContinueInput {
    pub session: SessionRef,
    pub parent_operation_id: Uuid,
    pub review_id: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovementProgress {
    pub bytes_processed: DecimalU64,
    pub total_bytes: Option<DecimalU64>,
    pub rows_processed: DecimalU64,
    pub confirmed_committed_rows: DecimalU64,
    pub confirmed_committed_batches: DecimalU64,
    pub unresolved_batches: DecimalU64,
    pub next_batch_index: Option<u32>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileArtifact {
    pub display_path: String,
    pub partial_display_path: Option<String>,
    pub format: MovementFormat,
    pub state: FileArtifactState,
    pub bytes_written: DecimalU64,
    pub rows_written: DecimalU64,
    pub source_completeness: Completion,
    pub finalized: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportBatchStatus {
    pub operation_id: Uuid,
    pub batch_index: u32,
    pub state: OperationState,
    pub version: DecimalU64,
    pub source_byte_start: DecimalU64,
    pub source_byte_end: DecimalU64,
    pub source_row_start: DecimalU64,
    pub source_row_end: DecimalU64,
    pub rows_committed: Option<DecimalU64>,
    pub error_code: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MovementStatus {
    pub operation: Operation,
    pub progress: MovementProgress,
    pub children: Page<ImportBatchStatus>,
    pub file: Option<FileArtifact>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NextUnsent {
    pub batch_index: u32,
    pub byte_start: DecimalU64,
    pub row_start: DecimalU64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovementContinuationMarker {
    pub token_hash: String,
    pub public_digest: String,
    pub resolved_digest: String,
    pub window_instance_id: Uuid,
    pub session: SessionRef,
    pub descriptor_revision: String,
    pub authority_epoch: String,
    pub journal_epoch: String,
    pub process_boot_epoch: Uuid,
    pub expires_at_ms: DecimalU64,
    pub next_unsent: NextUnsent,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportContinuationReview {
    pub review_id: Uuid,
    pub parent_operation_id: Uuid,
    pub source_digest: String,
    pub next_unsent: Option<NextUnsent>,
    pub confirmed_committed_rows: DecimalU64,
    pub unresolved_batches: DecimalU64,
    pub warnings: Vec<String>,
    pub expires_at_ms: DecimalU64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovementMapping {
    pub source_key: String,
    pub source_ordinal: u32,
    pub column_id: Uuid,
    pub target_ordinal: u32,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MovementTargetBasis {
    pub schema_name: String,
    pub table_name: String,
    pub schema_version: String,
    pub column_basis_digest: String,
    pub server_identity: Option<String>,
    pub actual_database_identity: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PersistedExportSource {
    Shown {
        identity: StreamIdentity,
        range: ShownRange,
    },
    Fresh {
        review_digest: String,
        snapshot_id: Option<Uuid>,
        read_started_at_ms: DecimalU64,
    },
}
/// Metadata-only native persistence. No source row, SQL, parameter, or secret
/// field exists; all counters remain exact decimal strings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum MovementMetadata {
    Import {
        signature_version: u32,
        source_digest: String,
        format: MovementFormat,
        object: ObjectRef,
        target_basis: MovementTargetBasis,
        mapping: Vec<MovementMapping>,
        total_bytes: DecimalU64,
        total_rows: DecimalU64,
        progress: MovementProgress,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        latest_continuation: Option<MovementContinuationMarker>,
    },
    Export {
        signature_version: u32,
        format: MovementFormat,
        formula_policy: FormulaPolicy,
        partial_policy: PartialPolicy,
        final_path: String,
        partial_path: Option<String>,
        file_identity: Option<String>,
        source_digest: String,
        source: PersistedExportSource,
        file: FileArtifact,
        progress: MovementProgress,
    },
}
