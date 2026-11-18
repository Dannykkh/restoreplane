//! Closed wire contracts for schema metadata and generated table reads.
use crate::workbench::{Column, DecimalU64, TypedCell};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectRef {
    pub catalog_id: Uuid,
    pub schema_version: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PageInput {
    pub cursor: Option<String>,
    pub limit: usize,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Limited,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Count {
    Unknown,
    Estimate { value: DecimalU64 },
    Exact { value: DecimalU64 },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub completeness: Completeness,
    pub count: Count,
    pub as_of_ms: DecimalU64,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CatalogKind {
    Schemas,
    Relations,
    Routines,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DescribePart {
    Columns,
    Keys,
    Indexes,
    ForeignKeys,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CatalogItem {
    pub object: ObjectRef,
    pub name: String,
    pub kind: String,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CatalogDetail {
    Column {
        column: Column,
        ordinal: usize,
    },
    Key {
        key_id: Uuid,
        columns: Vec<Uuid>,
        primary: bool,
        unique: bool,
        nullable: bool,
        partial: bool,
        expression: bool,
    },
    Index {
        index_id: Uuid,
        columns: Vec<Uuid>,
        unique: bool,
        predicate: Option<String>,
    },
    ForeignKey {
        key_id: Uuid,
        columns: Vec<Uuid>,
        referenced_relation: ObjectRef,
        referenced_columns: Vec<Uuid>,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Filter {
    And {
        items: Vec<Filter>,
    },
    Or {
        items: Vec<Filter>,
    },
    Compare {
        column_id: Uuid,
        operator: Comparison,
        value: Option<TypedCell>,
    },
}
impl Default for Filter {
    fn default() -> Self {
        Self::And { items: Vec::new() }
    }
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    IsNull,
    IsNotNull,
    Contains,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Asc,
    Desc,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NullOrder {
    First,
    Last,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Order {
    pub column_id: Uuid,
    pub direction: Direction,
    pub nulls: NullOrder,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Limits {
    pub max_rows: usize,
    pub max_bytes: usize,
    pub statement_timeout_ms: u64,
    pub lock_wait_ms: u64,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Row {
    pub row_handle: Uuid,
    pub cells: Vec<TypedCell>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Consistency {
    LivePerPage,
    TransactionSnapshot {
        snapshot_id: Uuid,
        expires_at_ms: DecimalU64,
    },
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultPage {
    #[serde(flatten)]
    pub page: Page<Row>,
    pub result_set_id: Uuid,
    pub columns: Vec<Column>,
    pub consistency: Consistency,
    pub schema_version: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagram {
    pub nodes: Vec<CatalogItem>,
    pub edges: Vec<CatalogDetail>,
    pub schema_version: String,
    pub limited: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphEdge {
    pub to: Uuid,
    pub name: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GraphNode {
    pub node_id: Uuid,
    pub relation: ObjectRef,
    pub name: String,
    pub edges: Vec<GraphEdge>,
}
