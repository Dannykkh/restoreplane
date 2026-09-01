use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::OperationalStore;

use super::{ConsoleSnapshotPart, ConsoleSnapshotReplica, RepositoryError};

const MAX_SNAPSHOT_DETAIL_PAGE: usize = 100;
const MAX_SNAPSHOT_DETAIL_PAGE_BYTES: usize = 512 * 1024;
const MAX_CURSOR_BYTES: usize = 256;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSnapshotVerificationSummary {
    pub id: String,
    pub level: i64,
    pub result: String,
    pub verified_at_ms: i64,
    pub tool_name: Option<String>,
    pub tool_version: Option<String>,
    pub evidence_checksum_blake3: Option<String>,
    pub evidence_locator: Option<String>,
    pub evidence_available: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSnapshotRestoreDrillSummary {
    pub restore_run_id: String,
    pub verification_run_id: String,
    pub target_connection_id: String,
    pub target_connection_name: String,
    pub target_name: String,
    pub state: String,
    pub measured_rto_ms: Option<i64>,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub error_code: Option<String>,
    pub evidence_available: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "section", rename_all = "snake_case")]
pub enum ConsoleSnapshotDetailPage {
    Parts {
        items: Vec<ConsoleSnapshotPart>,
        #[serde(rename = "nextCursor")]
        next_cursor: Option<String>,
        #[serde(rename = "totalCount")]
        total_count: i64,
        complete: bool,
    },
    Replicas {
        items: Vec<ConsoleSnapshotReplica>,
        #[serde(rename = "nextCursor")]
        next_cursor: Option<String>,
        #[serde(rename = "totalCount")]
        total_count: i64,
        complete: bool,
    },
    VerificationEvidence {
        items: Vec<ConsoleSnapshotVerificationSummary>,
        #[serde(rename = "nextCursor")]
        next_cursor: Option<String>,
        #[serde(rename = "totalCount")]
        total_count: i64,
        complete: bool,
    },
    RestoreDrills {
        items: Vec<ConsoleSnapshotRestoreDrillSummary>,
        #[serde(rename = "nextCursor")]
        next_cursor: Option<String>,
        #[serde(rename = "totalCount")]
        total_count: i64,
        complete: bool,
    },
}

impl ConsoleSnapshotDetailPage {
    pub const fn complete(&self) -> bool {
        match self {
            Self::Parts { complete, .. }
            | Self::Replicas { complete, .. }
            | Self::VerificationEvidence { complete, .. }
            | Self::RestoreDrills { complete, .. } => *complete,
        }
    }
}

#[derive(Clone)]
pub struct SnapshotDetailRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn snapshot_detail(&self) -> SnapshotDetailRepository {
        SnapshotDetailRepository {
            store: self.clone(),
        }
    }
}

