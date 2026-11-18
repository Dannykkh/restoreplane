use std::collections::BTreeSet;
use std::str::FromStr;

use restoreplane_core::{SelectedTable, MAX_SELECTION_IDENTIFIER_BYTES};
use restoreplane_domain::{
    BackupScopeId, DatabaseTargetId, OperationRunId, PlanRevisionId, WorkspaceId,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Serialize;
use serde_json::json;

use crate::OperationalStore;

use super::RepositoryError;

const MAX_DISCOVERED_OBJECTS: usize = 100_000;
const MAX_SELECTED_OBJECTS: usize = 1_000;
const MAX_SELECTOR_BYTES: usize = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupScopeMode {
    WholeDatabase,
    ExplicitTables,
}

impl BackupScopeMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WholeDatabase => "whole_database",
            Self::ExplicitTables => "explicit_tables",
        }
    }

    fn parse(value: &str) -> Result<Self, RepositoryError> {
        match value {
            "whole_database" => Ok(Self::WholeDatabase),
            "explicit_tables" => Ok(Self::ExplicitTables),
            _ => Err(RepositoryError::BackupScopeConflict),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupScopeRecord {
    id: BackupScopeId,
    database_target_id: DatabaseTargetId,
    display_name: String,
    mode: BackupScopeMode,
    state: String,
    selector_hash_blake3: String,
    source_fingerprint: String,
    database_source_fingerprint: String,
    resolved_object_count: i64,
    last_resolved_at_ms: Option<i64>,
    created_at_ms: i64,
    updated_at_ms: i64,
}

impl BackupScopeRecord {
    pub fn id(&self) -> &BackupScopeId {
        &self.id
    }

    pub fn database_target_id(&self) -> &DatabaseTargetId {
        &self.database_target_id
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    pub const fn mode(&self) -> BackupScopeMode {
        self.mode
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn selector_hash_blake3(&self) -> &str {
        &self.selector_hash_blake3
    }

    pub fn source_fingerprint(&self) -> &str {
        &self.source_fingerprint
    }

    pub fn database_source_fingerprint(&self) -> &str {
        &self.database_source_fingerprint
    }

    pub const fn resolved_object_count(&self) -> i64 {
        self.resolved_object_count
    }

    pub const fn last_resolved_at_ms(&self) -> Option<i64> {
        self.last_resolved_at_ms
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredDatabaseObjectSpec {
    engine_object_id: String,
    schema_name: String,
    object_name: String,
    object_type: String,
    eligibility: String,
    exclusion_reason: Option<String>,
    object_fingerprint_blake3: String,
}

impl DiscoveredDatabaseObjectSpec {
    pub fn eligible_table(
        engine_object_id: impl Into<String>,
        schema_name: impl Into<String>,
        object_name: impl Into<String>,
        object_fingerprint_blake3: impl Into<String>,
    ) -> Self {
        Self {
            engine_object_id: engine_object_id.into(),
            schema_name: schema_name.into(),
            object_name: object_name.into(),
            object_type: "table".to_owned(),
            eligibility: "eligible".to_owned(),
            exclusion_reason: None,
            object_fingerprint_blake3: object_fingerprint_blake3.into(),
        }
    }

    pub fn excluded(
        engine_object_id: impl Into<String>,
        schema_name: impl Into<String>,
        object_name: impl Into<String>,
        object_type: impl Into<String>,
        exclusion_reason: impl Into<String>,
        object_fingerprint_blake3: impl Into<String>,
    ) -> Self {
        Self {
            engine_object_id: engine_object_id.into(),
            schema_name: schema_name.into(),
            object_name: object_name.into(),
            object_type: object_type.into(),
            eligibility: "excluded".to_owned(),
            exclusion_reason: Some(exclusion_reason.into()),
            object_fingerprint_blake3: object_fingerprint_blake3.into(),
        }
    }

    fn validate(&self) -> bool {
        !self.engine_object_id.trim().is_empty()
            && self.engine_object_id.len() <= 2_048
            && self.schema_name.len() <= 256
            && !self.object_name.trim().is_empty()
            && self.object_name.len() <= 256
            && matches!(
                self.object_type.as_str(),
                "table" | "partitioned_table" | "view" | "materialized_view" | "foreign_table"
            )
            && matches!(
                self.eligibility.as_str(),
                "eligible" | "excluded" | "unsupported"
            )
            && (self.eligibility != "eligible"
                || (self.object_type == "table" && self.exclusion_reason.is_none()))
            && (self.eligibility == "eligible"
                || self
                    .exclusion_reason
                    .as_ref()
                    .is_some_and(|reason| !reason.is_empty() && reason.len() <= 256))
            && is_blake3(&self.object_fingerprint_blake3)
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseObjectDiscoverySpec {
    operation_run_id: OperationRunId,
    database_target_id: DatabaseTargetId,
    inventory_hash_blake3: String,
    objects: Vec<DiscoveredDatabaseObjectSpec>,
    observed_at_ms: i64,
    selected_coverage: bool,
}

impl DatabaseObjectDiscoverySpec {
    pub fn new(
        operation_run_id: OperationRunId,
        database_target_id: DatabaseTargetId,
        inventory_hash_blake3: impl Into<String>,
        objects: Vec<DiscoveredDatabaseObjectSpec>,
        observed_at_ms: i64,
    ) -> Self {
        Self {
            operation_run_id,
            database_target_id,
            inventory_hash_blake3: inventory_hash_blake3.into(),
            objects,
            observed_at_ms,
            selected_coverage: false,
        }
    }

    pub fn with_selected_coverage(mut self) -> Self {
        self.selected_coverage = true;
        self
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.objects.len() > MAX_DISCOVERED_OBJECTS
            || self.observed_at_ms < 0
            || !is_blake3(&self.inventory_hash_blake3)
            || self.objects.iter().any(|object| !object.validate())
            || self
                .objects
                .iter()
                .map(|object| &object.engine_object_id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.objects.len()
        {
            return Err(RepositoryError::BackupScopeInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct ExplicitBackupScopeSpec {
    id: BackupScopeId,
    database_target_id: DatabaseTargetId,
    display_name: String,
    discovery_operation_run_id: OperationRunId,
    engine_object_ids: Vec<String>,
    created_at_ms: i64,
    workbench_selection_proof: bool,
}

impl ExplicitBackupScopeSpec {
    pub fn new(
        id: BackupScopeId,
        database_target_id: DatabaseTargetId,
        display_name: impl Into<String>,
        discovery_operation_run_id: OperationRunId,
        engine_object_ids: Vec<String>,
        created_at_ms: i64,
    ) -> Self {
        Self {
            id,
            database_target_id,
            display_name: display_name.into(),
            discovery_operation_run_id,
            engine_object_ids,
            created_at_ms,
            workbench_selection_proof: false,
        }
    }

    /// Only the service's owner-validated, same-transaction proof guard may
    /// enable selected inventory. This does not widen the selected objects.
    pub fn with_workbench_selection_proof(mut self) -> Self {
        self.workbench_selection_proof = true;
        self
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        let display_name = self.display_name.trim();
        if display_name.is_empty()
            || display_name.chars().count() > 96
            || display_name.chars().any(char::is_control)
            || self.engine_object_ids.is_empty()
            || self.engine_object_ids.len() > MAX_SELECTED_OBJECTS
            || self.engine_object_ids.iter().any(|value| {
                value.trim().is_empty() || value.len() > 2_048 || value.contains(['\0', '\r', '\n'])
            })
            || self.engine_object_ids.iter().collect::<BTreeSet<_>>().len()
                != self.engine_object_ids.len()
            || self.created_at_ms < 0
        {
            return Err(RepositoryError::BackupScopeInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredDatabaseObjectRecord {
    pub discovery_run_id: OperationRunId,
    pub engine_object_id: String,
    pub schema_name: String,
    pub object_name: String,
    pub object_type: String,
    pub eligibility: String,
    pub exclusion_reason: Option<String>,
    pub object_fingerprint_blake3: String,
    pub ordinal: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupScopeObjectPage {
    items: Vec<DiscoveredDatabaseObjectRecord>,
    next_cursor: Option<String>,
    total_count: i64,
    eligible_count: i64,
    discovery_run_id: OperationRunId,
    inventory_hash_blake3: String,
    observed_at_ms: i64,
}

impl BackupScopeObjectPage {
    pub fn items(&self) -> &[DiscoveredDatabaseObjectRecord] {
        &self.items
    }

    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }

    pub const fn total_count(&self) -> i64 {
        self.total_count
    }

    pub const fn eligible_count(&self) -> i64 {
        self.eligible_count
    }

    pub fn discovery_run_id(&self) -> &OperationRunId {
        &self.discovery_run_id
    }

    pub fn inventory_hash_blake3(&self) -> &str {
        &self.inventory_hash_blake3
    }

    pub const fn observed_at_ms(&self) -> i64 {
        self.observed_at_ms
    }
}

#[derive(Clone)]
pub struct BackupScopeRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn backup_scopes(&self) -> BackupScopeRepository {
        BackupScopeRepository {
            store: self.clone(),
        }
    }
}

impl BackupScopeRepository {
    pub fn list_in_workspace(
        &self,
        workspace_id: &WorkspaceId,
    ) -> Result<Vec<BackupScopeRecord>, RepositoryError> {
        let reader = self.store.open_reader()?;
        let mut statement = reader.connection().prepare(
            "SELECT scope.id, scope.database_target_id, scope.display_name, scope.mode, scope.state,
                    scope.selector_hash_blake3, scope.source_fingerprint,
                    scope.database_source_fingerprint, scope.resolved_object_count,
                    scope.last_resolved_at_ms, scope.created_at_ms, scope.updated_at_ms
             FROM backup_scopes AS scope
             JOIN database_targets AS target ON target.id = scope.database_target_id
             JOIN workspace_connection_profiles AS ownership
               ON ownership.connection_profile_id = target.connection_profile_id
             WHERE ownership.workspace_id = ?1
             ORDER BY target.normalized_name, scope.mode = 'whole_database' DESC,
                      scope.state = 'retired', scope.normalized_name, scope.created_at_ms",
        )?;
        let rows = statement.query_map([workspace_id.as_str()], scope_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::from)
    }

    pub fn list_for_target(
        &self,
        database_target_id: &DatabaseTargetId,
    ) -> Result<Vec<BackupScopeRecord>, RepositoryError> {
        let reader = self.store.open_reader()?;
        let mut statement = reader.connection().prepare(
            "SELECT id, database_target_id, display_name, mode, state,
                    selector_hash_blake3, source_fingerprint, database_source_fingerprint,
                    resolved_object_count, last_resolved_at_ms, created_at_ms, updated_at_ms
             FROM backup_scopes
             WHERE database_target_id = ?1
             ORDER BY mode = 'whole_database' DESC, state = 'retired', normalized_name, created_at_ms",
        )?;
        let rows = statement.query_map([database_target_id.as_str()], scope_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::from)
    }

    pub fn record_object_discovery(
        &self,
        spec: DatabaseObjectDiscoverySpec,
    ) -> Result<(), RepositoryError> {
        spec.validate()?;
        self.store
            .run_write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let operation_valid: bool = transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM operation_runs
                         WHERE id = ?1
                           AND operation_kind = 'connection_discovery'
                           AND subject_type = 'database_target'
                           AND subject_id = ?2
                           AND state IN ('created', 'queued', 'running')
                     )",
                    params![
                        spec.operation_run_id.as_str(),
                        spec.database_target_id.as_str()
                    ],
                    |row| row.get(0),
                )?;
                if !operation_valid {
                    return Err(RepositoryError::BackupScopeDiscoveryInvalid);
                }
                let already_recorded: bool = transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM operation_runs
                         WHERE id = ?1
                           AND json_extract(result_json, '$.kind') = 'backup_scope_object_discovery'
                     )",
                    [spec.operation_run_id.as_str()],
                    |row| row.get(0),
                )?;
                if already_recorded {
                    return Err(RepositoryError::BackupScopeConflict);
                }

                let eligible_count = spec
                    .objects
                    .iter()
                    .filter(|object| object.eligibility == "eligible")
                    .count();
                for (ordinal, object) in spec.objects.iter().enumerate() {
                    transaction.execute(
                        "INSERT INTO discovered_database_objects(
                             operation_run_id, database_target_id, engine_object_id,
                             schema_name, object_name, object_type, eligibility,
                             exclusion_reason, object_fingerprint_blake3, ordinal, observed_at_ms
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            spec.operation_run_id.as_str(),
                            spec.database_target_id.as_str(),
                            object.engine_object_id,
                            object.schema_name,
                            object.object_name,
                            object.object_type,
                            object.eligibility,
                            object.exclusion_reason,
                            object.object_fingerprint_blake3,
                            i64::try_from(ordinal).unwrap_or(i64::MAX),
                            spec.observed_at_ms
                        ],
                    )?;
                }
                let mut result = json!({
                    "kind": "backup_scope_object_discovery",
                    "inventoryHashBlake3": spec.inventory_hash_blake3,
                    "totalCount": spec.objects.len(),
                    "eligibleCount": eligible_count,
                    "observedAtMs": spec.observed_at_ms,
                });
                if spec.selected_coverage {
                    result["inventoryCoverage"] = json!("selected");
                }
                let result_json = result.to_string();
                transaction.execute(
                    "UPDATE operation_runs
                     SET result_json = ?2, updated_at_ms = MAX(updated_at_ms, ?3)
                     WHERE id = ?1",
                    params![
                        spec.operation_run_id.as_str(),
                        result_json,
                        spec.observed_at_ms
                    ],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .map_err(RepositoryError::from)??;
        Ok(())
    }

    pub fn object_page(
        &self,
        database_target_id: &DatabaseTargetId,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<BackupScopeObjectPage, RepositoryError> {
        if limit == 0 || limit > 200 {
            return Err(RepositoryError::BackupScopeInputInvalid);
        }
        let after_ordinal = cursor
            .map(str::parse::<i64>)
            .transpose()
            .map_err(|_| RepositoryError::BackupScopeInputInvalid)?
            .unwrap_or(-1);
        if after_ordinal < -1 {
            return Err(RepositoryError::BackupScopeInputInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let latest: Option<(String, String, i64, i64, i64)> = connection
            .query_row(
                "SELECT operation.id,
                        json_extract(operation.result_json, '$.inventoryHashBlake3'),
                        json_extract(operation.result_json, '$.totalCount'),
                        json_extract(operation.result_json, '$.eligibleCount'),
                        json_extract(operation.result_json, '$.observedAtMs')
                 FROM operation_runs AS operation
                 WHERE operation.operation_kind = 'connection_discovery'
                   AND operation.subject_type = 'database_target'
                   AND operation.subject_id = ?1
                   AND json_extract(operation.result_json, '$.kind') = 'backup_scope_object_discovery'
                   AND COALESCE(json_extract(operation.result_json, '$.inventoryCoverage'),'full')='full'
                 ORDER BY json_extract(operation.result_json, '$.observedAtMs') DESC,
                          operation.created_at_ms DESC
                 LIMIT 1",
                [database_target_id.as_str()],
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
        let Some((operation_id, inventory_hash, total_count, eligible_count, observed_at_ms)) =
            latest
        else {
            return Err(RepositoryError::NotFound);
        };
        let discovery_run_id = OperationRunId::from_str(&operation_id)
            .map_err(|_| RepositoryError::BackupScopeConflict)?;
        let mut statement = connection.prepare(
            "SELECT engine_object_id, schema_name, object_name, object_type,
                    eligibility, exclusion_reason, object_fingerprint_blake3, ordinal
             FROM discovered_database_objects
             WHERE operation_run_id = ?1 AND ordinal > ?2
             ORDER BY ordinal
             LIMIT ?3",
        )?;
        let fetch_limit = i64::from(limit) + 1;
        let rows =
            statement.query_map(params![operation_id, after_ordinal, fetch_limit], |row| {
                Ok(DiscoveredDatabaseObjectRecord {
                    discovery_run_id: discovery_run_id.clone(),
                    engine_object_id: row.get(0)?,
                    schema_name: row.get(1)?,
                    object_name: row.get(2)?,
                    object_type: row.get(3)?,
                    eligibility: row.get(4)?,
                    exclusion_reason: row.get(5)?,
                    object_fingerprint_blake3: row.get(6)?,
                    ordinal: row.get(7)?,
                })
            })?;
        let mut items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = items.len() > limit as usize;
        if has_more {
            items.truncate(limit as usize);
        }
        let next_cursor = has_more
            .then(|| items.last().map(|item| item.ordinal.to_string()))
            .flatten();
        Ok(BackupScopeObjectPage {
            items,
            next_cursor,
            total_count,
            eligible_count,
            discovery_run_id,
            inventory_hash_blake3: inventory_hash,
            observed_at_ms,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn object_page_filtered(
        &self,
        database_target_id: &DatabaseTargetId,
        discovery_run_id: &OperationRunId,
        cursor: Option<&str>,
        limit: u32,
        search: Option<&str>,
        schema_name: Option<&str>,
        eligibility: Option<&str>,
    ) -> Result<BackupScopeObjectPage, RepositoryError> {
        if limit == 0
            || limit > 100
            || search.is_some_and(|value| !valid_object_filter(value))
            || schema_name.is_some_and(|value| !valid_object_filter(value))
            || eligibility
                .is_some_and(|value| !matches!(value, "eligible" | "excluded" | "unsupported"))
        {
            return Err(RepositoryError::BackupScopeInputInvalid);
        }
        let after_ordinal = cursor
            .map(str::parse::<i64>)
            .transpose()
            .map_err(|_| RepositoryError::BackupScopeInputInvalid)?
            .unwrap_or(-1);
        if after_ordinal < -1 {
            return Err(RepositoryError::BackupScopeInputInvalid);
        }
        let search_pattern = search.map(object_search_pattern).transpose()?;
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let metadata: Option<(String, i64, i64)> = connection
            .query_row(
                "SELECT json_extract(operation.result_json, '$.inventoryHashBlake3'),
                        json_extract(operation.result_json, '$.eligibleCount'),
                        json_extract(operation.result_json, '$.observedAtMs')
                 FROM operation_runs AS operation
                 WHERE operation.id = ?1
                   AND operation.operation_kind = 'connection_discovery'
                   AND operation.subject_type = 'database_target'
                   AND operation.subject_id = ?2
                   AND json_extract(operation.result_json, '$.kind') = 'backup_scope_object_discovery'",
                params![discovery_run_id.as_str(), database_target_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((inventory_hash, eligible_count, observed_at_ms)) = metadata else {
            return Err(RepositoryError::NotFound);
        };
        let total_count = connection.query_row(
            "SELECT COUNT(*)
             FROM discovered_database_objects AS observed
             WHERE observed.operation_run_id = ?1
               AND (?2 IS NULL OR lower(
                    COALESCE(observed.schema_name, '') || '.' || observed.object_name || ' ' ||
                    observed.engine_object_id
               ) LIKE ?2 ESCAPE '\\')
               AND (?3 IS NULL OR observed.schema_name = ?3)
               AND (?4 IS NULL OR observed.eligibility = ?4)",
            params![
                discovery_run_id.as_str(),
                search_pattern,
                schema_name,
                eligibility,
            ],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT engine_object_id, schema_name, object_name, object_type,
                    eligibility, exclusion_reason, object_fingerprint_blake3, ordinal
             FROM discovered_database_objects AS observed
             WHERE observed.operation_run_id = ?1
               AND (?2 IS NULL OR lower(
                    COALESCE(observed.schema_name, '') || '.' || observed.object_name || ' ' ||
                    observed.engine_object_id
               ) LIKE ?2 ESCAPE '\\')
               AND (?3 IS NULL OR observed.schema_name = ?3)
               AND (?4 IS NULL OR observed.eligibility = ?4)
               AND observed.ordinal > ?5
             ORDER BY observed.ordinal
             LIMIT ?6",
        )?;
        let rows = statement.query_map(
            params![
                discovery_run_id.as_str(),
                search_pattern,
                schema_name,
                eligibility,
                after_ordinal,
                i64::from(limit) + 1,
            ],
            |row| {
                Ok(DiscoveredDatabaseObjectRecord {
                    discovery_run_id: discovery_run_id.clone(),
                    engine_object_id: row.get(0)?,
                    schema_name: row.get(1)?,
                    object_name: row.get(2)?,
                    object_type: row.get(3)?,
                    eligibility: row.get(4)?,
                    exclusion_reason: row.get(5)?,
                    object_fingerprint_blake3: row.get(6)?,
                    ordinal: row.get(7)?,
                })
            },
        )?;
        let mut items = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        let has_more = items.len() > limit as usize;
        if has_more {
            items.truncate(limit as usize);
        }
        let next_cursor = has_more
            .then(|| items.last().map(|item| item.ordinal.to_string()))
            .flatten();
        Ok(BackupScopeObjectPage {
            items,
            next_cursor,
            total_count,
            eligible_count,
            discovery_run_id: discovery_run_id.clone(),
            inventory_hash_blake3: inventory_hash,
            observed_at_ms,
        })
    }

    pub fn create_explicit(
        &self,
        spec: ExplicitBackupScopeSpec,
    ) -> Result<BackupScopeRecord, RepositoryError> {
        self.create_explicit_checked(spec, |_| Ok(()), |_, _, _, _| Ok(()))
    }

    /// The optional guard is trusted Rust metadata validation in the same
    /// local transaction as Scope creation. It accepts no remote/user SQL.
    pub fn create_explicit_checked<F, G>(
        &self,
        spec: ExplicitBackupScopeSpec,
        guard: F,
        record: G,
    ) -> Result<BackupScopeRecord, RepositoryError>
    where
        F: FnOnce(&rusqlite::Transaction<'_>) -> Result<(), RepositoryError> + Send + 'static,
        G: FnOnce(
                &rusqlite::Transaction<'_>,
                &BackupScopeId,
                &str,
                &str,
            ) -> Result<(), RepositoryError>
            + Send
            + 'static,
    {
        spec.validate()?;
        let scope_id = spec.id.clone();
        self.store
            .run_write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                guard(&transaction)?;
                let discovery_valid: bool = transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM operation_runs
                         WHERE id = ?1
                           AND operation_kind = 'connection_discovery'
                           AND subject_type = 'database_target'
                           AND subject_id = ?2
                           AND json_extract(result_json, '$.kind') = 'backup_scope_object_discovery'
                           AND (
                               (COALESCE(json_extract(result_json, '$.inventoryCoverage'),'full')='full' AND ?3=0)
                               OR (json_extract(result_json, '$.inventoryCoverage')='selected' AND ?3=1 AND id=?4 AND state='succeeded')
                           )
                     )",
                    params![
                        spec.discovery_operation_run_id.as_str(),
                        spec.database_target_id.as_str(),
                        spec.workbench_selection_proof,
                        spec.id.as_str()
                    ],
                    |row| row.get(0),
                )?;
                if !discovery_valid {
                    return Err(RepositoryError::BackupScopeDiscoveryInvalid);
                }
                let (database_source_fingerprint, engine): (String, String) = transaction
                    .query_row(
                        "SELECT target.source_fingerprint, connection.engine
                         FROM database_targets AS target
                         JOIN connection_profiles AS connection
                           ON connection.id = target.connection_profile_id
                         WHERE target.id = ?1 AND target.state = 'active'",
                        [spec.database_target_id.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?
                    .ok_or(RepositoryError::NotFound)?;
                if !matches!(engine.as_str(), "postgresql" | "mysql" | "mariadb") {
                    return Err(RepositoryError::BackupScopeInputInvalid);
                }

                let mut selected = Vec::with_capacity(spec.engine_object_ids.len());
                for engine_object_id in &spec.engine_object_ids {
                    let object = transaction
                        .query_row(
                            "SELECT schema_name, object_name, object_fingerprint_blake3
                             FROM discovered_database_objects
                             WHERE operation_run_id = ?1
                               AND database_target_id = ?2
                               AND engine_object_id = ?3
                               AND object_type = 'table'
                               AND eligibility = 'eligible'",
                            params![
                                spec.discovery_operation_run_id.as_str(),
                                spec.database_target_id.as_str(),
                                engine_object_id
                            ],
                            |row| {
                                Ok((
                                    engine_object_id.clone(),
                                    row.get::<_, String>(0)?,
                                    row.get::<_, String>(1)?,
                                    row.get::<_, String>(2)?,
                                ))
                            },
                        )
                        .optional()?;
                    let Some(object) = object else {
                        return Err(RepositoryError::BackupScopeObjectMissing);
                    };
                    selected.push(object);
                }
                selected.sort_by(|left, right| left.0.cmp(&right.0));
                let selection_bytes = selected.iter().try_fold(
                    0_usize,
                    |total, (_, schema_name, object_name, _)| {
                        SelectedTable::new(schema_name, object_name)
                            .map_err(|_| RepositoryError::BackupScopeInputInvalid)?;
                        total
                            .checked_add(schema_name.len())
                            .and_then(|value| value.checked_add(object_name.len()))
                            .ok_or(RepositoryError::BackupScopeInputInvalid)
                    },
                )?;
                if selection_bytes > MAX_SELECTION_IDENTIFIER_BYTES {
                    return Err(RepositoryError::BackupScopeInputInvalid);
                }
                let selector_json = json!({
                    "mode": "explicit_tables",
                    "objects": selected
                        .iter()
                        .map(|(engine_object_id, schema_name, object_name, fingerprint)| json!({
                            "engineObjectId": engine_object_id,
                            "kind": "table",
                            "schema": schema_name,
                            "name": object_name,
                            "objectFingerprintBlake3": fingerprint,
                        }))
                        .collect::<Vec<_>>(),
                })
                .to_string();
                if selector_json.len() > MAX_SELECTOR_BYTES {
                    return Err(RepositoryError::BackupScopeInputInvalid);
                }
                let selector_hash = blake3::hash(selector_json.as_bytes()).to_hex().to_string();
                let source_fingerprint = scope_fingerprint(
                    &database_source_fingerprint,
                    BackupScopeMode::ExplicitTables,
                    &selector_hash,
                );
                let display_name = spec.display_name.trim().to_owned();
                let normalized_name = normalize_name(&display_name);
                let resolved_object_count = i64::try_from(selected.len()).unwrap_or(i64::MAX);
                let observed_at_ms: i64 = transaction.query_row(
                    "SELECT MAX(observed_at_ms)
                     FROM discovered_database_objects WHERE operation_run_id = ?1",
                    [spec.discovery_operation_run_id.as_str()],
                    |row| row.get(0),
                )?;
                let insert = transaction.execute(
                    "INSERT INTO backup_scopes(
                         id, database_target_id, display_name, normalized_name, mode, state,
                         selector_json, selector_hash_blake3, source_fingerprint,
                         database_source_fingerprint, resolved_object_count, last_resolved_at_ms,
                         created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, 'explicit_tables', 'active',
                               ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)",
                    params![
                        spec.id.as_str(),
                        spec.database_target_id.as_str(),
                        display_name,
                        normalized_name,
                        selector_json,
                        selector_hash,
                        source_fingerprint,
                        database_source_fingerprint,
                        resolved_object_count,
                        observed_at_ms,
                        spec.created_at_ms
                    ],
                );
                if let Err(error) = insert {
                    if error.to_string().contains("UNIQUE constraint failed") {
                        return Err(RepositoryError::BackupScopeConflict);
                    }
                    return Err(error.into());
                }
                for (ordinal, (engine_object_id, schema_name, object_name, fingerprint)) in
                    selected.iter().enumerate()
                {
                    transaction.execute(
                        "INSERT INTO backup_scope_objects(
                             backup_scope_id, object_kind, schema_name, object_name,
                             engine_object_id, object_fingerprint_blake3, ordinal, created_at_ms
                         ) VALUES (?1, 'table', ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            spec.id.as_str(),
                            schema_name,
                            object_name,
                            engine_object_id,
                            fingerprint,
                            i64::try_from(ordinal).unwrap_or(i64::MAX),
                            spec.created_at_ms
                        ],
                    )?;
                }
                record(&transaction, &spec.id, &source_fingerprint, &selector_hash)?;
                transaction.commit()?;
                Ok(())
            })
            .map_err(RepositoryError::from)??;
        self.get(&scope_id)
    }

    pub fn bind_revision_scope(
        &self,
        revision_id: &PlanRevisionId,
        scope_id: &BackupScopeId,
        created_at_ms: i64,
    ) -> Result<(), RepositoryError> {
        if created_at_ms < 0 {
            return Err(RepositoryError::BackupScopeInputInvalid);
        }
        let revision_id = revision_id.clone();
        let scope_id = scope_id.clone();
        self.store
            .run_write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let revision_is_draft: bool = transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM backup_plan_revisions
                         WHERE id = ?1 AND published_at_ms IS NULL
                     )",
                    [revision_id.as_str()],
                    |row| row.get(0),
                )?;
                if !revision_is_draft {
                    return Err(RepositoryError::BackupScopeConflict);
                }
                let scope: Option<(String, String, String, String, i64, String, String)> =
                    transaction
                        .query_row(
                            "SELECT scope.database_target_id, scope.mode,
                                    scope.source_fingerprint, scope.database_source_fingerprint,
                                    scope.resolved_object_count, scope.selector_json,
                                    scope.selector_hash_blake3
                             FROM backup_scopes AS scope
                             JOIN database_targets AS target ON target.id = scope.database_target_id
                             WHERE scope.id = ?1
                               AND scope.state = 'active'
                               AND target.state = 'active'
                               AND target.source_fingerprint = scope.database_source_fingerprint",
                            [scope_id.as_str()],
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
                transaction.execute(
                    "INSERT OR IGNORE INTO plan_revision_targets(
                         plan_revision_id, database_target_id, target_options_json, created_at_ms
                     ) VALUES (?1, ?2, '{}', ?3)",
                    params![revision_id.as_str(), target_id, created_at_ms],
                )?;
                transaction.execute(
                    "DELETE FROM plan_revision_scopes
                     WHERE plan_revision_id = ?1 AND database_target_id = ?2",
                    params![revision_id.as_str(), target_id],
                )?;
                transaction.execute(
                    "INSERT INTO plan_revision_scopes(
                         plan_revision_id, database_target_id, backup_scope_id,
                         scope_mode, source_fingerprint, database_source_fingerprint,
                         resolved_object_count, scope_snapshot_json,
                         scope_snapshot_hash_blake3, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        revision_id.as_str(),
                        target_id,
                        scope_id.as_str(),
                        mode,
                        source_fingerprint,
                        database_source_fingerprint,
                        resolved_object_count,
                        selector_json,
                        selector_hash,
                        created_at_ms
                    ],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .map_err(RepositoryError::from)??;
        Ok(())
    }

    pub fn get(&self, scope_id: &BackupScopeId) -> Result<BackupScopeRecord, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT id, database_target_id, display_name, mode, state,
                        selector_hash_blake3, source_fingerprint, database_source_fingerprint,
                        resolved_object_count, last_resolved_at_ms, created_at_ms, updated_at_ms
                 FROM backup_scopes WHERE id = ?1",
                [scope_id.as_str()],
                scope_from_row,
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }
}

fn scope_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BackupScopeRecord> {
    let id = parse_scope_id(row.get(0)?, 0)?;
    let database_target_id = parse_target_id(row.get(1)?, 1)?;
    let mode_text: String = row.get(3)?;
    let mode = BackupScopeMode::parse(&mode_text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(BackupScopeRecord {
        id,
        database_target_id,
        display_name: row.get(2)?,
        mode,
        state: row.get(4)?,
        selector_hash_blake3: row.get(5)?,
        source_fingerprint: row.get(6)?,
        database_source_fingerprint: row.get(7)?,
        resolved_object_count: row.get(8)?,
        last_resolved_at_ms: row.get(9)?,
        created_at_ms: row.get(10)?,
        updated_at_ms: row.get(11)?,
    })
}

fn parse_scope_id(value: String, column: usize) -> rusqlite::Result<BackupScopeId> {
    BackupScopeId::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn parse_target_id(value: String, column: usize) -> rusqlite::Result<DatabaseTargetId> {
    DatabaseTargetId::from_str(&value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}

fn scope_fingerprint(
    database_source_fingerprint: &str,
    mode: BackupScopeMode,
    selector_hash_blake3: &str,
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"restoreplane.backup-scope.v1\0");
    for value in [
        database_source_fingerprint.as_bytes(),
        mode.as_str().as_bytes(),
        selector_hash_blake3.as_bytes(),
    ] {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value);
    }
    hasher.finalize().to_hex().to_string()
}

fn normalize_name(value: &str) -> String {
    value.trim().to_lowercase()
}

fn valid_object_filter(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && value.len() <= 256 && !value.contains(['\0', '\r', '\n'])
}

fn object_search_pattern(value: &str) -> Result<String, RepositoryError> {
    if !valid_object_filter(value) {
        return Err(RepositoryError::BackupScopeInputInvalid);
    }
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('%');
    for character in value.trim().to_lowercase().chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('%');
    Ok(escaped)
}

fn is_blake3(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
