//! Reviewed generated edits. Values are transient current-user memory only.
use crate::workbench::{DecimalU64, SessionRef, TargetContext, TypedCell};
use crate::workbench_metadata::ObjectRef;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditValue {
    pub column_id: Uuid,
    pub value: TypedCell,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum EditChange {
    Insert {
        values: Vec<EditValue>,
    },
    Update {
        row_handle: Uuid,
        values: Vec<EditValue>,
    },
    Delete {
        row_handle: Uuid,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditPrepareInput {
    pub session: SessionRef,
    pub object: ObjectRef,
    pub changes: Vec<EditChange>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditApplyInput {
    pub session: SessionRef,
    pub draft_id: Uuid,
    pub review_id: Uuid,
    pub confirmed_warnings: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReviewedChange {
    pub kind: String,
    pub row_handle: Option<Uuid>,
    pub original: Vec<EditValue>,
    pub draft: Vec<EditValue>,
    pub omitted_default_column_ids: Vec<Uuid>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditReview {
    pub draft_id: Uuid,
    pub review_id: Uuid,
    pub target: TargetContext,
    pub object: ObjectRef,
    pub statement_previews: Vec<String>,
    pub parameter_preview: Vec<TypedCell>,
    pub changes: Vec<ReviewedChange>,
    pub warnings: Vec<String>,
    pub expires_at_ms: DecimalU64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservedCurrent {
    Present { values: Vec<EditValue> },
    Missing,
    Unavailable { reason: String },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConflictRow {
    pub change_index: u32,
    pub row_handle: Option<Uuid>,
    pub current: ObservedCurrent,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConflictObservation {
    pub conflict_id: Uuid,
    pub draft_id: Uuid,
    pub review_id: Uuid,
    pub operation_id: Uuid,
    pub basis_object: ObjectRef,
    pub current_schema_version: Option<String>,
    pub observed_at_ms: DecimalU64,
    pub rows: Vec<ConflictRow>,
}