impl SnapshotDetailRepository {
    pub fn page_in_workspace(
        &self,
        workspace_id: &str,
        recovery_point_id: &str,
        section: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ConsoleSnapshotDetailPage, RepositoryError> {
        validate_page_input(workspace_id, recovery_point_id, cursor, limit)?;
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let owned = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM workspace_recovery_points
                 WHERE workspace_id = ?1 AND recovery_point_id = ?2
             )",
            params![workspace_id, recovery_point_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !owned {
            return Err(RepositoryError::NotFound);
        }

        match section {
            "parts" => part_page(connection, recovery_point_id, cursor, limit),
            "replicas" => replica_page(connection, recovery_point_id, cursor, limit),
            "verification_evidence" => {
                verification_page(connection, recovery_point_id, cursor, limit)
            }
            "restore_drills" => restore_drill_page(connection, recovery_point_id, cursor, limit),
            _ => Err(RepositoryError::RuntimeStateInvalid),
        }
    }

    pub fn eligible_replica_in_workspace(
        &self,
        workspace_id: &str,
        recovery_point_id: &str,
        requested_replica_id: Option<&str>,
    ) -> Result<ConsoleSnapshotReplica, RepositoryError> {
        if !valid_uuid(workspace_id)
            || !valid_uuid(recovery_point_id)
            || requested_replica_id.is_some_and(|value| !valid_uuid(value))
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT replica.id, replica.destination_fingerprint,
                        replica.destination_kind, replica.requirement, replica.state,
                        replica.immutability_capability, replica.retention_until_ms,
                        replica.reader_status, replica.last_observed_at_ms
                 FROM catalog_artifact_replicas AS replica
                 JOIN workspace_recovery_points AS ownership
                   ON ownership.recovery_point_id = replica.recovery_point_id
                 WHERE ownership.workspace_id = ?1
                   AND replica.recovery_point_id = ?2
                   AND (?3 IS NULL OR replica.id = ?3)
                   AND replica.state = 'committed'
                   AND replica.reader_status = 'eligible'
                 ORDER BY
                   CASE replica.requirement WHEN 'required' THEN 0 ELSE 1 END,
                   replica.id
                 LIMIT 1",
                params![workspace_id, recovery_point_id, requested_replica_id],
                replica_from_row,
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }
}

fn part_page(
    connection: &rusqlite::Connection,
    recovery_point_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<ConsoleSnapshotDetailPage, RepositoryError> {
    let total_count = connection.query_row(
        "SELECT COUNT(*) FROM catalog_artifact_parts WHERE recovery_point_id = ?1",
        [recovery_point_id],
        |row| row.get(0),
    )?;
    let mut statement = connection.prepare(
        "SELECT id, part_role, logical_name, required, dump_format,
                plaintext_bytes, plaintext_checksum_blake3
         FROM catalog_artifact_parts
         WHERE recovery_point_id = ?1 AND (?2 IS NULL OR id > ?2)
         ORDER BY id
         LIMIT ?3",
    )?;
    let rows = statement
        .query_map(
            params![recovery_point_id, cursor, fetch_limit(limit)?],
            |row| {
                Ok(ConsoleSnapshotPart {
                    id: row.get(0)?,
                    role: row.get(1)?,
                    logical_name: row.get(2)?,
                    required: row.get::<_, i64>(3)? != 0,
                    dump_format: row.get(4)?,
                    plaintext_bytes: row.get(5)?,
                    checksum_blake3: row.get(6)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let (items, next_cursor) = bound_page(rows, limit, |row| row.id.clone())?;
    Ok(ConsoleSnapshotDetailPage::Parts {
        complete: next_cursor.is_none(),
        items,
        next_cursor,
        total_count,
    })
}

fn replica_page(
    connection: &rusqlite::Connection,
    recovery_point_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<ConsoleSnapshotDetailPage, RepositoryError> {
    let total_count = connection.query_row(
        "SELECT COUNT(*) FROM catalog_artifact_replicas WHERE recovery_point_id = ?1",
        [recovery_point_id],
        |row| row.get(0),
    )?;
    let mut statement = connection.prepare(
        "SELECT id, destination_fingerprint, destination_kind, requirement,
                state, immutability_capability, retention_until_ms,
                reader_status, last_observed_at_ms
         FROM catalog_artifact_replicas
         WHERE recovery_point_id = ?1 AND (?2 IS NULL OR id > ?2)
         ORDER BY id
         LIMIT ?3",
    )?;
    let rows = statement
        .query_map(
            params![recovery_point_id, cursor, fetch_limit(limit)?],
            replica_from_row,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let (items, next_cursor) = bound_page(rows, limit, |row| row.id.clone())?;
    Ok(ConsoleSnapshotDetailPage::Replicas {
        complete: next_cursor.is_none(),
        items,
        next_cursor,
        total_count,
    })
}

fn verification_page(
    connection: &rusqlite::Connection,
    recovery_point_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<ConsoleSnapshotDetailPage, RepositoryError> {
    let cursor = cursor.map(parse_time_cursor).transpose()?;
    let (cursor_time, cursor_id) = cursor
        .as_ref()
        .map(|(time, id)| (Some(*time), Some(id.as_str())))
        .unwrap_or((None, None));
    let total_count = connection.query_row(
        "SELECT COUNT(*) FROM catalog_verification_projection WHERE recovery_point_id = ?1",
        [recovery_point_id],
        |row| row.get(0),
    )?;
    let mut statement = connection.prepare(
        "SELECT id, verify_level, result, verified_at_ms, tool_name, tool_version,
                evidence_checksum_blake3, evidence_locator,
                evidence_json <> '{}' AND evidence_json <> 'null'
         FROM catalog_verification_projection
         WHERE recovery_point_id = ?1
           AND (
             ?2 IS NULL OR verified_at_ms < ?2
             OR (verified_at_ms = ?2 AND id < ?3)
           )
         ORDER BY verified_at_ms DESC, id DESC
         LIMIT ?4",
    )?;
    let rows = statement
        .query_map(
            params![
                recovery_point_id,
                cursor_time,
                cursor_id,
                fetch_limit(limit)?
            ],
            |row| {
                Ok(ConsoleSnapshotVerificationSummary {
                    id: row.get(0)?,
                    level: row.get(1)?,
                    result: row.get(2)?,
                    verified_at_ms: row.get(3)?,
                    tool_name: row.get(4)?,
                    tool_version: row.get(5)?,
                    evidence_checksum_blake3: row.get(6)?,
                    evidence_locator: row.get(7)?,
                    evidence_available: row.get::<_, i64>(8)? != 0,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let (items, next_cursor) = bound_page(rows, limit, |row| {
        format!("{}:{}", row.verified_at_ms, row.id)
    })?;
    Ok(ConsoleSnapshotDetailPage::VerificationEvidence {
        complete: next_cursor.is_none(),
        items,
        next_cursor,
        total_count,
    })
}

fn restore_drill_page(
    connection: &rusqlite::Connection,
    recovery_point_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<ConsoleSnapshotDetailPage, RepositoryError> {
    let cursor = cursor.map(parse_time_cursor).transpose()?;
    let (cursor_time, cursor_id) = cursor
        .as_ref()
        .map(|(time, id)| (Some(*time), Some(id.as_str())))
        .unwrap_or((None, None));
    let total_count = connection.query_row(
        "SELECT COUNT(*)
         FROM restore_runs AS restore
         JOIN verification_runs AS verification
           ON verification.restore_run_id = restore.id
          AND verification.trigger_kind = 'restore_drill'
          AND verification.requested_level = 3
         WHERE restore.recovery_point_id = ?1 AND restore.purpose = 'restore_drill'",
        [recovery_point_id],
        |row| row.get(0),
    )?;
    let mut statement = connection.prepare(
        "SELECT restore.id, verification.id,
                restore.target_connection_profile_id, profile.display_name,
                restore.target_database_name, restore.state, restore.measured_rto_ms,
                restore.started_at_ms, restore.completed_at_ms, restore.error_code,
                restore.validation_result_json IS NOT NULL,
                restore.created_at_ms
         FROM restore_runs AS restore
         JOIN verification_runs AS verification
           ON verification.restore_run_id = restore.id
          AND verification.trigger_kind = 'restore_drill'
          AND verification.requested_level = 3
         JOIN connection_profiles AS profile
           ON profile.id = restore.target_connection_profile_id
         WHERE restore.recovery_point_id = ?1
           AND restore.purpose = 'restore_drill'
           AND (
             ?2 IS NULL OR restore.created_at_ms < ?2
             OR (restore.created_at_ms = ?2 AND restore.id < ?3)
           )
         ORDER BY restore.created_at_ms DESC, restore.id DESC
         LIMIT ?4",
    )?;
    let rows = statement
        .query_map(
            params![
                recovery_point_id,
                cursor_time,
                cursor_id,
                fetch_limit(limit)?
            ],
            |row| {
                Ok((
                    ConsoleSnapshotRestoreDrillSummary {
                        restore_run_id: row.get(0)?,
                        verification_run_id: row.get(1)?,
                        target_connection_id: row.get(2)?,
                        target_connection_name: row.get(3)?,
                        target_name: row.get(4)?,
                        state: row.get(5)?,
                        measured_rto_ms: row.get(6)?,
                        started_at_ms: row.get(7)?,
                        completed_at_ms: row.get(8)?,
                        error_code: row.get(9)?,
                        evidence_available: row.get::<_, i64>(10)? != 0,
                    },
                    row.get::<_, i64>(11)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let (items, next_cursor) = bound_page(rows, limit, |(row, created_at_ms)| {
        format!("{created_at_ms}:{}", row.restore_run_id)
    })?;
    Ok(ConsoleSnapshotDetailPage::RestoreDrills {
        complete: next_cursor.is_none(),
        items: items.into_iter().map(|(item, _)| item).collect(),
        next_cursor,
        total_count,
    })
}

fn replica_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConsoleSnapshotReplica> {
    Ok(ConsoleSnapshotReplica {
        id: row.get(0)?,
        destination_fingerprint: row.get(1)?,
        destination_kind: row.get(2)?,
        requirement: row.get(3)?,
        state: row.get(4)?,
        immutability_capability: row.get(5)?,
        retention_until_ms: row.get(6)?,
        reader_status: row.get(7)?,
        observed_at_ms: row.get(8)?,
    })
}

fn bound_page<T: Serialize>(
    mut items: Vec<T>,
    limit: usize,
    cursor: impl Fn(&T) -> String,
) -> Result<(Vec<T>, Option<String>), RepositoryError> {
    let database_has_more = items.len() > limit;
    if database_has_more {
        items.truncate(limit);
    }
    let mut encoded_bytes = 2_048usize;
    let mut keep = 0usize;
    for item in &items {
        let item_bytes = serde_json::to_vec(item)
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?
            .len();
        if encoded_bytes.saturating_add(item_bytes).saturating_add(1)
            > MAX_SNAPSHOT_DETAIL_PAGE_BYTES
        {
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
    let next_cursor = (database_has_more || byte_has_more)
        .then(|| items.last().map(&cursor))
        .flatten();
    Ok((items, next_cursor))
}

fn validate_page_input(
    workspace_id: &str,
    recovery_point_id: &str,
    cursor: Option<&str>,
    limit: usize,
) -> Result<(), RepositoryError> {
    if !valid_uuid(workspace_id)
        || !valid_uuid(recovery_point_id)
        || limit == 0
        || limit > MAX_SNAPSHOT_DETAIL_PAGE
        || cursor.is_some_and(|value| {
            value.is_empty() || value.len() > MAX_CURSOR_BYTES || value.contains(['\0', '\r', '\n'])
        })
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok(())
}

fn parse_time_cursor(value: &str) -> Result<(i64, String), RepositoryError> {
    let (time, id) = value
        .split_once(':')
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let time = time
        .parse::<i64>()
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if time < 0 || !valid_uuid(id) {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok((time, id.to_owned()))
}

fn valid_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn fetch_limit(limit: usize) -> Result<i64, RepositoryError> {
    i64::try_from(limit.saturating_add(1)).map_err(|_| RepositoryError::RuntimeStateInvalid)
}
