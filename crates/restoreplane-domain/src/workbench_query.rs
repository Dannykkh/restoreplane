//! Closed query-workspace requests; SQL is transient unless explicitly saved.
use crate::workbench::{Admission, DecimalU64, Mode, Scope, SessionRef};
use crate::workbench_metadata::{Limits, PageInput};
use crate::workbench_stream::{Completion, StreamIdentity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueryPurpose {
    Run,
    ExplainPlan,
    ExplainAnalyze,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryReviewInput {
    pub session: SessionRef,
    pub sql: String,
    pub purpose: QueryPurpose,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryReview {
    pub review_id: Uuid,
    pub digest: String,
    pub required_mode: Mode,
    pub statement_count: usize,
    pub warnings: Vec<String>,
    pub expires_at_ms: DecimalU64,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryRunInput {
    pub session: SessionRef,
    pub review_id: Uuid,
    pub confirmed_warnings: Vec<String>,
    pub limits: Limits,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryAdmission {
    #[serde(flatten)]
    pub admission: Admission,
    pub query_run_id: Uuid,
    pub execution_epoch: DecimalU64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryResultsInput {
    pub session: SessionRef,
    pub query_run_id: Uuid,
    pub page: PageInput,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    pub identity: StreamIdentity,
    pub state: QueryResultState,
    pub completeness: Option<Completion>,
}
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryResultState {
    Streaming,
    Terminal,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryCancelInput {
    pub session: SessionRef,
    pub query_run_id: Uuid,
    pub execution_epoch: DecimalU64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryCancelResult {
    pub cancel_requested: bool,
    pub settled: bool,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionBeginInput {
    pub session: SessionRef,
    pub isolation: String,
    pub read_only: bool,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TransactionInput {
    pub session: SessionRef,
    pub transaction_id: Uuid,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQuerySummary {
    pub query_id: Uuid,
    pub name: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQuery {
    pub query_id: Uuid,
    pub name: String,
    pub sql: String,
    pub version: DecimalU64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SavedQueryPut {
    pub scope: Scope,
    pub query_id: Option<Uuid>,
    pub name: String,
    pub sql: String,
    pub expected_version: Option<DecimalU64>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedQueryVersion {
    pub query_id: Uuid,
    pub version: DecimalU64,
}
