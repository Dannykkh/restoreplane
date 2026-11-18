use std::collections::{BTreeMap, BTreeSet};

use restoreplane_core::ProviderCapability;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::{OperationalStore, DEFAULT_WORKSPACE_ID};

use super::{server_identity_from_capabilities, RepositoryError};

const MAX_INVENTORY_PAGE: usize = 100;
const MAX_ACTIVITY_PAGE: usize = 500;
const MAX_CURSOR_BYTES: usize = 256;
const MAX_TARGET_SCOPE_HEALTH_ROWS: usize = 1_000;
const MAX_RESPONSE_PAGE_BYTES: usize = 768 * 1024;
const MAX_GLOBAL_INVENTORY_ROWS: usize = 25;
const MAX_ENGINE_CERTIFICATION_ROWS: usize = 10;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorPage<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
    pub total_count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleConnection {
    pub id: String,
    pub display_name: String,
    pub engine: String,
    pub transport: String,
    pub endpoint: String,
    pub state: String,
    pub last_probe_status: Option<String>,
    pub last_probe_at_ms: Option<i64>,
    pub restore_test_target: bool,
    pub updated_at_ms: i64,
    pub target_count: i64,
    /// Retained as an empty compatibility field. Targets are available only through target.list.
    pub targets: Vec<ConsoleDatabaseTarget>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleDatabaseTarget {
    pub id: String,
    pub connection_id: String,
    pub connection_name: String,
    pub engine: String,
    pub display_name: String,
    pub state: String,
    pub missing_since_ms: Option<i64>,
    pub source_fingerprint: String,
    pub latest_recovery_point_id: Option<String>,
    pub latest_success_at_ms: Option<i64>,
    pub latest_failure_at_ms: Option<i64>,
    pub artifact_completeness: String,
    pub replica_durability: String,
    pub protection_scope: String,
    pub protection_compliance: String,
    pub rpo_violation: bool,
    pub verification_state: String,
    pub verification_overdue: bool,
    pub evidence_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleBackupScopeHealth {
    pub id: String,
    pub database_target_id: String,
    pub database_name: String,
    pub connection_id: String,
    pub connection_name: String,
    pub engine: String,
    pub display_name: String,
    pub mode: String,
    pub state: String,
    pub selector_hash_blake3: String,
    pub source_fingerprint: String,
    pub database_source_fingerprint: String,
    pub selected_object_count: Option<i64>,
    pub eligible_object_count: Option<i64>,
    pub new_object_count: Option<i64>,
    pub missing_object_count: Option<i64>,
    pub changed_object_count: Option<i64>,
    pub drift_state: String,
    pub source_drift: bool,
    pub discovery_run_id: Option<String>,
    pub inventory_hash_blake3: Option<String>,
    pub inventory_observed_at_ms: Option<i64>,
    pub latest_backup_state: Option<String>,
    pub latest_recovery_point_id: Option<String>,
    pub latest_success_at_ms: Option<i64>,
    pub latest_failure_at_ms: Option<i64>,
    pub artifact_completeness: String,
    pub replica_durability: String,
    pub protection_compliance: String,
    pub restore_eligibility: String,
    pub latest_verified_level: Option<i64>,
    pub last_verified_at_ms: Option<i64>,
    pub recovery_evidence_at_ms: Option<i64>,
    pub rpo_ms: Option<i64>,
    pub rpo_state: String,
    pub rpo_violation: bool,
    pub verification_requested_level: Option<i64>,
    pub verification_state: String,
    pub verification_overdue: bool,
    pub plan_count: i64,
    pub active_plan_count: i64,
    pub retention_policy_count: i64,
    pub evidence_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSchedulerHealth {
    pub state: String,
    pub configured_plan_count: i64,
    pub observed_cursor_count: i64,
    pub active_work_count: i64,
    pub error_plan_count: i64,
    pub last_tick_at_ms: Option<i64>,
    pub last_materialized_for_ms: Option<i64>,
    pub last_error_code: Option<String>,
    pub last_error_at_ms: Option<i64>,
    pub evidence_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleRetentionHealth {
    pub state: String,
    pub configured_policy_count: i64,
    pub active_tombstone_count: i64,
    pub queued_delete_count: i64,
    pub failed_operation_count: i64,
    pub last_scan_at_ms: Option<i64>,
    pub last_error_code: Option<String>,
    pub last_error_at_ms: Option<i64>,
    pub evidence_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleDestination {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub endpoint: String,
    pub state: String,
    pub immutability_capability: String,
    pub last_probe_status: Option<String>,
    pub last_probe_at_ms: Option<i64>,
    pub updated_at_ms: i64,
    pub roles: Vec<ConsoleDestinationRole>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleDestinationRole {
    pub role: String,
    pub credential_bound: bool,
    pub probe_status: Option<String>,
    pub probed_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsolePlan {
    pub id: String,
    pub display_name: String,
    pub state: String,
    pub revision_id: Option<String>,
    pub revision_no: Option<i64>,
    pub schedule_kind: Option<String>,
    pub schedule_expression: Option<String>,
    pub timezone: Option<String>,
    pub rpo_ms: Option<i64>,
    pub published_at_ms: Option<i64>,
    pub target_count: i64,
    pub destination_count: i64,
    pub required_destination_count: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleActivityEvent {
    pub seq: i64,
    pub id: String,
    pub occurred_at_ms: i64,
    pub severity: String,
    pub event_code: String,
    pub phase: Option<String>,
    pub entity_type: String,
    pub entity_id: String,
    pub correlation_id: Option<String>,
    pub work_item_id: Option<String>,
    pub attempt_id: Option<String>,
    pub retryability: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSettings {
    pub resource_pools: Vec<ConsoleResourcePool>,
    pub spool_volumes: Vec<ConsoleSpoolVolume>,
    pub tools: Vec<ConsoleToolInventoryItem>,
    pub notification_channels: Vec<ConsoleNotificationChannel>,
    pub control_backup: ConsoleControlBackup,
    pub inventory_counts: ConsoleSettingsInventoryCounts,
    pub complete: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSettingsInventoryCounts {
    pub resource_pools: i64,
    pub spool_volumes: i64,
    pub tools: i64,
    pub notification_channels: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleResourcePool {
    pub id: String,
    pub resource_type: String,
    pub resource_key: String,
    pub capacity: i64,
    pub in_use: i64,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSpoolVolume {
    pub id: String,
    pub root_path: String,
    pub state: String,
    pub hard_limit_bytes: i64,
    pub soft_limit_bytes: i64,
    pub free_bytes: Option<i64>,
    pub probed_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleToolInventoryItem {
    pub id: String,
    pub name: String,
    pub version: String,
    pub architecture: String,
    pub trust_status: String,
    pub signature_status: String,
    pub verified_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleNotificationChannel {
    pub id: String,
    pub display_name: String,
    pub kind: String,
    pub state: String,
    pub last_test_status: Option<String>,
    pub last_test_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleControlBackup {
    pub last_completed_at_ms: Option<i64>,
    pub last_status: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEngineMatrix {
    pub generated_at_ms: i64,
    pub certification_total_count: i64,
    pub complete: bool,
    pub rows: Vec<ConsoleEngineMatrixRow>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEngineMatrixRow {
    pub certification_id: Option<String>,
    pub engine: String,
    pub server_version_range: Option<String>,
    pub tool_name: Option<String>,
    pub tool_version_range: Option<String>,
    pub operating_system: Option<String>,
    pub architecture: Option<String>,
    pub dump_format: Option<String>,
    pub consistency_grade: Option<String>,
    pub support_status: String,
    pub evidence_locator: Option<String>,
    pub evidence_checksum_blake3: Option<String>,
    pub certified_at_ms: Option<i64>,
    pub expires_at_ms: Option<i64>,
    pub capabilities: Vec<ConsoleEngineCapability>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEngineCapability {
    pub capability: String,
    pub support_status: String,
    pub limitation: Option<String>,
    pub evidence_locator: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleWorkItem {
    pub work_item_id: String,
    pub state: String,
    pub kind: String,
    pub current_attempt_no: i64,
    pub fencing_token: i64,
    pub wait_reason: Option<String>,
    pub activity_after_seq: i64,
    pub operation_run_id: Option<String>,
    pub operation_kind: Option<String>,
    pub result: Option<serde_json::Value>,
    pub error_code: Option<String>,
    #[serde(skip_serializing)]
    pub discovered: Vec<ConsoleDiscoveredDatabase>,
    pub discovered_count: i64,
    pub eligible_discovered_count: i64,
    pub subject: Option<ConsoleWorkItemSubject>,
    pub progress: Option<ConsoleWorkItemProgress>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleWorkItemSubject {
    pub plan_id: Option<String>,
    pub plan_name: Option<String>,
    pub target_id: Option<String>,
    pub target_name: Option<String>,
    pub engine: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleWorkItemProgress {
    pub phase: String,
    pub completed_units: Option<i64>,
    pub total_units: Option<i64>,
    pub unit: String,
    pub current_object: Option<String>,
    pub updated_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleDiscoveredDatabase {
    pub engine_object_id: String,
    pub database_name: String,
    pub eligibility: String,
    pub exclusion_reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ConsoleSnapshotQuery {
    pub cursor: Option<String>,
    pub limit: usize,
    pub scope: String,
    pub search: Option<String>,
    pub connection_id: Option<String>,
    pub engine: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub artifact_completeness: Option<String>,
    pub replica_durability: Option<String>,
    pub protection_compliance: Option<String>,
    pub min_verification_level: Option<i64>,
    pub restore_eligibility: Option<String>,
    pub signature_status: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSnapshot {
    pub id: String,
    pub source_target_identity: String,
    pub source_display_name: String,
    pub source_connection_id: Option<String>,
    pub source_connection_name: Option<String>,
    pub engine: String,
    pub state: String,
    pub committed_at_ms: Option<i64>,
    pub manifest_schema_version: i64,
    pub set_manifest_checksum_blake3: String,
    pub signing_key_id: String,
    pub signature_status: String,
    pub required_part_count: i64,
    pub present_part_count: i64,
    pub plaintext_bytes: Option<i64>,
    pub cipher_bytes: Option<i64>,
    pub artifact_completeness: String,
    pub replica_durability: String,
    pub protection_compliance: String,
    pub restore_eligibility: String,
    pub highest_verified_level: Option<i64>,
    pub last_verified_at_ms: Option<i64>,
    pub first_ingested_at_ms: i64,
    pub last_ingested_at_ms: i64,
    pub diagnostic_reason: Option<String>,
    pub part_count: i64,
    pub replica_count: i64,
    pub verification_evidence_count: i64,
    pub restore_drill_count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSnapshotReplica {
    pub id: String,
    pub destination_fingerprint: String,
    pub destination_kind: String,
    pub requirement: String,
    pub state: String,
    pub immutability_capability: String,
    pub retention_until_ms: Option<i64>,
    pub reader_status: String,
    pub observed_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSnapshotPart {
    pub id: String,
    pub role: String,
    pub logical_name: String,
    pub required: bool,
    pub dump_format: String,
    pub plaintext_bytes: Option<i64>,
    pub checksum_blake3: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ConsoleRestoreTargetFacts {
    pub connection_id: String,
    pub connection_name: String,
    pub engine: String,
    pub connection_state: String,
    pub credential_bound: bool,
    pub probe_status: Option<String>,
    pub server_version: Option<String>,
    pub server_identity: Option<String>,
    pub probed_at_ms: Option<i64>,
    pub restore_test_target: bool,
    pub target_exists: Option<bool>,
    pub available_spool_bytes: Option<i64>,
    pub spool_observed_at_ms: Option<i64>,
}

#[derive(Clone)]
pub struct ConsoleQueryRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn console_query(&self) -> ConsoleQueryRepository {
        ConsoleQueryRepository {
            store: self.clone(),
        }
    }
}

impl ConsoleQueryRepository {
    pub fn connection(&self, id: &str) -> Result<ConsoleConnection, RepositoryError> {
        self.connection_in_workspace(DEFAULT_WORKSPACE_ID, id)
    }

    pub fn connection_in_workspace(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<ConsoleConnection, RepositoryError> {
        self.connection_in_workspace_at(workspace_id, id, unix_time_ms()?)
    }

    pub fn connection_in_workspace_at(
        &self,
        workspace_id: &str,
        id: &str,
        now_ms: i64,
    ) -> Result<ConsoleConnection, RepositoryError> {
        if !valid_workspace_id(workspace_id)
            || id.is_empty()
            || id.len() > MAX_CURSOR_BYTES
            || id.contains(['\0', '\r', '\n'])
            || now_ms < 0
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let item = connection
            .query_row(
                "SELECT profile.id, profile.display_name, profile.engine, profile.transport,
                        profile.host, profile.port, profile.file_path,
                        state, last_probe_status, last_probe_at_ms, updated_at_ms,
                        adapter_config_json,
                        (SELECT COUNT(*) FROM database_targets AS target
                         WHERE target.connection_profile_id = profile.id)
                 FROM connection_profiles AS profile
                 JOIN workspace_connection_profiles AS ownership
                   ON ownership.connection_profile_id = profile.id
                 WHERE ownership.workspace_id = ?1
                   AND profile.id = ?2
                   AND profile.deleted_at_ms IS NULL",
                params![workspace_id, id],
                |row| {
                    let host: Option<String> = row.get(4)?;
                    let port: Option<i64> = row.get(5)?;
                    let file_path: Option<String> = row.get(6)?;
                    Ok(ConsoleConnection {
                        id: row.get(0)?,
                        display_name: row.get(1)?,
                        engine: row.get(2)?,
                        transport: row.get(3)?,
                        endpoint: endpoint_for_connection(
                            host.as_deref(),
                            port,
                            file_path.as_deref(),
                        ),
                        state: row.get(7)?,
                        last_probe_status: row.get(8)?,
                        last_probe_at_ms: row.get(9)?,
                        restore_test_target: restore_test_target_from_config(
                            &row.get::<_, String>(11)?,
                        ),
                        updated_at_ms: row.get(10)?,
                        target_count: row.get(12)?,
                        targets: Vec::new(),
                    })
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)?;
        Ok(item)
    }

    pub fn connections(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<CursorPage<ConsoleConnection>, RepositoryError> {
        self.connections_in_workspace(DEFAULT_WORKSPACE_ID, cursor, limit)
    }

    pub fn connections_in_workspace(
        &self,
        workspace_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<CursorPage<ConsoleConnection>, RepositoryError> {
        self.connections_in_workspace_at(workspace_id, cursor, limit, unix_time_ms()?)
    }

    pub fn connections_in_workspace_at(
        &self,
        workspace_id: &str,
        cursor: Option<&str>,
        limit: usize,
        now_ms: i64,
    ) -> Result<CursorPage<ConsoleConnection>, RepositoryError> {
        validate_inventory_page(cursor, limit)?;
        if !valid_workspace_id(workspace_id) || now_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let total_count = connection.query_row(
            "SELECT COUNT(*)
             FROM connection_profiles AS profile
             JOIN workspace_connection_profiles AS ownership
               ON ownership.connection_profile_id = profile.id
             WHERE ownership.workspace_id = ?1
               AND profile.deleted_at_ms IS NULL",
            [workspace_id],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT profile.id, profile.display_name, profile.engine, profile.transport,
                    profile.host, profile.port, profile.file_path,
                    state, last_probe_status, last_probe_at_ms, updated_at_ms,
                    adapter_config_json,
                    (SELECT COUNT(*) FROM database_targets AS target
                     WHERE target.connection_profile_id = profile.id)
             FROM connection_profiles AS profile
             JOIN workspace_connection_profiles AS ownership
               ON ownership.connection_profile_id = profile.id
             WHERE ownership.workspace_id = ?1
               AND profile.deleted_at_ms IS NULL
               AND (?2 IS NULL OR profile.id > ?2)
             ORDER BY profile.id
             LIMIT ?3",
        )?;
        let mut items = statement
            .query_map(
                params![workspace_id, cursor, page_fetch_limit(limit)?],
                |row| {
                    let host: Option<String> = row.get(4)?;
                    let port: Option<i64> = row.get(5)?;
                    let file_path: Option<String> = row.get(6)?;
                    let endpoint =
                        endpoint_for_connection(host.as_deref(), port, file_path.as_deref());
                    Ok(ConsoleConnection {
                        id: row.get(0)?,
                        display_name: row.get(1)?,
                        engine: row.get(2)?,
                        transport: row.get(3)?,
                        endpoint,
                        state: row.get(7)?,
                        last_probe_status: row.get(8)?,
                        last_probe_at_ms: row.get(9)?,
                        restore_test_target: restore_test_target_from_config(
                            &row.get::<_, String>(11)?,
                        ),
                        updated_at_ms: row.get(10)?,
                        target_count: row.get(12)?,
                        targets: Vec::new(),
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = trim_page(&mut items, limit, |item| item.id.clone());
        Ok(CursorPage {
            items,
            next_cursor,
            total_count,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn database_targets_in_workspace_at(
        &self,
        workspace_id: &str,
        cursor: Option<&str>,
        limit: usize,
        connection_id: Option<&str>,
        state: Option<&str>,
        search: Option<&str>,
        now_ms: i64,
    ) -> Result<CursorPage<ConsoleDatabaseTarget>, RepositoryError> {
        validate_target_page_input(
            workspace_id,
            cursor,
            limit,
            connection_id,
            state,
            search,
            now_ms,
        )?;
        let cursor = cursor.map(parse_created_cursor).transpose()?;
        let search_pattern = search.map(scope_search_pattern).transpose()?;
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let total_count = connection.query_row(
            "SELECT COUNT(*)
             FROM database_targets AS target
             JOIN connection_profiles AS profile
               ON profile.id = target.connection_profile_id
             JOIN workspace_connection_profiles AS ownership
               ON ownership.connection_profile_id = profile.id
             WHERE ownership.workspace_id = ?1
               AND profile.deleted_at_ms IS NULL
               AND (?2 IS NULL OR profile.id = ?2)
               AND (?3 IS NULL OR target.state = ?3)
               AND (?4 IS NULL OR lower(
                    target.display_name || ' ' || target.engine_object_id || ' ' ||
                    profile.display_name || ' ' || profile.engine
               ) LIKE ?4 ESCAPE '\\')",
            params![workspace_id, connection_id, state, search_pattern],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT target.id, profile.id, profile.display_name, profile.engine,
                    target.display_name, target.state, target.missing_since_ms,
                    target.source_fingerprint, target.created_at_ms
             FROM database_targets AS target
             JOIN connection_profiles AS profile
               ON profile.id = target.connection_profile_id
             JOIN workspace_connection_profiles AS ownership
               ON ownership.connection_profile_id = profile.id
             WHERE ownership.workspace_id = ?1
               AND profile.deleted_at_ms IS NULL
               AND (?2 IS NULL OR profile.id = ?2)
               AND (?3 IS NULL OR target.state = ?3)
               AND (?4 IS NULL OR lower(
                    target.display_name || ' ' || target.engine_object_id || ' ' ||
                    profile.display_name || ' ' || profile.engine
               ) LIKE ?4 ESCAPE '\\')
               AND (
                    ?5 IS NULL OR target.created_at_ms > ?5
                    OR (target.created_at_ms = ?5 AND target.id > ?6)
               )
             ORDER BY target.created_at_ms, target.id
             LIMIT ?7",
        )?;
        let (cursor_created_at_ms, cursor_id) = cursor
            .as_ref()
            .map(|(created_at_ms, id)| (Some(*created_at_ms), Some(id.as_str())))
            .unwrap_or((None, None));
        let mut rows = statement
            .query_map(
                params![
                    workspace_id,
                    connection_id,
                    state,
                    search_pattern,
                    cursor_created_at_ms,
                    cursor_id,
                    page_fetch_limit(limit)?,
                ],
                |row| {
                    Ok((
                        ConsoleDatabaseTarget {
                            id: row.get(0)?,
                            connection_id: row.get(1)?,
                            connection_name: row.get(2)?,
                            engine: row.get(3)?,
                            display_name: row.get(4)?,
                            state: row.get(5)?,
                            missing_since_ms: row.get(6)?,
                            source_fingerprint: row.get(7)?,
                            latest_recovery_point_id: None,
                            latest_success_at_ms: None,
                            latest_failure_at_ms: None,
                            artifact_completeness: "unknown".to_owned(),
                            replica_durability: "unknown".to_owned(),
                            protection_scope: "none".to_owned(),
                            protection_compliance: "not_configured".to_owned(),
                            rpo_violation: false,
                            verification_state: "not_configured".to_owned(),
                            verification_overdue: false,
                            evidence_at_ms: None,
                        },
                        row.get::<_, i64>(8)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let has_more = rows.len() > limit;
        if has_more {
            rows.truncate(limit);
        }
        let target_ids = rows
            .iter()
            .map(|(target, _)| target.id.clone())
            .collect::<Vec<_>>();
        let (scope_health, scope_health_complete) =
            self.bounded_scope_health_for_targets(workspace_id, &target_ids, now_ms)?;
        for (target, _) in &mut rows {
            if scope_health_complete {
                roll_up_target_health(target, &scope_health);
            } else {
                target.protection_scope = "unknown".to_owned();
                target.protection_compliance = "unknown".to_owned();
                target.verification_state = "unknown".to_owned();
            }
        }
        let next_cursor = has_more.then(|| {
            let (target, created_at_ms) = rows.last().expect("non-empty page with lookahead");
            created_cursor(*created_at_ms, &target.id)
        });
        Ok(CursorPage {
            items: rows.into_iter().map(|(target, _)| target).collect(),
            next_cursor,
            total_count,
        })
    }

    pub fn destinations(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<CursorPage<ConsoleDestination>, RepositoryError> {
        self.destinations_in_workspace(DEFAULT_WORKSPACE_ID, cursor, limit)
    }

    pub fn destinations_in_workspace(
        &self,
        workspace_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<CursorPage<ConsoleDestination>, RepositoryError> {
        validate_inventory_page(cursor, limit)?;
        if !valid_workspace_id(workspace_id) {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let total_count = connection.query_row(
            "SELECT COUNT(*)
             FROM destinations AS destination
             JOIN workspace_destinations AS ownership
               ON ownership.destination_id = destination.id
             WHERE ownership.workspace_id = ?1
               AND destination.deleted_at_ms IS NULL",
            [workspace_id],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT id, display_name, kind, endpoint_json, state,
                    immutability_capability, last_probe_status,
                    last_probe_at_ms, updated_at_ms
             FROM destinations AS destination
             JOIN workspace_destinations AS ownership
               ON ownership.destination_id = destination.id
             WHERE ownership.workspace_id = ?1
               AND destination.deleted_at_ms IS NULL
               AND (?2 IS NULL OR destination.id > ?2)
             ORDER BY destination.id
             LIMIT ?3",
        )?;
        let mut items = statement
            .query_map(
                params![workspace_id, cursor, page_fetch_limit(limit)?],
                |row| {
                    let kind: String = row.get(2)?;
                    let endpoint_json: String = row.get(3)?;
                    Ok(ConsoleDestination {
                        id: row.get(0)?,
                        display_name: row.get(1)?,
                        endpoint: public_destination_endpoint(&kind, &endpoint_json),
                        kind,
                        state: row.get(4)?,
                        immutability_capability: row.get(5)?,
                        last_probe_status: row.get(6)?,
                        last_probe_at_ms: row.get(7)?,
                        updated_at_ms: row.get(8)?,
                        roles: Vec::new(),
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        for item in &mut items {
            item.roles = destination_roles(connection, &item.id)?;
        }
        let next_cursor = trim_page(&mut items, limit, |item| item.id.clone());
        Ok(CursorPage {
            items,
            next_cursor,
            total_count,
        })
    }

    pub fn plans(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<CursorPage<ConsolePlan>, RepositoryError> {
        self.plans_in_workspace(DEFAULT_WORKSPACE_ID, cursor, limit)
    }

    pub fn plans_in_workspace(
        &self,
        workspace_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<CursorPage<ConsolePlan>, RepositoryError> {
        validate_inventory_page(cursor, limit)?;
        if !valid_workspace_id(workspace_id) {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let total_count = connection.query_row(
            "SELECT COUNT(*)
             FROM backup_plans AS plans
             JOIN workspace_backup_plans AS ownership ON ownership.backup_plan_id = plans.id
             WHERE ownership.workspace_id = ?1 AND plans.deleted_at_ms IS NULL",
            [workspace_id],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT plans.id, plans.display_name, plans.state,
                    revisions.id, revisions.revision_no, revisions.schedule_kind,
                    revisions.schedule_expression, revisions.timezone,
                    revisions.rpo_ms, revisions.published_at_ms,
                    (
                        SELECT COUNT(*) FROM plan_revision_targets AS targets
                        WHERE targets.plan_revision_id = revisions.id
                    ),
                    (
                        SELECT COUNT(*) FROM plan_revision_destinations AS destinations
                        WHERE destinations.plan_revision_id = revisions.id
                    ),
                    (
                        SELECT COUNT(*) FROM plan_revision_destinations AS destinations
                        WHERE destinations.plan_revision_id = revisions.id
                          AND destinations.requirement = 'required'
                    ),
                    plans.updated_at_ms
             FROM backup_plans AS plans
             JOIN workspace_backup_plans AS ownership ON ownership.backup_plan_id = plans.id
             LEFT JOIN backup_plan_revisions AS revisions
               ON revisions.id = plans.current_revision_id
             WHERE ownership.workspace_id = ?1
               AND plans.deleted_at_ms IS NULL
               AND (?2 IS NULL OR plans.id > ?2)
             ORDER BY plans.id
             LIMIT ?3",
        )?;
        let mut items = statement
            .query_map(
                params![workspace_id, cursor, page_fetch_limit(limit)?],
                |row| {
                    Ok(ConsolePlan {
                        id: row.get(0)?,
                        display_name: row.get(1)?,
                        state: row.get(2)?,
                        revision_id: row.get(3)?,
                        revision_no: row.get(4)?,
                        schedule_kind: row.get(5)?,
                        schedule_expression: row.get(6)?,
                        timezone: row.get(7)?,
                        rpo_ms: row.get(8)?,
                        published_at_ms: row.get(9)?,
                        target_count: row.get(10)?,
                        destination_count: row.get(11)?,
                        required_destination_count: row.get(12)?,
                        updated_at_ms: row.get(13)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = trim_page(&mut items, limit, |item| item.id.clone());
        Ok(CursorPage {
            items,
            next_cursor,
            total_count,
        })
    }

    pub fn backup_scopes_in_workspace(
        &self,
        workspace_id: &str,
        database_target_id: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<ConsoleBackupScopeHealth>, RepositoryError> {
        self.backup_scopes_page_in_workspace(
            workspace_id,
            database_target_id,
            None,
            MAX_INVENTORY_PAGE,
            None,
            None,
            now_ms,
        )
        .map(|page| page.items)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn backup_scopes_page_in_workspace(
        &self,
        workspace_id: &str,
        database_target_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        state: Option<&str>,
        search: Option<&str>,
        now_ms: i64,
    ) -> Result<CursorPage<ConsoleBackupScopeHealth>, RepositoryError> {
        validate_scope_page_input(
            workspace_id,
            database_target_id,
            cursor,
            limit,
            state,
            search,
            now_ms,
        )?;
        let cursor = cursor.map(parse_scope_cursor).transpose()?;
        let search_pattern = search.map(scope_search_pattern).transpose()?;
        let reader = self.store.open_reader()?;
        let total_count = reader.connection().query_row(
            "SELECT COUNT(*)
             FROM backup_scopes AS scope
             JOIN database_targets AS target ON target.id = scope.database_target_id
             JOIN connection_profiles AS connection ON connection.id = target.connection_profile_id
             JOIN workspace_connection_profiles AS ownership
               ON ownership.connection_profile_id = connection.id
             WHERE ownership.workspace_id = ?1
               AND (?2 IS NULL OR target.id = ?2)
               AND (?3 IS NULL OR scope.state = ?3)
               AND (?4 IS NULL OR lower(
                    scope.display_name || ' ' || target.display_name || ' ' ||
                    connection.display_name || ' ' || connection.engine
               ) LIKE ?4 ESCAPE '\\')",
            params![workspace_id, database_target_id, state, search_pattern],
            |row| row.get(0),
        )?;
        drop(reader);
        let mut rows = self.backup_scope_health_rows(
            workspace_id,
            database_target_id,
            None,
            None,
            cursor.as_ref(),
            page_fetch_limit(limit)?,
            state,
            search_pattern.as_deref(),
            now_ms,
        )?;
        let next_cursor = if rows.len() > limit {
            rows.truncate(limit);
            rows.last()
                .map(|(scope, created_at_ms)| scope_cursor(*created_at_ms, &scope.id))
        } else {
            None
        };
        Ok(CursorPage {
            items: rows.into_iter().map(|(scope, _)| scope).collect(),
            next_cursor,
            total_count,
        })
    }

    fn bounded_scope_health_for_targets(
        &self,
        workspace_id: &str,
        target_ids: &[String],
        now_ms: i64,
    ) -> Result<(Vec<ConsoleBackupScopeHealth>, bool), RepositoryError> {
        if target_ids.is_empty() {
            return Ok((Vec::new(), true));
        }
        if target_ids.len() > MAX_INVENTORY_PAGE
            || target_ids.iter().any(|id| !valid_filter_text(id))
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let target_ids_json =
            serde_json::to_string(target_ids).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        let fetch_limit = i64::try_from(MAX_TARGET_SCOPE_HEALTH_ROWS.saturating_add(1))
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        let rows = self.backup_scope_health_rows(
            workspace_id,
            None,
            None,
            Some(&target_ids_json),
            None,
            fetch_limit,
            None,
            None,
            now_ms,
        )?;
        if rows.len() > MAX_TARGET_SCOPE_HEALTH_ROWS {
            return Ok((Vec::new(), false));
        }
        Ok((rows.into_iter().map(|(scope, _)| scope).collect(), true))
    }

    #[allow(clippy::too_many_arguments)]
    fn backup_scope_health_rows(
        &self,
        workspace_id: &str,
        database_target_id: Option<&str>,
        connection_ids_json: Option<&str>,
        target_ids_json: Option<&str>,
        cursor: Option<&(i64, String)>,
        fetch_limit: i64,
        state: Option<&str>,
        search_pattern: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<(ConsoleBackupScopeHealth, i64)>, RepositoryError> {
        if !valid_workspace_id(workspace_id)
            || now_ms < 0
            || fetch_limit <= 0
            || database_target_id.is_some_and(|value| {
                value.is_empty()
                    || value.len() > MAX_CURSOR_BYTES
                    || value.contains(['\0', '\r', '\n'])
            })
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let mut statement = connection.prepare(
            "WITH ranked_discovery AS (
                 SELECT operation.id AS discovery_run_id,
                        operation.subject_id AS database_target_id,
                        json_extract(operation.result_json, '$.inventoryHashBlake3') AS inventory_hash,
                        json_extract(operation.result_json, '$.observedAtMs') AS observed_at_ms,
                        ROW_NUMBER() OVER (
                            PARTITION BY operation.subject_id
                            ORDER BY json_extract(operation.result_json, '$.observedAtMs') DESC,
                                     operation.created_at_ms DESC,
                                     operation.id DESC
                        ) AS rank
                 FROM operation_runs AS operation
                 WHERE operation.operation_kind = 'connection_discovery'
                   AND operation.subject_type = 'database_target'
                   AND json_extract(operation.result_json, '$.kind') = 'backup_scope_object_discovery'
                   AND operation.state = 'succeeded'
                   AND COALESCE(json_extract(operation.result_json, '$.inventoryCoverage'),'full')='full'
                   AND operation.subject_id IN (
                       SELECT target.id FROM database_targets target
                       JOIN workspace_connection_profiles ownership ON ownership.connection_profile_id=target.connection_profile_id
                       WHERE ownership.workspace_id=?1 AND (?2 IS NULL OR target.id=?2)
                         AND (?4 IS NULL OR target.connection_profile_id IN (SELECT value FROM json_each(?4)))
                   )
                   AND (?5 IS NULL OR operation.subject_id IN (SELECT value FROM json_each(?5)))
             ),
             latest_discovery AS (
                 SELECT discovery_run_id, database_target_id, inventory_hash, observed_at_ms
                 FROM ranked_discovery
                 WHERE rank = 1
             ),
             bound_scope_discovery AS (
                 SELECT scope.id AS backup_scope_id, operation.id AS discovery_run_id,
                        json_extract(operation.result_json,'$.inventoryHashBlake3') AS inventory_hash,
                        json_extract(operation.result_json,'$.observedAtMs') AS observed_at_ms
                 FROM backup_scopes scope
                 JOIN operation_runs operation ON operation.id=scope.id
                 JOIN database_targets target ON target.id=scope.database_target_id
                 JOIN workspace_connection_profiles ownership ON ownership.connection_profile_id=target.connection_profile_id
                 WHERE ownership.workspace_id=?1
                   AND (?2 IS NULL OR target.id=?2)
                   AND (?4 IS NULL OR target.connection_profile_id IN (SELECT value FROM json_each(?4)))
                   AND (?5 IS NULL OR target.id IN (SELECT value FROM json_each(?5)))
                   AND scope.mode='explicit_tables'
                   AND operation.operation_kind='connection_discovery'
                   AND operation.subject_type='database_target'
                   AND operation.subject_id=scope.database_target_id
                   AND operation.state='succeeded'
                   AND length(operation.result_json)<=1048576
                   AND json_extract(operation.result_json,'$.kind')='backup_scope_object_discovery'
                   AND json_extract(operation.result_json,'$.inventoryCoverage')='selected'
                   AND json_extract(operation.request_json,'$.workbenchAuthority.scope.workspaceId')=?1
                   AND json_extract(operation.request_json,'$.workbenchAuthority.scope.databaseTargetId')=target.id
                   AND json_extract(operation.request_json,'$.workbenchAuthority.scope.connectionProfileId')=target.connection_profile_id
                   AND json_extract(operation.result_json,'$.observedAtMs')=scope.last_resolved_at_ms
                   AND json_array_length(operation.result_json,'$.workbenchScopeLinks') BETWEEN 1 AND 100
                   AND (SELECT COUNT(*) FROM json_each(operation.result_json,'$.workbenchScopeLinks') link
                        WHERE json_extract(link.value,'$.scopeId')=scope.id)=1
                   AND EXISTS (
                       SELECT 1 FROM json_each(operation.result_json,'$.workbenchScopeLinks') link
                       WHERE json_extract(link.value,'$.scopeId')=scope.id
                         AND json_extract(link.value,'$.scopeFingerprint')=scope.source_fingerprint
                         AND json_extract(link.value,'$.selectorHash')=scope.selector_hash_blake3
                         AND json_extract(link.value,'$.selectionDigest')=json_extract(operation.result_json,'$.workbenchSelection.selectionDigest')
                         AND length(json_extract(link.value,'$.selectionDigest'))=64
                   )
             ),
             scope_discovery AS (
                 SELECT scope.id AS backup_scope_id,
                        COALESCE(newest.discovery_run_id,bound.discovery_run_id) AS discovery_run_id,
                        COALESCE(newest.inventory_hash,bound.inventory_hash) AS inventory_hash,
                        COALESCE(newest.observed_at_ms,bound.observed_at_ms) AS observed_at_ms,
                        newest.discovery_run_id IS NULL AND bound.discovery_run_id IS NOT NULL AS selected_coverage
                 FROM backup_scopes scope
                 LEFT JOIN bound_scope_discovery bound ON bound.backup_scope_id=scope.id
                 LEFT JOIN latest_discovery newest ON newest.database_target_id=scope.database_target_id
                  AND (scope.last_resolved_at_ms IS NULL OR newest.observed_at_ms>scope.last_resolved_at_ms
                       OR (bound.discovery_run_id IS NULL AND newest.observed_at_ms=scope.last_resolved_at_ms))
             ),
             ranked_scope_baseline AS (
                 SELECT scope.id AS backup_scope_id,
                        operation.id AS discovery_run_id,
                        ROW_NUMBER() OVER (
                            PARTITION BY scope.id
                            ORDER BY json_extract(operation.result_json, '$.observedAtMs') DESC,
                                     operation.created_at_ms DESC,
                                     operation.id DESC
                        ) AS rank
                 FROM backup_scopes AS scope
                 JOIN operation_runs AS operation
                   ON operation.subject_id = scope.database_target_id
                  AND operation.operation_kind = 'connection_discovery'
                  AND operation.subject_type = 'database_target'
                  AND json_extract(operation.result_json, '$.kind') = 'backup_scope_object_discovery'
                  AND operation.state='succeeded'
                  AND COALESCE(json_extract(operation.result_json, '$.inventoryCoverage'),'full')='full'
                  AND json_extract(operation.result_json, '$.observedAtMs') <= scope.last_resolved_at_ms
                 WHERE scope.mode = 'explicit_tables'
                   AND scope.last_resolved_at_ms IS NOT NULL
                   AND scope.database_target_id IN (
                       SELECT target.id FROM database_targets target
                       JOIN workspace_connection_profiles ownership ON ownership.connection_profile_id=target.connection_profile_id
                       WHERE ownership.workspace_id=?1 AND (?2 IS NULL OR target.id=?2)
                         AND (?4 IS NULL OR target.connection_profile_id IN (SELECT value FROM json_each(?4)))
                   )
                   AND (?5 IS NULL OR scope.database_target_id IN (SELECT value FROM json_each(?5)))
             ),
             scope_baseline AS (
                 SELECT backup_scope_id, discovery_run_id
                 FROM ranked_scope_baseline
                 WHERE rank = 1
             ),
             scope_policy AS (
                 SELECT binding.backup_scope_id,
                        plan.id AS plan_id,
                        plan.state AS plan_state,
                        revision.published_at_ms,
                        revision.rpo_ms,
                        revision.retention_policy_json,
                        CASE
                            WHEN json_type(revision.verification_policy_json, '$.requestedLevel') = 'integer'
                             AND json_extract(revision.verification_policy_json, '$.requestedLevel') BETWEEN 1 AND 3
                             AND json_type(revision.verification_policy_json, '$.level') IS NULL
                            THEN CAST(json_extract(revision.verification_policy_json, '$.requestedLevel') AS INTEGER)
                            WHEN json_type(revision.verification_policy_json, '$.requestedLevel') IS NULL
                             AND lower(CAST(json_extract(revision.verification_policy_json, '$.level') AS TEXT)) IN ('l1', '1')
                            THEN 1
                            WHEN json_type(revision.verification_policy_json, '$.requestedLevel') IS NULL
                             AND lower(CAST(json_extract(revision.verification_policy_json, '$.level') AS TEXT)) IN ('l2', '2')
                            THEN 2
                            WHEN json_type(revision.verification_policy_json, '$.requestedLevel') IS NULL
                             AND lower(CAST(json_extract(revision.verification_policy_json, '$.level') AS TEXT)) IN ('l3', '3')
                            THEN 3
                            WHEN json_type(revision.verification_policy_json, '$.requestedLevel') IS NULL
                             AND json_type(revision.verification_policy_json, '$.level') IS NULL
                            THEN 2
                            ELSE NULL
                        END AS verification_requested_level,
                        CASE
                            WHEN json_type(revision.verification_policy_json, '$.schedule') IS NULL
                            THEN 'every_run'
                            WHEN json_type(revision.verification_policy_json, '$.schedule') = 'text'
                            THEN json_extract(revision.verification_policy_json, '$.schedule')
                            ELSE NULL
                        END AS verification_schedule
                 FROM plan_revision_scopes AS binding
                 JOIN backup_plan_revisions AS revision ON revision.id = binding.plan_revision_id
                 JOIN backup_plans AS plan ON plan.current_revision_id = revision.id
                 JOIN backup_scopes AS filtered_scope ON filtered_scope.id = binding.backup_scope_id
                 WHERE plan.deleted_at_ms IS NULL AND plan.state <> 'retired'
                   AND (?5 IS NULL OR filtered_scope.database_target_id IN (SELECT value FROM json_each(?5)))
             ),
             scope_plan AS (
                 SELECT policy.backup_scope_id,
                        COUNT(DISTINCT plan.id) AS plan_count,
                        COUNT(DISTINCT CASE
                            WHEN policy.plan_state = 'enabled'
                             AND policy.published_at_ms IS NOT NULL
                            THEN policy.plan_id
                        END) AS active_plan_count,
                        COUNT(DISTINCT CASE
                            WHEN COALESCE(CAST(json_extract(policy.retention_policy_json, '$.daily') AS INTEGER), 0)
                               + COALESCE(CAST(json_extract(policy.retention_policy_json, '$.weekly') AS INTEGER), 0)
                               + COALESCE(CAST(json_extract(policy.retention_policy_json, '$.monthly') AS INTEGER), 0)
                               + COALESCE(CAST(json_extract(policy.retention_policy_json, '$.keepLast') AS INTEGER), 0) > 0
                            THEN policy.plan_id
                        END) AS retention_policy_count,
                        MIN(CASE
                            WHEN policy.plan_state = 'enabled' AND policy.published_at_ms IS NOT NULL
                            THEN policy.rpo_ms
                        END) AS rpo_ms,
                        MAX(CASE
                            WHEN policy.plan_state = 'enabled' AND policy.published_at_ms IS NOT NULL
                            THEN policy.verification_requested_level
                        END) AS verification_requested_level,
                        MAX(CASE
                            WHEN policy.plan_state = 'enabled'
                             AND policy.published_at_ms IS NOT NULL
                             AND (
                                 policy.verification_schedule IS NULL
                                 OR policy.verification_schedule NOT IN ('every_run', 'weekly')
                             )
                            THEN 1 ELSE 0
                        END) AS verification_schedule_unknown
                 FROM scope_policy AS policy
                 JOIN backup_plans AS plan ON plan.id = policy.plan_id
                 GROUP BY policy.backup_scope_id
             ),
             ranked_run AS (
                 SELECT snapshot.backup_scope_id, run.state, run.created_at_ms,
                        ROW_NUMBER() OVER (
                            PARTITION BY snapshot.backup_scope_id
                            ORDER BY run.created_at_ms DESC, run.id DESC
                        ) AS rank
                 FROM backup_run_scopes AS snapshot
                 JOIN backup_runs AS run ON run.id = snapshot.backup_run_id
                 JOIN backup_scopes AS filtered_scope ON filtered_scope.id = snapshot.backup_scope_id
                 WHERE ?5 IS NULL
                    OR filtered_scope.database_target_id IN (SELECT value FROM json_each(?5))
             ),
             latest_run AS (
                 SELECT backup_scope_id, state FROM ranked_run WHERE rank = 1
             ),
             ranked_success AS (
                 SELECT snapshot.backup_scope_id, run.recovery_point_id,
                        COALESCE(run.completed_at_ms, run.updated_at_ms) AS succeeded_at_ms,
                        ROW_NUMBER() OVER (
                            PARTITION BY snapshot.backup_scope_id
                            ORDER BY COALESCE(run.completed_at_ms, run.updated_at_ms) DESC, run.id DESC
                        ) AS rank
                 FROM backup_run_scopes AS snapshot
                 JOIN backup_runs AS run ON run.id = snapshot.backup_run_id
                 JOIN backup_scopes AS filtered_scope ON filtered_scope.id = snapshot.backup_scope_id
                 WHERE run.state IN ('succeeded', 'partially_succeeded')
                   AND run.recovery_point_id IS NOT NULL
                   AND (?5 IS NULL OR filtered_scope.database_target_id IN (SELECT value FROM json_each(?5)))
             ),
             latest_success AS (
                 SELECT backup_scope_id, recovery_point_id, succeeded_at_ms
                 FROM ranked_success WHERE rank = 1
             ),
             latest_failure AS (
                 SELECT snapshot.backup_scope_id,
                        MAX(COALESCE(run.completed_at_ms, run.updated_at_ms)) AS failed_at_ms
                 FROM backup_run_scopes AS snapshot
                 JOIN backup_runs AS run ON run.id = snapshot.backup_run_id
                 JOIN backup_scopes AS filtered_scope ON filtered_scope.id = snapshot.backup_scope_id
                 WHERE run.state = 'failed'
                   AND (?5 IS NULL OR filtered_scope.database_target_id IN (SELECT value FROM json_each(?5)))
                 GROUP BY snapshot.backup_scope_id
             )
             SELECT scope.id, target.id, target.display_name,
                    connection.id, connection.display_name, connection.engine,
                    scope.display_name, scope.mode, scope.state,
                    scope.selector_hash_blake3, scope.source_fingerprint,
                    scope.database_source_fingerprint,
                    CASE WHEN scope.mode = 'whole_database' THEN NULL ELSE (
                        SELECT COUNT(*) FROM backup_scope_objects AS selected
                        WHERE selected.backup_scope_id = scope.id
                    ) END AS selected_object_count,
                    CASE WHEN discovery.discovery_run_id IS NULL THEN NULL ELSE (
                        SELECT COUNT(*) FROM discovered_database_objects AS observed
                        WHERE observed.operation_run_id = discovery.discovery_run_id
                          AND observed.eligibility = 'eligible'
                    ) END AS eligible_object_count,
                    CASE
                        WHEN discovery.discovery_run_id IS NULL THEN NULL
                        WHEN scope.mode = 'whole_database' THEN 0
                        WHEN discovery.selected_coverage THEN 0
                        WHEN baseline.discovery_run_id IS NULL THEN NULL
                        ELSE (
                            SELECT COUNT(*) FROM discovered_database_objects AS observed
                            WHERE observed.operation_run_id = discovery.discovery_run_id
                              AND observed.eligibility = 'eligible'
                              AND NOT EXISTS (
                                  SELECT 1 FROM backup_scope_objects AS selected
                                  WHERE selected.backup_scope_id = scope.id
                                    AND selected.engine_object_id = observed.engine_object_id
                              )
                              AND NOT EXISTS (
                                  SELECT 1 FROM discovered_database_objects AS baseline_object
                                  WHERE baseline_object.operation_run_id = baseline.discovery_run_id
                                    AND baseline_object.engine_object_id = observed.engine_object_id
                                    AND baseline_object.eligibility = 'eligible'
                              )
                        )
                    END AS new_object_count,
                    CASE
                        WHEN discovery.discovery_run_id IS NULL THEN NULL
                        WHEN scope.mode = 'whole_database' THEN 0
                        ELSE (
                            SELECT COUNT(*) FROM backup_scope_objects AS selected
                            WHERE selected.backup_scope_id = scope.id
                              AND NOT EXISTS (
                                  SELECT 1 FROM discovered_database_objects AS observed
                                  WHERE observed.operation_run_id = discovery.discovery_run_id
                                    AND observed.engine_object_id = selected.engine_object_id
                                    AND observed.eligibility = 'eligible'
                              )
                        )
                    END AS missing_object_count,
                    CASE
                        WHEN discovery.discovery_run_id IS NULL THEN NULL
                        WHEN scope.mode = 'whole_database' THEN 0
                        ELSE (
                            SELECT COUNT(*) FROM backup_scope_objects AS selected
                            JOIN discovered_database_objects AS observed
                              ON observed.operation_run_id = discovery.discovery_run_id
                             AND observed.engine_object_id = selected.engine_object_id
                             AND observed.eligibility = 'eligible'
                            WHERE selected.backup_scope_id = scope.id
                              AND selected.object_fingerprint_blake3 <> observed.object_fingerprint_blake3
                        )
                    END AS changed_object_count,
                    scope.database_source_fingerprint <> target.source_fingerprint AS source_drift,
                    discovery.discovery_run_id, discovery.inventory_hash, discovery.observed_at_ms,
                    latest_run.state, latest_success.recovery_point_id,
                    latest_success.succeeded_at_ms, latest_failure.failed_at_ms,
                    snapshot.artifact_completeness, snapshot.replica_durability,
                    snapshot.protection_compliance, snapshot.restore_eligibility,
                    snapshot.highest_verified_level, snapshot.last_verified_at_ms,
                    snapshot.projected_at_ms,
                    COALESCE(scope_plan.rpo_ms, NULL),
                    scope_plan.verification_requested_level,
                    COALESCE(scope_plan.plan_count, 0),
                    COALESCE(scope_plan.active_plan_count, 0),
                    COALESCE(scope_plan.retention_policy_count, 0),
                    COALESCE(scope_plan.verification_schedule_unknown, 0),
                    CASE
                        WHEN latest_success.recovery_point_id IS NULL THEN 0
                        ELSE NOT EXISTS (
                            SELECT 1
                            FROM scope_policy AS policy
                            WHERE policy.backup_scope_id = scope.id
                              AND policy.plan_state = 'enabled'
                              AND policy.published_at_ms IS NOT NULL
                              AND policy.verification_requested_level IS NOT NULL
                              AND (
                                  policy.verification_schedule IS NULL
                                  OR policy.verification_schedule NOT IN ('every_run', 'weekly')
                                  OR
                                  (
                                      policy.verification_schedule = 'every_run'
                                      AND NOT EXISTS (
                                          SELECT 1
                                          FROM catalog_verification_projection AS evidence
                                          WHERE evidence.recovery_point_id = latest_success.recovery_point_id
                                            AND evidence.result = 'passed'
                                            AND evidence.verify_level >= policy.verification_requested_level
                                      )
                                  )
                                  OR
                                  (
                                      policy.verification_schedule = 'weekly'
                                      AND NOT EXISTS (
                                          SELECT 1
                                          FROM backup_run_scopes AS historical_scope
                                          JOIN backup_runs AS historical_run
                                            ON historical_run.id = historical_scope.backup_run_id
                                          JOIN catalog_verification_projection AS evidence
                                            ON evidence.recovery_point_id = historical_run.recovery_point_id
                                           AND evidence.result = 'passed'
                                           AND evidence.verify_level >= policy.verification_requested_level
                                          WHERE historical_scope.backup_scope_id = scope.id
                                            AND historical_run.state IN ('succeeded', 'partially_succeeded')
                                            AND evidence.verified_at_ms <= ?3
                                            AND ?3 - evidence.verified_at_ms <= 604800000
                                      )
                                  )
                              )
                        )
                    END AS verification_satisfied,
                    CASE
                        WHEN latest_success.recovery_point_id IS NULL THEN 0
                        ELSE EXISTS (
                            SELECT 1
                            FROM scope_policy AS policy
                            WHERE policy.backup_scope_id = scope.id
                              AND policy.plan_state = 'enabled'
                              AND policy.published_at_ms IS NOT NULL
                              AND policy.verification_requested_level IS NOT NULL
                              AND (
                                  (
                                      policy.verification_schedule = 'every_run'
                                      AND NOT EXISTS (
                                          SELECT 1
                                          FROM catalog_verification_projection AS evidence
                                          WHERE evidence.recovery_point_id = latest_success.recovery_point_id
                                            AND evidence.result = 'passed'
                                            AND evidence.verify_level >= policy.verification_requested_level
                                      )
                                  )
                                  OR
                                  (
                                      policy.verification_schedule = 'weekly'
                                      AND ?3 >= policy.published_at_ms
                                      AND ?3 - policy.published_at_ms >= 604800000
                                      AND NOT EXISTS (
                                          SELECT 1
                                          FROM backup_run_scopes AS historical_scope
                                          JOIN backup_runs AS historical_run
                                            ON historical_run.id = historical_scope.backup_run_id
                                          JOIN catalog_verification_projection AS evidence
                                            ON evidence.recovery_point_id = historical_run.recovery_point_id
                                           AND evidence.result = 'passed'
                                           AND evidence.verify_level >= policy.verification_requested_level
                                          WHERE historical_scope.backup_scope_id = scope.id
                                            AND historical_run.state IN ('succeeded', 'partially_succeeded')
                                            AND evidence.verified_at_ms <= ?3
                                            AND ?3 - evidence.verified_at_ms <= 604800000
                                      )
                                  )
                              )
                        )
                    END AS verification_overdue,
                    scope.created_at_ms
             FROM backup_scopes AS scope
             JOIN database_targets AS target ON target.id = scope.database_target_id
             JOIN connection_profiles AS connection ON connection.id = target.connection_profile_id
             JOIN workspace_connection_profiles AS ownership
               ON ownership.connection_profile_id = connection.id
             LEFT JOIN scope_discovery AS discovery
               ON discovery.backup_scope_id = scope.id
             LEFT JOIN scope_baseline AS baseline
               ON baseline.backup_scope_id = scope.id
             LEFT JOIN scope_plan ON scope_plan.backup_scope_id = scope.id
             LEFT JOIN latest_run ON latest_run.backup_scope_id = scope.id
             LEFT JOIN latest_success ON latest_success.backup_scope_id = scope.id
             LEFT JOIN latest_failure ON latest_failure.backup_scope_id = scope.id
             LEFT JOIN catalog_snapshot_projection AS snapshot
               ON snapshot.recovery_point_id = latest_success.recovery_point_id
              AND snapshot.source_target_identity = scope.source_fingerprint
             WHERE ownership.workspace_id = ?1
               AND (?2 IS NULL OR target.id = ?2)
               AND (?4 IS NULL OR connection.id IN (SELECT value FROM json_each(?4)))
               AND (?5 IS NULL OR target.id IN (SELECT value FROM json_each(?5)))
               AND (?6 IS NULL OR scope.state = ?6)
               AND (?7 IS NULL OR lower(
                    scope.display_name || ' ' || target.display_name || ' ' ||
                    connection.display_name || ' ' || connection.engine
               ) LIKE ?7 ESCAPE '\\')
               AND (
                    ?8 IS NULL OR scope.created_at_ms > ?8
                    OR (scope.created_at_ms = ?8 AND scope.id > ?9)
               )
             ORDER BY scope.created_at_ms, scope.id
             LIMIT ?10",
        )?;
        let (cursor_created_at_ms, cursor_id) = cursor
            .map(|(created_at_ms, id)| (Some(*created_at_ms), Some(id.as_str())))
            .unwrap_or((None, None));
        let rows = statement.query_map(
            params![
                workspace_id,
                database_target_id,
                now_ms,
                connection_ids_json,
                target_ids_json,
                state,
                search_pattern,
                cursor_created_at_ms,
                cursor_id,
                fetch_limit,
            ],
            |row| {
                let mode: String = row.get(7)?;
                let selected_object_count = row.get(12)?;
                let eligible_object_count = row.get(13)?;
                let new_object_count: Option<i64> = row.get(14)?;
                let missing_object_count: Option<i64> = row.get(15)?;
                let changed_object_count: Option<i64> = row.get(16)?;
                let source_drift = row.get::<_, i64>(17)? != 0;
                let inventory_observed_at_ms: Option<i64> = row.get(20)?;
                let latest_recovery_point_id: Option<String> = row.get(22)?;
                let latest_success_at_ms: Option<i64> = row.get(23)?;
                let recovery_evidence_at_ms: Option<i64> = row.get(31)?;
                let rpo_ms: Option<i64> = row.get(32)?;
                let verification_requested_level: Option<i64> = row.get(33)?;
                let plan_count: i64 = row.get(34)?;
                let active_plan_count: i64 = row.get(35)?;
                let verification_schedule_unknown = row.get::<_, i64>(37)? != 0;
                let verification_satisfied = row.get::<_, i64>(38)? != 0;

                let drift_state = if source_drift
                    || [new_object_count, missing_object_count, changed_object_count]
                        .into_iter()
                        .flatten()
                        .any(|count| count > 0)
                {
                    "drifted"
                } else if inventory_observed_at_ms.is_some()
                    && [new_object_count, missing_object_count, changed_object_count]
                        .into_iter()
                        .all(|count| count.is_some())
                {
                    "current"
                } else {
                    "unknown"
                };
                let rpo_state = match (active_plan_count, rpo_ms, latest_success_at_ms) {
                    (0, _, _) | (_, None, _) => "not_configured",
                    (_, Some(rpo), Some(success_at)) if now_ms >= success_at => {
                        if now_ms - success_at > rpo {
                            "violated"
                        } else {
                            "current"
                        }
                    }
                    _ => "unknown",
                };
                let latest_verified_level: Option<i64> = row.get(29)?;
                let verification_state = match (
                    active_plan_count,
                    verification_requested_level,
                    latest_recovery_point_id.as_ref(),
                    verification_satisfied,
                ) {
                    (0, _, _, _) | (_, None, _, _) => "not_configured",
                    (_, _, None, _) => "unknown",
                    (_, Some(_), Some(_), true) => "current",
                    (_, Some(_), Some(_), false) if verification_schedule_unknown => "unknown",
                    _ => "pending",
                };
                let evidence_at_ms = [inventory_observed_at_ms, recovery_evidence_at_ms]
                    .into_iter()
                    .flatten()
                    .max();
                let protection_compliance = if active_plan_count == 0 {
                    "not_configured".to_owned()
                } else {
                    row.get::<_, Option<String>>(27)?
                        .unwrap_or_else(|| "unknown".to_owned())
                };

                Ok((
                    ConsoleBackupScopeHealth {
                        id: row.get(0)?,
                        database_target_id: row.get(1)?,
                        database_name: row.get(2)?,
                        connection_id: row.get(3)?,
                        connection_name: row.get(4)?,
                        engine: row.get(5)?,
                        display_name: row.get(6)?,
                        mode,
                        state: row.get(8)?,
                        selector_hash_blake3: row.get(9)?,
                        source_fingerprint: row.get(10)?,
                        database_source_fingerprint: row.get(11)?,
                        selected_object_count,
                        eligible_object_count,
                        new_object_count,
                        missing_object_count,
                        changed_object_count,
                        drift_state: drift_state.to_owned(),
                        source_drift,
                        discovery_run_id: row.get(18)?,
                        inventory_hash_blake3: row.get(19)?,
                        inventory_observed_at_ms,
                        latest_backup_state: row.get(21)?,
                        latest_recovery_point_id,
                        latest_success_at_ms,
                        latest_failure_at_ms: row.get(24)?,
                        artifact_completeness: row
                            .get::<_, Option<String>>(25)?
                            .unwrap_or_else(|| "unknown".to_owned()),
                        replica_durability: row
                            .get::<_, Option<String>>(26)?
                            .unwrap_or_else(|| "unknown".to_owned()),
                        protection_compliance,
                        restore_eligibility: row
                            .get::<_, Option<String>>(28)?
                            .unwrap_or_else(|| "unknown".to_owned()),
                        latest_verified_level,
                        last_verified_at_ms: row.get(30)?,
                        recovery_evidence_at_ms,
                        rpo_ms,
                        rpo_state: rpo_state.to_owned(),
                        rpo_violation: rpo_state == "violated",
                        verification_requested_level,
                        verification_state: verification_state.to_owned(),
                        verification_overdue: row.get::<_, i64>(39)? != 0,
                        plan_count,
                        active_plan_count,
                        retention_policy_count: row.get(36)?,
                        evidence_at_ms,
                    },
                    row.get(40)?,
                ))
            },
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::from)
    }

    pub fn scheduler_health(&self) -> Result<ConsoleSchedulerHealth, RepositoryError> {
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let (
            configured_plan_count,
            observed_cursor_count,
            active_work_count,
            error_plan_count,
            last_tick_at_ms,
            last_materialized_for_ms,
            evidence_at_ms,
        ): (i64, i64, i64, i64, Option<i64>, Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT COUNT(DISTINCT revision.id),
                        COUNT(DISTINCT cursor.plan_revision_id),
                        COUNT(DISTINCT CASE
                            WHEN work.state IN ('queued', 'delayed', 'leased', 'starting', 'running',
                                                'committing', 'cancel_requested')
                            THEN work.id
                        END),
                        COUNT(DISTINCT CASE WHEN cursor.last_error_code IS NOT NULL
                                           THEN cursor.plan_revision_id END),
                        MAX(cursor.last_evaluated_at_ms),
                        MAX(cursor.last_materialized_for_ms),
                        MAX(cursor.updated_at_ms)
                 FROM backup_plans AS plan
                 JOIN backup_plan_revisions AS revision ON revision.id = plan.current_revision_id
                 LEFT JOIN scheduler_cursors AS cursor ON cursor.plan_revision_id = revision.id
                 LEFT JOIN backup_runs AS run ON run.plan_revision_id = revision.id
                 LEFT JOIN work_items AS work ON work.backup_run_id = run.id
                 WHERE plan.state = 'enabled'
                   AND plan.deleted_at_ms IS NULL
                   AND revision.published_at_ms IS NOT NULL
                   AND revision.schedule_kind = 'cron'
                   AND revision.schedule_expression IS NOT NULL",
                [],
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
            )?;
        let last_error: Option<(String, i64)> = connection
            .query_row(
                "SELECT cursor.last_error_code, cursor.updated_at_ms
                 FROM scheduler_cursors AS cursor
                 JOIN backup_plan_revisions AS revision ON revision.id = cursor.plan_revision_id
                 JOIN backup_plans AS plan ON plan.current_revision_id = revision.id
                 WHERE plan.state = 'enabled'
                   AND plan.deleted_at_ms IS NULL
                   AND cursor.last_error_code IS NOT NULL
                 ORDER BY cursor.updated_at_ms DESC, cursor.plan_revision_id DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let state = if configured_plan_count == 0 {
            "not_configured"
        } else if observed_cursor_count == 0 {
            "awaiting_first_tick"
        } else if error_plan_count > 0 {
            "attention"
        } else if observed_cursor_count < configured_plan_count {
            "partial"
        } else {
            "observed"
        };
        Ok(ConsoleSchedulerHealth {
            state: state.to_owned(),
            configured_plan_count,
            observed_cursor_count,
            active_work_count,
            error_plan_count,
            last_tick_at_ms,
            last_materialized_for_ms,
            last_error_code: last_error.as_ref().map(|value| value.0.clone()),
            last_error_at_ms: last_error.map(|value| value.1),
            evidence_at_ms,
        })
    }

    pub fn retention_health(&self) -> Result<ConsoleRetentionHealth, RepositoryError> {
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let configured_policy_count = connection.query_row(
            "SELECT COUNT(DISTINCT plan.id)
             FROM backup_plans AS plan
             JOIN backup_plan_revisions AS revision ON revision.id = plan.current_revision_id
             WHERE plan.state = 'enabled'
               AND plan.deleted_at_ms IS NULL
               AND revision.published_at_ms IS NOT NULL
               AND COALESCE(CAST(json_extract(revision.retention_policy_json, '$.daily') AS INTEGER), 0)
                 + COALESCE(CAST(json_extract(revision.retention_policy_json, '$.weekly') AS INTEGER), 0)
                 + COALESCE(CAST(json_extract(revision.retention_policy_json, '$.monthly') AS INTEGER), 0)
                 + COALESCE(CAST(json_extract(revision.retention_policy_json, '$.keepLast') AS INTEGER), 0) > 0",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let last_scan_at_ms: Option<i64> = connection.query_row(
            "SELECT MAX(updated_at_ms)
             FROM app_meta
             WHERE key IN ('retention.runtime.last_scan.v1', 'retention.runtime.cursor.v1')",
            [],
            |row| row.get(0),
        )?;
        let active_tombstone_count = connection.query_row(
            "SELECT COUNT(*) FROM operation_runs
             WHERE operation_kind = 'retention_delete'
               AND state = 'created'
               AND json_extract(request_json, '$.phase') = 'tombstoned'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let queued_delete_count = connection.query_row(
            "SELECT COUNT(DISTINCT operation.id)
             FROM operation_runs AS operation
             JOIN work_items AS work ON work.operation_run_id = operation.id
             WHERE operation.operation_kind = 'retention_delete'
               AND work.state IN ('queued', 'delayed', 'leased', 'starting', 'running',
                                  'committing', 'cancel_requested')",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let failed_operation_count = connection.query_row(
            "SELECT COUNT(*) FROM operation_runs
             WHERE operation_kind = 'retention_delete' AND state = 'failed'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let last_error: Option<(Option<String>, i64)> = connection
            .query_row(
                "SELECT error_code, updated_at_ms
                 FROM operation_runs
                 WHERE operation_kind = 'retention_delete' AND state = 'failed'
                 ORDER BY updated_at_ms DESC, id DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let state = if failed_operation_count > 0 {
            "attention"
        } else if configured_policy_count == 0 {
            "not_configured"
        } else if last_scan_at_ms.is_none() {
            "awaiting_first_scan"
        } else {
            "observed"
        };
        let last_error_at_ms = last_error.as_ref().map(|value| value.1);
        let evidence_at_ms = [last_scan_at_ms, last_error_at_ms]
            .into_iter()
            .flatten()
            .max();
        Ok(ConsoleRetentionHealth {
            state: state.to_owned(),
            configured_policy_count,
            active_tombstone_count,
            queued_delete_count,
            failed_operation_count,
            last_scan_at_ms,
            last_error_code: last_error.and_then(|value| value.0),
            last_error_at_ms,
            evidence_at_ms,
        })
    }

    pub fn snapshots(
        &self,
        query: ConsoleSnapshotQuery,
    ) -> Result<CursorPage<ConsoleSnapshot>, RepositoryError> {
        self.snapshots_in_workspace(DEFAULT_WORKSPACE_ID, query)
    }

    pub fn snapshots_in_workspace(
        &self,
        workspace_id: &str,
        query: ConsoleSnapshotQuery,
    ) -> Result<CursorPage<ConsoleSnapshot>, RepositoryError> {
        validate_snapshot_query(&query)?;
        if !valid_workspace_id(workspace_id) {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let cursor = query
            .cursor
            .as_deref()
            .map(parse_snapshot_cursor)
            .transpose()?;
        let (cursor_time, cursor_id) = cursor
            .map(|(time, id)| (Some(time), Some(id)))
            .unwrap_or((None, None));
        let search = query
            .search
            .as_deref()
            .map(|value| format!("%{}%", value.trim().to_ascii_lowercase()));
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let count_sql = snapshot_select_sql(true);
        let total_count = connection.query_row(
            &count_sql,
            params![
                workspace_id,
                query.scope,
                search,
                query.connection_id,
                query.engine,
                query.from_ms,
                query.to_ms,
                query.artifact_completeness,
                query.replica_durability,
                query.protection_compliance,
                query.min_verification_level,
                query.restore_eligibility,
                query.signature_status,
            ],
            |row| row.get(0),
        )?;

        let select_sql = snapshot_select_sql(false);
        let mut statement = connection.prepare(&select_sql)?;
        let mut items = statement
            .query_map(
                params![
                    workspace_id,
                    query.scope,
                    search,
                    query.connection_id,
                    query.engine,
                    query.from_ms,
                    query.to_ms,
                    query.artifact_completeness,
                    query.replica_durability,
                    query.protection_compliance,
                    query.min_verification_level,
                    query.restore_eligibility,
                    query.signature_status,
                    cursor_time,
                    cursor_id,
                    page_fetch_limit(query.limit)?,
                ],
                |row| {
                    let state: String = row.get(4)?;
                    let signature_status: String = row.get(9)?;
                    Ok(ConsoleSnapshot {
                        id: row.get(0)?,
                        source_target_identity: row.get(1)?,
                        source_display_name: row.get(2)?,
                        source_connection_id: row.get(22)?,
                        source_connection_name: row.get(23)?,
                        engine: row.get(3)?,
                        state: state.clone(),
                        committed_at_ms: row.get(5)?,
                        manifest_schema_version: row.get(6)?,
                        set_manifest_checksum_blake3: row.get(7)?,
                        signing_key_id: row.get(8)?,
                        signature_status: signature_status.clone(),
                        required_part_count: row.get(10)?,
                        present_part_count: row.get(11)?,
                        plaintext_bytes: row.get(12)?,
                        cipher_bytes: row.get(13)?,
                        artifact_completeness: row.get(16)?,
                        replica_durability: row.get(17)?,
                        protection_compliance: row.get(18)?,
                        restore_eligibility: row.get(19)?,
                        highest_verified_level: row.get(20)?,
                        last_verified_at_ms: row.get(21)?,
                        first_ingested_at_ms: row.get(14)?,
                        last_ingested_at_ms: row.get(15)?,
                        diagnostic_reason: snapshot_diagnostic_reason(&state, &signature_status),
                        part_count: row.get(24)?,
                        replica_count: row.get(25)?,
                        verification_evidence_count: row.get(26)?,
                        restore_drill_count: row.get(27)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = trim_serialized_page(&mut items, query.limit, |item| {
            format!("{}:{}", item.committed_at_ms.unwrap_or(-1), item.id)
        })?;
        Ok(CursorPage {
            items,
            next_cursor,
            total_count,
        })
    }

    pub fn restore_target_facts(
        &self,
        connection_id: &str,
        target_name: &str,
    ) -> Result<ConsoleRestoreTargetFacts, RepositoryError> {
        if !valid_filter_text(connection_id)
            || !valid_filter_text(target_name)
            || target_name.len() > 512
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        type ConnectionFactsRow = (
            String,
            String,
            String,
            String,
            bool,
            Option<String>,
            Option<String>,
            Option<i64>,
            String,
            Option<String>,
        );
        let row: ConnectionFactsRow = connection
            .query_row(
                "SELECT profile.id, profile.display_name, profile.engine, profile.state,
                        EXISTS(
                            SELECT 1
                            FROM connection_secret_bindings AS binding
                            JOIN secret_refs AS secret
                              ON secret.id = binding.secret_ref_id
                            WHERE binding.connection_profile_id = profile.id
                              AND binding.secret_role = 'database_auth'
                              AND secret.state = 'active'
                        ),
                        probe.status, probe.server_version, probe.completed_at_ms,
                        profile.adapter_config_json, probe.observed_capabilities_json
                 FROM connection_profiles AS profile
                 LEFT JOIN connection_probe_runs AS probe
                   ON probe.id = (
                        SELECT candidate.id
                        FROM connection_probe_runs AS candidate
                        WHERE candidate.connection_profile_id = profile.id
                        ORDER BY candidate.started_at_ms DESC, candidate.id DESC
                        LIMIT 1
                   )
                 WHERE profile.id = ?1
                   AND profile.deleted_at_ms IS NULL",
                [connection_id],
                |row| {
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
                        row.get(9)?,
                    ))
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)?;
        let latest_discovery_id: Option<String> = connection
            .query_row(
                "SELECT id
                 FROM discovery_runs
                 WHERE connection_profile_id = ?1
                   AND status = 'succeeded'
                 ORDER BY completed_at_ms DESC, id DESC
                 LIMIT 1",
                [connection_id],
                |row| row.get(0),
            )
            .optional()?;
        let target_exists = latest_discovery_id
            .map(|discovery_id| {
                connection.query_row(
                    "SELECT EXISTS(
                         SELECT 1
                         FROM discovered_databases
                         WHERE discovery_run_id = ?1
                           AND normalized_name = LOWER(?2)
                     )",
                    params![discovery_id, target_name],
                    |row| row.get::<_, bool>(0),
                )
            })
            .transpose()?;
        let spool: (Option<i64>, Option<i64>) = connection.query_row(
            "SELECT
                 MAX(
                     CASE
                         WHEN last_free_bytes > reserve_floor_bytes
                         THEN last_free_bytes - reserve_floor_bytes
                         ELSE 0
                     END
                 ),
                 MAX(last_probed_at_ms)
             FROM spool_volumes
             WHERE state IN ('ready', 'throttled')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let server_identity = row
            .9
            .as_deref()
            .and_then(|json| server_identity_from_capabilities(&row.2, json));
        Ok(ConsoleRestoreTargetFacts {
            connection_id: row.0,
            connection_name: row.1,
            engine: row.2,
            connection_state: row.3,
            credential_bound: row.4,
            probe_status: row.5,
            server_version: row.6,
            server_identity,
            probed_at_ms: row.7,
            restore_test_target: restore_test_target_from_config(&row.8),
            target_exists,
            available_spool_bytes: spool.0,
            spool_observed_at_ms: spool.1,
        })
    }

    pub fn activity(
        &self,
        cursor: Option<&str>,
        limit: usize,
        severity: Option<&str>,
    ) -> Result<CursorPage<ConsoleActivityEvent>, RepositoryError> {
        self.activity_in_workspace(DEFAULT_WORKSPACE_ID, cursor, limit, severity)
    }

    pub fn activity_in_workspace(
        &self,
        workspace_id: &str,
        cursor: Option<&str>,
        limit: usize,
        severity: Option<&str>,
    ) -> Result<CursorPage<ConsoleActivityEvent>, RepositoryError> {
        if limit == 0
            || limit > MAX_ACTIVITY_PAGE
            || cursor.is_some_and(|value| value.len() > MAX_CURSOR_BYTES)
            || !valid_workspace_id(workspace_id)
            || severity.is_some_and(|value| {
                !matches!(value, "debug" | "info" | "warning" | "error" | "critical")
            })
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let cursor = cursor.map(parse_activity_cursor).transpose()?;
        let (cursor_time, cursor_seq) = cursor.unzip();
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let total_count = connection.query_row(
            "SELECT COUNT(*)
             FROM activity_events AS event
             JOIN workspace_activity_events AS ownership
               ON ownership.activity_event_seq = event.seq
             WHERE ownership.workspace_id = ?1
               AND (?2 IS NULL OR event.severity = ?2)",
            params![workspace_id, severity],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT seq, id, occurred_at_ms, severity, event_code, phase,
                    entity_type, entity_id, correlation_id, work_item_id,
                    attempt_id, retryability, message
             FROM activity_events AS event
             JOIN workspace_activity_events AS ownership
               ON ownership.activity_event_seq = event.seq
             WHERE ownership.workspace_id = ?1
               AND (?2 IS NULL OR event.severity = ?2)
               AND (
                    ?3 IS NULL
                    OR event.occurred_at_ms < ?3
                    OR (event.occurred_at_ms = ?3 AND event.seq < ?4)
               )
             ORDER BY event.occurred_at_ms DESC, event.seq DESC
             LIMIT ?5",
        )?;
        let mut items = statement
            .query_map(
                params![
                    workspace_id,
                    severity,
                    cursor_time,
                    cursor_seq,
                    page_fetch_limit(limit)?
                ],
                |row| {
                    Ok(ConsoleActivityEvent {
                        seq: row.get(0)?,
                        id: row.get(1)?,
                        occurred_at_ms: row.get(2)?,
                        severity: row.get(3)?,
                        event_code: row.get(4)?,
                        phase: row.get(5)?,
                        entity_type: row.get(6)?,
                        entity_id: row.get(7)?,
                        correlation_id: row.get(8)?,
                        work_item_id: row.get(9)?,
                        attempt_id: row.get(10)?,
                        retryability: row.get(11)?,
                        message: row.get(12)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let next_cursor = trim_activity_page(&mut items, limit)?;
        Ok(CursorPage {
            items,
            next_cursor,
            total_count,
        })
    }

    pub fn engine_matrix(&self, now_ms: i64) -> Result<ConsoleEngineMatrix, RepositoryError> {
        if now_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let certification_total_count =
            connection.query_row("SELECT COUNT(*) FROM engine_certifications", [], |row| {
                row.get(0)
            })?;
        let mut certification_statement = connection.prepare(
            "SELECT id, engine, server_version_range, tool_name, tool_version_range,
                    operating_system, architecture, dump_format, consistency_grade,
                    status, evidence_locator, evidence_checksum_blake3,
                    certified_at_ms, expires_at_ms
             FROM engine_certifications
             ORDER BY engine, status = 'certified' DESC, certified_at_ms DESC, id
             LIMIT ?1",
        )?;
        let certifications = certification_statement
            .query_map([MAX_ENGINE_CERTIFICATION_ROWS as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                    row.get::<_, Option<i64>>(13)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut rows = Vec::with_capacity(certifications.len() + 9);
        let mut represented = BTreeSet::new();
        for certification in certifications {
            represented.insert(certification.1.clone());
            let mut capability_statement = connection.prepare(
                "SELECT capability, support_status, limitation_text, evidence_locator
                 FROM engine_certification_capabilities
                 WHERE certification_id = ?1
                 ORDER BY capability",
            )?;
            let observed = capability_statement
                .query_map([certification.0.as_str()], |row| {
                    Ok(ConsoleEngineCapability {
                        capability: row.get(0)?,
                        support_status: row.get(1)?,
                        limitation: row.get(2)?,
                        evidence_locator: row.get(3)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let observed_by_name = observed
                .iter()
                .map(|item| (item.capability.as_str(), item))
                .collect::<BTreeMap<_, _>>();
            let capabilities = ProviderCapability::REQUIRED
                .into_iter()
                .map(|capability| {
                    observed_by_name.get(capability.as_str()).map_or_else(
                        || ConsoleEngineCapability {
                            capability: capability.as_str().to_owned(),
                            support_status: "not_run".to_owned(),
                            limitation: Some(
                                "No current-build certification evidence is recorded.".to_owned(),
                            ),
                            evidence_locator: None,
                        },
                        |item| (*item).clone(),
                    )
                })
                .collect::<Vec<_>>();
            let support_status = matrix_support_status(
                &certification.9,
                certification.12,
                certification.13,
                now_ms,
                &observed,
            );
            rows.push(ConsoleEngineMatrixRow {
                certification_id: Some(certification.0),
                engine: certification.1,
                server_version_range: Some(certification.2),
                tool_name: Some(certification.3),
                tool_version_range: Some(certification.4),
                operating_system: Some(certification.5),
                architecture: Some(certification.6),
                dump_format: Some(certification.7),
                consistency_grade: Some(certification.8),
                support_status,
                evidence_locator: Some(certification.10),
                evidence_checksum_blake3: Some(certification.11),
                certified_at_ms: certification.12,
                expires_at_ms: certification.13,
                capabilities,
            });
        }
        for engine in [
            "postgresql",
            "mysql",
            "mariadb",
            "sqlserver",
            "sqlite",
            "mongodb",
            "firebird",
            "redis",
            "valkey",
        ] {
            if represented.contains(engine) {
                continue;
            }
            rows.push(ConsoleEngineMatrixRow {
                certification_id: None,
                engine: engine.to_owned(),
                server_version_range: None,
                tool_name: None,
                tool_version_range: None,
                operating_system: None,
                architecture: None,
                dump_format: None,
                consistency_grade: None,
                support_status: "unavailable".to_owned(),
                evidence_locator: None,
                evidence_checksum_blake3: None,
                certified_at_ms: None,
                expires_at_ms: None,
                capabilities: ProviderCapability::REQUIRED
                    .into_iter()
                    .map(|capability| ConsoleEngineCapability {
                        capability: capability.as_str().to_owned(),
                        support_status: "not_run".to_owned(),
                        limitation: Some(
                            "No certification row exists for this engine and build.".to_owned(),
                        ),
                        evidence_locator: None,
                    })
                    .collect(),
            });
        }
        rows.sort_by(|left, right| {
            left.engine
                .cmp(&right.engine)
                .then_with(|| right.certified_at_ms.cmp(&left.certified_at_ms))
                .then_with(|| left.certification_id.cmp(&right.certification_id))
        });
        let response_budget_complete = truncate_global_rows(&mut rows)?;
        Ok(ConsoleEngineMatrix {
            generated_at_ms: now_ms,
            certification_total_count,
            complete: certification_total_count <= MAX_ENGINE_CERTIFICATION_ROWS as i64
                && response_budget_complete,
            rows,
        })
    }

    pub fn settings(&self) -> Result<ConsoleSettings, RepositoryError> {
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let inventory_counts = connection.query_row(
            "SELECT
                 (SELECT COUNT(*) FROM resource_pools),
                 (SELECT COUNT(*) FROM spool_volumes),
                 (SELECT COUNT(*) FROM native_tool_inventory WHERE deleted_at_ms IS NULL),
                 (SELECT COUNT(*) FROM notification_channels WHERE deleted_at_ms IS NULL)",
            [],
            |row| {
                Ok(ConsoleSettingsInventoryCounts {
                    resource_pools: row.get(0)?,
                    spool_volumes: row.get(1)?,
                    tools: row.get(2)?,
                    notification_channels: row.get(3)?,
                })
            },
        )?;

        let mut resource_statement = connection.prepare(
            "SELECT pools.id, pools.resource_type, pools.resource_key, pools.capacity,
                    COALESCE((
                        SELECT SUM(allocations.units)
                        FROM resource_allocations AS allocations
                        WHERE allocations.resource_pool_id = pools.id
                          AND allocations.released_at_ms IS NULL
                    ), 0),
                    pools.enabled
             FROM resource_pools AS pools
             ORDER BY pools.resource_type, pools.resource_key, pools.id
             LIMIT ?1",
        )?;
        let mut resource_pools = resource_statement
            .query_map([MAX_GLOBAL_INVENTORY_ROWS as i64], |row| {
                Ok(ConsoleResourcePool {
                    id: row.get(0)?,
                    resource_type: row.get(1)?,
                    resource_key: row.get(2)?,
                    capacity: row.get(3)?,
                    in_use: row.get(4)?,
                    enabled: row.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut spool_statement = connection.prepare(
            "SELECT id, root_path, state, hard_limit_bytes, soft_limit_bytes,
                    last_free_bytes, last_probed_at_ms
             FROM spool_volumes
             ORDER BY root_path, id
             LIMIT ?1",
        )?;
        let mut spool_volumes = spool_statement
            .query_map([MAX_GLOBAL_INVENTORY_ROWS as i64], |row| {
                Ok(ConsoleSpoolVolume {
                    id: row.get(0)?,
                    root_path: row.get(1)?,
                    state: row.get(2)?,
                    hard_limit_bytes: row.get(3)?,
                    soft_limit_bytes: row.get(4)?,
                    free_bytes: row.get(5)?,
                    probed_at_ms: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut tool_statement = connection.prepare(
            "SELECT id, tool_name, version_text, architecture, trust_status,
                    signature_status, last_verified_at_ms
             FROM native_tool_inventory
             WHERE deleted_at_ms IS NULL
             ORDER BY tool_name, version_text, id
             LIMIT ?1",
        )?;
        let mut tools = tool_statement
            .query_map([MAX_GLOBAL_INVENTORY_ROWS as i64], |row| {
                Ok(ConsoleToolInventoryItem {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    version: row.get(2)?,
                    architecture: row.get(3)?,
                    trust_status: row.get(4)?,
                    signature_status: row.get(5)?,
                    verified_at_ms: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut notification_statement = connection.prepare(
            "SELECT id, display_name, kind, state, last_probe_status, last_probe_at_ms
             FROM notification_channels
             WHERE deleted_at_ms IS NULL
             ORDER BY display_name, id
             LIMIT ?1",
        )?;
        let mut notification_channels = notification_statement
            .query_map([MAX_GLOBAL_INVENTORY_ROWS as i64], |row| {
                Ok(ConsoleNotificationChannel {
                    id: row.get(0)?,
                    display_name: row.get(1)?,
                    kind: row.get(2)?,
                    state: row.get(3)?,
                    last_test_status: row.get(4)?,
                    last_test_at_ms: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let control_backup = connection
            .query_row(
                "SELECT completed_at_ms,
                        CASE status
                            WHEN 'succeeded' THEN 'succeeded'
                            WHEN 'failed' THEN 'failed'
                            ELSE 'unknown'
                        END
                 FROM control_store_backups
                 WHERE completed_at_ms IS NOT NULL
                 ORDER BY completed_at_ms DESC, id DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok(ConsoleControlBackup {
                        last_completed_at_ms: row.get(0)?,
                        last_status: row.get(1)?,
                    })
                },
            )
            .optional()?
            .unwrap_or(ConsoleControlBackup {
                last_completed_at_ms: None,
                last_status: "unknown".to_owned(),
            });

        let row_cap_complete = inventory_counts.resource_pools == resource_pools.len() as i64
            && inventory_counts.spool_volumes == spool_volumes.len() as i64
            && inventory_counts.tools == tools.len() as i64
            && inventory_counts.notification_channels == notification_channels.len() as i64;
        let response_budget_complete = truncate_settings_inventory(
            &mut resource_pools,
            &mut spool_volumes,
            &mut tools,
            &mut notification_channels,
        )?;
        Ok(ConsoleSettings {
            resource_pools,
            spool_volumes,
            tools,
            notification_channels,
            control_backup,
            inventory_counts,
            complete: row_cap_complete && response_budget_complete,
        })
    }

    pub fn work_item(&self, id: &str) -> Result<ConsoleWorkItem, RepositoryError> {
        self.work_item_in_workspace(DEFAULT_WORKSPACE_ID, id)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn discovery_results_in_workspace(
        &self,
        workspace_id: &str,
        discovery_run_id: &str,
        cursor: Option<&str>,
        limit: usize,
        search: Option<&str>,
        eligibility: Option<&str>,
    ) -> Result<CursorPage<ConsoleDiscoveredDatabase>, RepositoryError> {
        validate_discovery_page_input(
            workspace_id,
            discovery_run_id,
            cursor,
            limit,
            search,
            eligibility,
        )?;
        let cursor = cursor.map(parse_created_cursor).transpose()?;
        let search_pattern = search.map(scope_search_pattern).transpose()?;
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let belongs_to_workspace = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM discovery_runs AS discovery
                 JOIN workspace_connection_profiles AS ownership
                   ON ownership.connection_profile_id = discovery.connection_profile_id
                 WHERE discovery.id = ?1 AND ownership.workspace_id = ?2
             )",
            params![discovery_run_id, workspace_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !belongs_to_workspace {
            return Err(RepositoryError::NotFound);
        }
        let total_count = connection.query_row(
            "SELECT COUNT(*)
             FROM discovered_databases AS observed
             WHERE observed.discovery_run_id = ?1
               AND (?2 IS NULL OR observed.eligibility = ?2)
               AND (?3 IS NULL OR lower(
                    observed.database_name || ' ' || observed.engine_object_id
               ) LIKE ?3 ESCAPE '\\')",
            params![discovery_run_id, eligibility, search_pattern],
            |row| row.get(0),
        )?;
        let mut statement = connection.prepare(
            "SELECT observed.engine_object_id, observed.database_name,
                    observed.eligibility, observed.exclusion_reason,
                    observed.created_at_ms
             FROM discovered_databases AS observed
             WHERE observed.discovery_run_id = ?1
               AND (?2 IS NULL OR observed.eligibility = ?2)
               AND (?3 IS NULL OR lower(
                    observed.database_name || ' ' || observed.engine_object_id
               ) LIKE ?3 ESCAPE '\\')
               AND (
                    ?4 IS NULL OR observed.created_at_ms > ?4
                    OR (observed.created_at_ms = ?4 AND observed.engine_object_id > ?5)
               )
             ORDER BY observed.created_at_ms, observed.engine_object_id
             LIMIT ?6",
        )?;
        let (cursor_created_at_ms, cursor_id) = cursor
            .as_ref()
            .map(|(created_at_ms, id)| (Some(*created_at_ms), Some(id.as_str())))
            .unwrap_or((None, None));
        let mut rows = statement
            .query_map(
                params![
                    discovery_run_id,
                    eligibility,
                    search_pattern,
                    cursor_created_at_ms,
                    cursor_id,
                    page_fetch_limit(limit)?,
                ],
                |row| {
                    Ok((
                        ConsoleDiscoveredDatabase {
                            engine_object_id: row.get(0)?,
                            database_name: row.get(1)?,
                            eligibility: row.get(2)?,
                            exclusion_reason: row.get(3)?,
                        },
                        row.get::<_, i64>(4)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = rows.len() > limit;
        if has_more {
            rows.truncate(limit);
        }
        let next_cursor = has_more.then(|| {
            let (item, created_at_ms) = rows.last().expect("non-empty page with lookahead");
            created_cursor(*created_at_ms, &item.engine_object_id)
        });
        Ok(CursorPage {
            items: rows.into_iter().map(|(item, _)| item).collect(),
            next_cursor,
            total_count,
        })
    }

    /// A bounded primary-key lookup of completed native restore evidence.
    /// Source identities, names, and unfinished work cannot manufacture proof.
    pub fn workbench_restore_proof(
        &self,
        workspace_id: &str,
        work_item_id: &str,
    ) -> Result<
        Option<restoreplane_domain::workbench_protection::WorkbenchRestoreProof>,
        RepositoryError,
    > {
        if !valid_workspace_id(workspace_id) || !valid_filter_text(work_item_id) {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let row:Option<(String,String,String,String,i64,String)>=reader.connection().query_row(
            "SELECT restore.id,restore.target_connection_profile_id,restore.target_database_name,profile.engine,restore.completed_at_ms,restore.validation_result_json FROM work_items work JOIN workspace_work_items ownership ON ownership.work_item_id=work.id JOIN restore_runs restore ON restore.id=work.restore_run_id JOIN connection_profiles profile ON profile.id=restore.target_connection_profile_id JOIN workspace_connection_profiles profile_owner ON profile_owner.connection_profile_id=restore.target_connection_profile_id AND profile_owner.workspace_id=ownership.workspace_id WHERE work.id=?1 AND ownership.workspace_id=?2 AND work.kind='restore' AND work.state='succeeded' AND restore.state='succeeded' AND restore.completed_at_ms IS NOT NULL AND length(restore.validation_result_json)<=1048576",
            params![work_item_id,workspace_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?)),
        ).optional()?;
        let Some((run, profile, database, engine, completed, raw)) = row else {
            return Ok(None);
        };
        let evidence: serde_json::Value =
            serde_json::from_str(&raw).map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        if evidence["phase"] != "completed"
            || evidence["restore"]["nativeExitCode"] != 0
            || evidence["restore"]["targetCreated"] != true
            || evidence["validation"]["status"] != "passed"
            || evidence["cleanup"]["required"] != false
        {
            return Ok(None);
        }
        let identity = &evidence["restore"]["evidence"]["workbenchRestoreIdentity"];
        if identity["signatureVersion"] != 1
            || identity["connectionProfileId"] != profile
            || identity["databaseName"] != database
            || identity["engine"] != engine
        {
            return Ok(None);
        }
        let proof=serde_json::from_value::<restoreplane_domain::workbench_protection::WorkbenchRestoreProof>(serde_json::json!({
            "restoreWorkItemId":work_item_id,"restoreRunId":run,"connectionProfileId":profile,
            "databaseName":database,"engine":engine,"serverIdentity":identity["serverIdentity"],
            "actualDatabaseIdentity":identity["actualDatabaseIdentity"],"completedAtMs":completed.to_string(),
        })).map_err(|_|RepositoryError::RuntimeStateInvalid)?;
        Ok(proof.is_well_formed().then_some(proof))
    }

    pub fn work_item_in_workspace(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> Result<ConsoleWorkItem, RepositoryError> {
        if !valid_workspace_id(workspace_id)
            || id.is_empty()
            || id.len() > MAX_CURSOR_BYTES
            || id.contains(['\0', '\r', '\n'])
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        struct WorkRow {
            work_item_id: String,
            state: String,
            kind: String,
            current_attempt_no: i64,
            fencing_token: i64,
            wait_reason: Option<String>,
            operation_run_id: Option<String>,
            operation_kind: Option<String>,
            result_json: Option<String>,
            error_code: Option<String>,
            created_at_ms: i64,
            updated_at_ms: i64,
            plan_id: Option<String>,
            plan_name: Option<String>,
            target_id: Option<String>,
            target_name: Option<String>,
            engine: Option<String>,
            backup_state: Option<String>,
            plaintext_bytes: Option<i64>,
        }
        let row: WorkRow = connection
            .query_row(
                "SELECT work.id, work.state, work.kind, work.current_attempt_no,
                        work.fencing_token, work.wait_reason,
                        operation.id, operation.operation_kind,
                        operation.result_json, operation.error_code,
                        work.created_at_ms, work.updated_at_ms,
                        plan.id, plan.display_name,
                        target.id, target.display_name,
                        backup.engine, backup.state, backup.plaintext_bytes
                 FROM work_items AS work
                 JOIN workspace_work_items AS ownership ON ownership.work_item_id = work.id
                 LEFT JOIN operation_runs AS operation
                   ON operation.id = work.operation_run_id
                 LEFT JOIN backup_runs AS backup
                   ON backup.id = work.backup_run_id
                 LEFT JOIN backup_plan_revisions AS revision
                   ON revision.id = backup.plan_revision_id
                 LEFT JOIN backup_plans AS plan
                   ON plan.id = revision.backup_plan_id
                 LEFT JOIN database_targets AS target
                   ON target.id = backup.database_target_id
                 WHERE ownership.workspace_id = ?1 AND work.id = ?2",
                params![workspace_id, id],
                |row| {
                    Ok(WorkRow {
                        work_item_id: row.get(0)?,
                        state: row.get(1)?,
                        kind: row.get(2)?,
                        current_attempt_no: row.get(3)?,
                        fencing_token: row.get(4)?,
                        wait_reason: row.get(5)?,
                        operation_run_id: row.get(6)?,
                        operation_kind: row.get(7)?,
                        result_json: row.get(8)?,
                        error_code: row.get(9)?,
                        created_at_ms: row.get(10)?,
                        updated_at_ms: row.get(11)?,
                        plan_id: row.get(12)?,
                        plan_name: row.get(13)?,
                        target_id: row.get(14)?,
                        target_name: row.get(15)?,
                        engine: row.get(16)?,
                        backup_state: row.get(17)?,
                        plaintext_bytes: row.get(18)?,
                    })
                },
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)?;
        let activity_after_seq = connection.query_row(
            "SELECT COALESCE(MAX(event.seq), 0)
             FROM activity_events AS event
             JOIN workspace_activity_events AS ownership
               ON ownership.activity_event_seq = event.seq
             WHERE ownership.workspace_id = ?1",
            [workspace_id],
            |row| row.get(0),
        )?;
        let result = row
            .result_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        let (discovered_count, eligible_discovered_count) =
            if row.operation_kind.as_deref() == Some("connection_discovery") {
                let Some(discovery_run_id) = row.operation_run_id.as_deref() else {
                    return Err(RepositoryError::RuntimeStateInvalid);
                };
                connection.query_row(
                    "SELECT COUNT(*),
                        COUNT(CASE WHEN eligibility = 'eligible' THEN 1 END)
                 FROM discovered_databases
                 WHERE discovery_run_id = ?1",
                    [discovery_run_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
            } else {
                (0, 0)
            };
        let discovered = if row.operation_kind.as_deref() == Some("connection_discovery") {
            let discovery_run_id = row
                .operation_run_id
                .as_deref()
                .ok_or(RepositoryError::RuntimeStateInvalid)?;
            let mut statement = connection.prepare(
                "SELECT engine_object_id, database_name, eligibility, exclusion_reason
                 FROM discovered_databases
                 WHERE discovery_run_id = ?1
                 ORDER BY created_at_ms, engine_object_id
                 LIMIT 100",
            )?;
            let records = statement
                .query_map([discovery_run_id], |row| {
                    Ok(ConsoleDiscoveredDatabase {
                        engine_object_id: row.get(0)?,
                        database_name: row.get(1)?,
                        eligibility: row.get(2)?,
                        exclusion_reason: row.get(3)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            records
        } else {
            Vec::new()
        };
        let (subject, progress) = if row.kind == "backup" {
            if row.plan_id.is_none()
                || row.plan_name.is_none()
                || row.target_id.is_none()
                || row.target_name.is_none()
                || row.engine.is_none()
                || row.backup_state.is_none()
            {
                return Err(RepositoryError::RuntimeStateInvalid);
            }
            let measured_bytes =
                if row.state == "succeeded" && row.backup_state.as_deref() == Some("succeeded") {
                    row.plaintext_bytes
                } else {
                    None
                };
            (
                Some(ConsoleWorkItemSubject {
                    plan_id: row.plan_id,
                    plan_name: row.plan_name,
                    target_id: row.target_id,
                    target_name: row.target_name,
                    engine: row.engine,
                }),
                Some(ConsoleWorkItemProgress {
                    phase: row.state.clone(),
                    completed_units: measured_bytes,
                    total_units: measured_bytes,
                    unit: "bytes".to_owned(),
                    current_object: None,
                    updated_at_ms: Some(row.updated_at_ms),
                }),
            )
        } else {
            (None, None)
        };
        Ok(ConsoleWorkItem {
            work_item_id: row.work_item_id,
            state: row.state,
            kind: row.kind,
            current_attempt_no: row.current_attempt_no,
            fencing_token: row.fencing_token,
            wait_reason: row.wait_reason,
            activity_after_seq,
            operation_run_id: row.operation_run_id,
            operation_kind: row.operation_kind,
            result,
            error_code: row.error_code,
            discovered,
            discovered_count,
            eligible_discovered_count,
            subject,
            progress,
            created_at_ms: row.created_at_ms,
            updated_at_ms: row.updated_at_ms,
        })
    }
}

fn restore_test_target_from_config(adapter_config_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(adapter_config_json)
        .ok()
        .and_then(|value| {
            value
                .get("restoreTestTarget")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false)
}

fn matrix_support_status(
    persisted_status: &str,
    certified_at_ms: Option<i64>,
    expires_at_ms: Option<i64>,
    now_ms: i64,
    capabilities: &[ConsoleEngineCapability],
) -> String {
    if persisted_status == "suspended" {
        return "suspended".to_owned();
    }
    if persisted_status == "expired"
        || expires_at_ms.is_some_and(|expires_at_ms| expires_at_ms <= now_ms)
    {
        return "expired".to_owned();
    }
    if persisted_status != "certified"
        || certified_at_ms.is_none_or(|certified_at_ms| certified_at_ms > now_ms)
        || capabilities.len() != ProviderCapability::REQUIRED.len()
    {
        return "candidate".to_owned();
    }
    if capabilities
        .iter()
        .any(|capability| capability.support_status == "failed")
    {
        return "experimental".to_owned();
    }
    if capabilities
        .iter()
        .any(|capability| capability.support_status == "not_applicable")
    {
        return "unsupported".to_owned();
    }
    if capabilities
        .iter()
        .any(|capability| capability.support_status == "limited")
    {
        return "limited".to_owned();
    }
    if capabilities
        .iter()
        .all(|capability| capability.support_status == "passed")
    {
        "supported".to_owned()
    } else {
        "candidate".to_owned()
    }
}

fn validate_snapshot_query(query: &ConsoleSnapshotQuery) -> Result<(), RepositoryError> {
    validate_inventory_page(query.cursor.as_deref(), query.limit)?;
    let scope_valid = matches!(query.scope.as_str(), "complete" | "diagnostics" | "all");
    let search_valid = query.search.as_deref().is_none_or(|value| {
        let value = value.trim();
        !value.is_empty() && value.len() <= MAX_CURSOR_BYTES && !value.contains(['\0', '\r', '\n'])
    });
    let connection_valid = query.connection_id.as_deref().is_none_or(valid_filter_text);
    let engine_valid = query
        .engine
        .as_deref()
        .is_none_or(|value| valid_filter_text(value) && value.len() <= 64);
    let range_valid = query.from_ms.is_none_or(|value| value >= 0)
        && query.to_ms.is_none_or(|value| value >= 0)
        && match (query.from_ms, query.to_ms) {
            (Some(from), Some(to)) => from <= to,
            _ => true,
        };
    let artifact_valid = query
        .artifact_completeness
        .as_deref()
        .is_none_or(|value| matches!(value, "complete" | "incomplete"));
    let durability_valid = query
        .replica_durability
        .as_deref()
        .is_none_or(|value| matches!(value, "healthy" | "degraded" | "unavailable"));
    let compliance_valid = query
        .protection_compliance
        .as_deref()
        .is_none_or(|value| matches!(value, "compliant" | "non_compliant" | "unknown"));
    let verification_valid = query
        .min_verification_level
        .is_none_or(|value| (1..=3).contains(&value));
    let eligibility_valid = query
        .restore_eligibility
        .as_deref()
        .is_none_or(|value| matches!(value, "eligible" | "conditional" | "blocked"));
    let signature_valid = query
        .signature_status
        .as_deref()
        .is_none_or(|value| matches!(value, "valid" | "invalid" | "unknown_key" | "not_checked"));
    if scope_valid
        && search_valid
        && connection_valid
        && engine_valid
        && range_valid
        && artifact_valid
        && durability_valid
        && compliance_valid
        && verification_valid
        && eligibility_valid
        && signature_valid
    {
        Ok(())
    } else {
        Err(RepositoryError::RuntimeStateInvalid)
    }
}

fn valid_filter_text(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_CURSOR_BYTES && !value.contains(['\0', '\r', '\n'])
}

fn valid_workspace_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn snapshot_select_sql(count_only: bool) -> String {
    let select = if count_only {
        "SELECT COUNT(*)".to_owned()
    } else {
        "SELECT recovery.id, recovery.source_target_identity,
                recovery.source_display_name, recovery.engine, recovery.state,
                recovery.committed_at_ms, recovery.manifest_schema_version,
                recovery.set_manifest_checksum_blake3, recovery.signing_key_id,
                recovery.signature_status, recovery.required_part_count,
                recovery.present_part_count, recovery.plaintext_bytes,
                recovery.cipher_bytes, recovery.first_ingested_at_ms,
                recovery.last_ingested_at_ms,
                COALESCE(snapshot.artifact_completeness, 'incomplete'),
                COALESCE(snapshot.replica_durability, 'unavailable'),
                COALESCE(snapshot.protection_compliance, 'unknown'),
                COALESCE(snapshot.restore_eligibility, 'blocked'),
                snapshot.highest_verified_level, snapshot.last_verified_at_ms,
                source_connection.id, source_connection.display_name,
                (SELECT COUNT(*) FROM catalog_artifact_parts AS part
                 WHERE part.recovery_point_id = recovery.id),
                (SELECT COUNT(*) FROM catalog_artifact_replicas AS replica
                 WHERE replica.recovery_point_id = recovery.id),
                (SELECT COUNT(*) FROM catalog_verification_projection AS evidence
                 WHERE evidence.recovery_point_id = recovery.id),
                (SELECT COUNT(*)
                 FROM restore_runs AS restore
                 JOIN verification_runs AS verification
                   ON verification.restore_run_id = restore.id
                  AND verification.trigger_kind = 'restore_drill'
                  AND verification.requested_level = 3
                 WHERE restore.recovery_point_id = recovery.id
                   AND restore.purpose = 'restore_drill')"
            .to_owned()
    };
    let mut sql = format!(
        "{select}
         FROM catalog_recovery_points AS recovery
         JOIN workspace_recovery_points AS ownership
           ON ownership.recovery_point_id = recovery.id
         LEFT JOIN catalog_snapshot_projection AS snapshot
           ON snapshot.recovery_point_id = recovery.id
         LEFT JOIN database_targets AS source_target
           ON source_target.id = (
                SELECT candidate.id
                FROM database_targets AS candidate
                WHERE candidate.source_fingerprint = recovery.source_target_identity
                ORDER BY
                    CASE candidate.state WHEN 'active' THEN 0 ELSE 1 END,
                    candidate.id
                LIMIT 1
           )
         LEFT JOIN connection_profiles AS source_connection
           ON source_connection.id = source_target.connection_profile_id
         WHERE ownership.workspace_id = ?1
           AND (
             ?2 = 'all'
             OR (?2 = 'complete'
                 AND recovery.state = 'complete'
                 AND recovery.signature_status = 'valid')
             OR (?2 = 'diagnostics'
                 AND (recovery.state <> 'complete'
                      OR recovery.signature_status <> 'valid'))
         )
           AND (
               ?3 IS NULL
               OR LOWER(
                   recovery.source_display_name || ' ' ||
                   recovery.id || ' ' ||
                   recovery.source_target_identity || ' ' ||
                   recovery.engine || ' ' ||
                   COALESCE(source_connection.display_name, '') || ' ' ||
                   COALESCE(snapshot.search_text, '')
               ) LIKE ?3
           )
           AND (?4 IS NULL OR source_connection.id = ?4)
           AND (?5 IS NULL OR recovery.engine = ?5)
           AND (?6 IS NULL OR recovery.committed_at_ms >= ?6)
           AND (?7 IS NULL OR recovery.committed_at_ms <= ?7)
           AND (
               ?8 IS NULL
               OR COALESCE(snapshot.artifact_completeness, 'incomplete') = ?8
           )
           AND (
               ?9 IS NULL
               OR COALESCE(snapshot.replica_durability, 'unavailable') = ?9
           )
           AND (
               ?10 IS NULL
               OR COALESCE(snapshot.protection_compliance, 'unknown') = ?10
           )
           AND (
               ?11 IS NULL
               OR COALESCE(snapshot.highest_verified_level, 0) >= ?11
           )
           AND (
               ?12 IS NULL
               OR COALESCE(snapshot.restore_eligibility, 'blocked') = ?12
           )
           AND (?13 IS NULL OR recovery.signature_status = ?13)"
    );
    if !count_only {
        sql.push_str(
            "
           AND (
               ?14 IS NULL
               OR COALESCE(recovery.committed_at_ms, -1) < ?14
               OR (
                   COALESCE(recovery.committed_at_ms, -1) = ?14
                   AND recovery.id < ?15
               )
           )
         ORDER BY COALESCE(recovery.committed_at_ms, -1) DESC, recovery.id DESC
         LIMIT ?16",
        );
    }
    sql
}

fn parse_snapshot_cursor(cursor: &str) -> Result<(i64, String), RepositoryError> {
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES || cursor.contains(['\0', '\r', '\n']) {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let (committed_at, id) = cursor
        .split_once(':')
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let committed_at = committed_at
        .parse::<i64>()
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if committed_at < -1 || !valid_filter_text(id) {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok((committed_at, id.to_owned()))
}

fn snapshot_diagnostic_reason(state: &str, signature_status: &str) -> Option<String> {
    let reason = match signature_status {
        "invalid" => "manifest_invalid_signature",
        "unknown_key" => "manifest_unknown_key",
        "not_checked" => "manifest_not_checked",
        _ => match state {
            "incomplete" => "manifest_incomplete",
            "orphaned" => "manifest_orphaned",
            "quarantined" => "manifest_quarantined",
            _ => return None,
        },
    };
    Some(reason.to_owned())
}

fn validate_inventory_page(cursor: Option<&str>, limit: usize) -> Result<(), RepositoryError> {
    if limit == 0
        || limit > MAX_INVENTORY_PAGE
        || cursor.is_some_and(|value| {
            value.is_empty() || value.len() > MAX_CURSOR_BYTES || value.contains(['\0', '\r', '\n'])
        })
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_scope_page_input(
    workspace_id: &str,
    database_target_id: Option<&str>,
    cursor: Option<&str>,
    limit: usize,
    state: Option<&str>,
    search: Option<&str>,
    now_ms: i64,
) -> Result<(), RepositoryError> {
    validate_inventory_page(cursor, limit)?;
    if !valid_workspace_id(workspace_id)
        || now_ms < 0
        || database_target_id.is_some_and(|value| !valid_filter_text(value))
        || state
            .is_some_and(|value| !matches!(value, "active" | "attention" | "disabled" | "retired"))
        || search.is_some_and(|value| {
            let value = value.trim();
            value.is_empty() || value.len() > MAX_CURSOR_BYTES || value.contains(['\0', '\r', '\n'])
        })
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_target_page_input(
    workspace_id: &str,
    cursor: Option<&str>,
    limit: usize,
    connection_id: Option<&str>,
    state: Option<&str>,
    search: Option<&str>,
    now_ms: i64,
) -> Result<(), RepositoryError> {
    validate_inventory_page(cursor, limit)?;
    if !valid_workspace_id(workspace_id)
        || now_ms < 0
        || connection_id.is_some_and(|value| !valid_filter_text(value))
        || state
            .is_some_and(|value| !matches!(value, "active" | "attention" | "disabled" | "retired"))
        || !valid_optional_search(search)
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(())
}

fn validate_discovery_page_input(
    workspace_id: &str,
    discovery_run_id: &str,
    cursor: Option<&str>,
    limit: usize,
    search: Option<&str>,
    eligibility: Option<&str>,
) -> Result<(), RepositoryError> {
    validate_inventory_page(cursor, limit)?;
    if !valid_workspace_id(workspace_id)
        || !valid_filter_text(discovery_run_id)
        || !valid_optional_search(search)
        || eligibility
            .is_some_and(|value| !matches!(value, "eligible" | "excluded" | "offline" | "unknown"))
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(())
}

fn valid_optional_search(search: Option<&str>) -> bool {
    search.is_none_or(|value| {
        let value = value.trim();
        !value.is_empty() && value.len() <= MAX_CURSOR_BYTES && !value.contains(['\0', '\r', '\n'])
    })
}

fn parse_scope_cursor(cursor: &str) -> Result<(i64, String), RepositoryError> {
    let (created_at_ms, id) = cursor
        .split_once(':')
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let created_at_ms = created_at_ms
        .parse::<i64>()
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if created_at_ms < 0 || !valid_filter_text(id) {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok((created_at_ms, id.to_owned()))
}

fn parse_created_cursor(cursor: &str) -> Result<(i64, String), RepositoryError> {
    parse_scope_cursor(cursor)
}

fn scope_cursor(created_at_ms: i64, id: &str) -> String {
    format!("{created_at_ms}:{id}")
}

fn created_cursor(created_at_ms: i64, id: &str) -> String {
    scope_cursor(created_at_ms, id)
}

fn scope_search_pattern(search: &str) -> Result<String, RepositoryError> {
    let search = search.trim().to_lowercase();
    if search.is_empty() {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let mut escaped = String::with_capacity(search.len() + 2);
    escaped.push('%');
    for character in search.chars() {
        if matches!(character, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped.push('%');
    Ok(escaped)
}

fn unix_time_ms() -> Result<i64, RepositoryError> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    i64::try_from(elapsed.as_millis()).map_err(|_| RepositoryError::RuntimeStateInvalid)
}

fn page_fetch_limit(limit: usize) -> Result<i64, RepositoryError> {
    i64::try_from(limit.saturating_add(1)).map_err(|_| RepositoryError::RuntimeStateInvalid)
}

fn trim_page<T>(
    items: &mut Vec<T>,
    limit: usize,
    cursor: impl FnOnce(&T) -> String,
) -> Option<String> {
    if items.len() <= limit {
        return None;
    }
    items.truncate(limit);
    items.last().map(cursor)
}

fn trim_serialized_page<T: Serialize>(
    items: &mut Vec<T>,
    limit: usize,
    cursor: impl Fn(&T) -> String,
) -> Result<Option<String>, RepositoryError> {
    let database_has_more = items.len() > limit;
    if database_has_more {
        items.truncate(limit);
    }
    let mut encoded_bytes = 2_048usize;
    let mut keep = 0usize;
    for item in items.iter() {
        let item_bytes = serde_json::to_vec(item)
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?
            .len();
        if encoded_bytes.saturating_add(item_bytes).saturating_add(1) > MAX_RESPONSE_PAGE_BYTES {
            break;
        }
        encoded_bytes = encoded_bytes.saturating_add(item_bytes).saturating_add(1);
        keep += 1;
    }
    if keep == 0 && !items.is_empty() {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let byte_has_more = keep < items.len();
    if byte_has_more {
        items.truncate(keep);
    }
    Ok((database_has_more || byte_has_more)
        .then(|| items.last().map(cursor))
        .flatten())
}

fn truncate_global_rows<T: Serialize>(items: &mut Vec<T>) -> Result<bool, RepositoryError> {
    let mut encoded_bytes = 2_048usize;
    retain_serialized_under_budget(items, &mut encoded_bytes)
}

fn truncate_settings_inventory(
    resource_pools: &mut Vec<ConsoleResourcePool>,
    spool_volumes: &mut Vec<ConsoleSpoolVolume>,
    tools: &mut Vec<ConsoleToolInventoryItem>,
    notification_channels: &mut Vec<ConsoleNotificationChannel>,
) -> Result<bool, RepositoryError> {
    let mut encoded_bytes = 2_048usize;
    let mut complete = true;
    complete &= retain_serialized_under_budget(resource_pools, &mut encoded_bytes)?;
    complete &= retain_serialized_under_budget(spool_volumes, &mut encoded_bytes)?;
    complete &= retain_serialized_under_budget(tools, &mut encoded_bytes)?;
    complete &= retain_serialized_under_budget(notification_channels, &mut encoded_bytes)?;
    Ok(complete)
}

fn retain_serialized_under_budget<T: Serialize>(
    items: &mut Vec<T>,
    encoded_bytes: &mut usize,
) -> Result<bool, RepositoryError> {
    let original_len = items.len();
    let mut keep = 0usize;
    for item in items.iter() {
        let item_bytes = serde_json::to_vec(item)
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?
            .len();
        if encoded_bytes.saturating_add(item_bytes).saturating_add(1) > MAX_RESPONSE_PAGE_BYTES {
            break;
        }
        *encoded_bytes = encoded_bytes.saturating_add(item_bytes).saturating_add(1);
        keep += 1;
    }
    items.truncate(keep);
    Ok(keep == original_len)
}

fn trim_activity_page(
    items: &mut Vec<ConsoleActivityEvent>,
    limit: usize,
) -> Result<Option<String>, RepositoryError> {
    let database_has_more = items.len() > limit;
    if database_has_more {
        items.truncate(limit);
    }
    let mut encoded_bytes = 128usize;
    let mut keep = 0usize;
    for item in items.iter() {
        let item_bytes = serde_json::to_vec(item)
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?
            .len();
        if encoded_bytes.saturating_add(item_bytes).saturating_add(1) > MAX_RESPONSE_PAGE_BYTES {
            break;
        }
        encoded_bytes = encoded_bytes.saturating_add(item_bytes).saturating_add(1);
        keep += 1;
    }
    if keep == 0 && !items.is_empty() {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let byte_has_more = keep < items.len();
    if byte_has_more {
        items.truncate(keep);
    }
    if database_has_more || byte_has_more {
        Ok(items
            .last()
            .map(|item| format!("{}:{}", item.occurred_at_ms, item.seq)))
    } else {
        Ok(None)
    }
}

fn parse_activity_cursor(cursor: &str) -> Result<(i64, i64), RepositoryError> {
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES || cursor.contains(['\0', '\r', '\n']) {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let (occurred_at, seq) = cursor
        .split_once(':')
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let occurred_at = occurred_at
        .parse::<i64>()
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    let seq = seq
        .parse::<i64>()
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if occurred_at < 0 || seq <= 0 {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok((occurred_at, seq))
}

fn endpoint_for_connection(
    host: Option<&str>,
    port: Option<i64>,
    file_path: Option<&str>,
) -> String {
    match (host, port, file_path) {
        (Some(host), Some(port), _) => format!("{host}:{port}"),
        (_, _, Some(path)) => path.to_owned(),
        (Some(host), None, _) => host.to_owned(),
        _ => "endpoint unavailable".to_owned(),
    }
}

fn roll_up_target_health(
    target: &mut ConsoleDatabaseTarget,
    scope_health: &[ConsoleBackupScopeHealth],
) {
    let active = scope_health
        .iter()
        .filter(|scope| {
            scope.database_target_id == target.id
                && scope.active_plan_count > 0
                && !matches!(scope.state.as_str(), "disabled" | "retired")
        })
        .collect::<Vec<_>>();
    let whole_database = active.iter().any(|scope| scope.mode == "whole_database");
    let protected = active
        .into_iter()
        .filter(|scope| !whole_database || scope.mode == "whole_database")
        .collect::<Vec<_>>();
    if protected.is_empty() {
        return;
    }

    target.protection_scope = if whole_database {
        "whole_database".to_owned()
    } else {
        "partial".to_owned()
    };
    target.latest_failure_at_ms = protected
        .iter()
        .filter_map(|scope| scope.latest_failure_at_ms)
        .max();
    target.rpo_violation = protected.iter().any(|scope| scope.rpo_violation);
    target.verification_state = if protected
        .iter()
        .any(|scope| scope.verification_state == "unknown")
    {
        "unknown"
    } else if protected
        .iter()
        .any(|scope| scope.verification_state == "pending")
    {
        "pending"
    } else if protected
        .iter()
        .all(|scope| scope.verification_state == "current")
    {
        "current"
    } else {
        "not_configured"
    }
    .to_owned();
    target.verification_overdue = protected.iter().any(|scope| scope.verification_overdue);
    target.evidence_at_ms = protected
        .iter()
        .filter_map(|scope| scope.evidence_at_ms)
        .max();

    let latest = protected
        .iter()
        .filter(|scope| scope.latest_recovery_point_id.is_some())
        .max_by(|left, right| {
            left.latest_success_at_ms
                .cmp(&right.latest_success_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });
    if let Some(scope) = latest {
        target.latest_recovery_point_id = scope.latest_recovery_point_id.clone();
        target.latest_success_at_ms = scope.latest_success_at_ms;
        target.artifact_completeness = scope.artifact_completeness.clone();
        target.replica_durability = scope.replica_durability.clone();
        target.protection_compliance = if whole_database {
            scope.protection_compliance.clone()
        } else {
            "partial".to_owned()
        };
    } else {
        target.protection_compliance = if whole_database {
            "unknown".to_owned()
        } else {
            "partial".to_owned()
        };
    }
}

fn public_destination_endpoint(kind: &str, endpoint_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(endpoint_json) else {
        return "endpoint unavailable".to_owned();
    };
    let text = |key: &str| value.get(key).and_then(serde_json::Value::as_str);
    match kind {
        "local" => text("path").unwrap_or("path unavailable").to_owned(),
        "s3" => {
            let bucket = text("bucket").unwrap_or("bucket unavailable");
            let prefix = text("prefix").unwrap_or_default().trim_matches('/');
            if prefix.is_empty() {
                bucket.to_owned()
            } else {
                format!("{bucket}/{prefix}")
            }
        }
        "ssh_sftp" => {
            let host = text("host").unwrap_or("host unavailable");
            let path = text("path").unwrap_or("path unavailable");
            format!("{host}:{path}")
        }
        "azure_blob" => {
            let container = text("container").unwrap_or("container unavailable");
            let prefix = text("prefix").unwrap_or_default().trim_matches('/');
            if prefix.is_empty() {
                container.to_owned()
            } else {
                format!("{container}/{prefix}")
            }
        }
        "smb" => text("share").unwrap_or("share unavailable").to_owned(),
        "rclone" => {
            let remote = text("remote").unwrap_or("remote unavailable");
            let path = text("path").unwrap_or_default().trim_matches('/');
            if path.is_empty() {
                remote.to_owned()
            } else {
                format!("{remote}:{path}")
            }
        }
        _ => "endpoint unavailable".to_owned(),
    }
}

fn destination_roles(
    connection: &rusqlite::Connection,
    destination_id: &str,
) -> Result<Vec<ConsoleDestinationRole>, RepositoryError> {
    ["backup_writer", "restore_reader", "delete_operator"]
        .into_iter()
        .map(|role| {
            let credential_bound = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM destination_secret_bindings
                     WHERE destination_id = ?1 AND secret_role = ?2
                 )",
                params![destination_id, role],
                |row| row.get::<_, bool>(0),
            )?;
            let probe: Option<(String, i64)> = connection
                .query_row(
                    "SELECT status, started_at_ms
                     FROM destination_probe_runs
                     WHERE destination_id = ?1 AND credential_role = ?2
                     ORDER BY started_at_ms DESC, id DESC
                     LIMIT 1",
                    params![destination_id, role],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            Ok(ConsoleDestinationRole {
                role: role.to_owned(),
                credential_bound,
                probe_status: probe.as_ref().map(|value| value.0.clone()),
                probed_at_ms: probe.map(|value| value.1),
            })
        })
        .collect()
}
