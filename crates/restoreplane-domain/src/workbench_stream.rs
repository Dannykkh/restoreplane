//! Result delivery identity and accounting are separate from durable operations.
use crate::workbench::{Column, DecimalU64};
use crate::workbench_metadata::{Consistency, Row};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StreamIdentity {
    pub window_id: Uuid,
    pub workspace_id: String,
    pub database_target_id: String,
    pub session_id: Uuid,
    pub connection_generation: DecimalU64,
    pub query_run_id: Uuid,
    pub result_set_id: Uuid,
    pub stream_generation: DecimalU64,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    Complete,
    Capped,
    Cancelled,
    Interrupted,
    Partial,
    Failed,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ResultEvent {
    Schema {
        columns: Vec<Column>,
        consistency: Consistency,
    },
    Rows {
        start_row: DecimalU64,
        rows: Vec<Row>,
    },
    Terminal {
        completeness: Completion,
        rows_seen: DecimalU64,
        operation_version: DecimalU64,
        error_code: Option<String>,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResultEnvelope {
    pub protocol_version: u32,
    pub identity: StreamIdentity,
    pub sequence: DecimalU64,
    pub encoded_bytes: usize,
    pub event: ResultEvent,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Ack {
    pub identity: StreamIdentity,
    pub highest_contiguous_sequence: DecimalU64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AckResult {
    pub accepted_through: DecimalU64,
    pub released_bytes: DecimalU64,
    pub available_bytes: DecimalU64,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResultNotice {
    pub protocol_version: u32,
    pub kind: ResultNoticeKind,
    pub identity: StreamIdentity,
    pub available_through_sequence: DecimalU64,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultNoticeKind {
    FramesAvailable,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrameBatch {
    pub frames: Vec<ResultEnvelope>,
    pub has_more: bool,
}
