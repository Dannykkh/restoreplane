use dbbackup_domain::{WorkItemId, WorkspaceId};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

use crate::{OperationalStore, DEFAULT_WORKSPACE_ID};

use super::RepositoryError;

#[derive(Clone, Debug)]
pub struct WorkspaceSpec {
    id: WorkspaceId,
    display_name: String,
    created_at_ms: i64,
}

impl WorkspaceSpec {
    pub fn new(id: WorkspaceId, display_name: impl Into<String>, created_at_ms: i64) -> Self {
        Self {
            id,
            display_name: display_name.into(),
            created_at_ms,
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        let display_name = self.display_name.trim();
        if self.created_at_ms < 0
            || display_name.is_empty()
            || display_name.chars().count() > 96
            || display_name.chars().any(char::is_control)
        {
            return Err(RepositoryError::WorkspaceInputInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub id: String,
    pub display_name: String,
    pub state: String,
    pub is_default: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub connection_count: i64,
    pub database_target_count: i64,
    pub destination_count: i64,
    pub plan_count: i64,
    pub active_work_count: i64,
    pub latest_activity_at_ms: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspacePage {
    pub items: Vec<WorkspaceRecord>,
    pub next_cursor: Option<String>,
    pub total_count: i64,
    pub complete: bool,
}

#[derive(Clone)]
pub struct WorkspaceRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn workspace(&self) -> WorkspaceRepository {
        WorkspaceRepository {
            store: self.clone(),
        }
    }
}

impl WorkspaceRepository {
    pub fn create(&self, spec: WorkspaceSpec) -> Result<WorkspaceRecord, RepositoryError> {
        spec.validate()?;
        let workspace_id = spec.id.as_str().to_owned();
        let display_name = spec.display_name.trim().to_owned();
        let normalized_name = normalize_name(&display_name);
        let created_at_ms = spec.created_at_ms;
        self.store
            .run_write(move |connection| {
                let transaction = connection.transaction()?;
                let duplicate: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM workspaces WHERE normalized_name = ?1)",
                    [&normalized_name],
                    |row| row.get(0),
                )?;
                if duplicate {
                    return Err(RepositoryError::WorkspaceNameConflict);
                }
                transaction.execute(
                    "INSERT INTO workspaces(
                         id, display_name, normalized_name, state, is_default,
                         created_at_ms, updated_at_ms
                     ) VALUES (?1, ?2, ?3, 'active', 0, ?4, ?4)",
                    params![workspace_id, display_name, normalized_name, created_at_ms],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .map_err(RepositoryError::from)??;
        self.get(spec.id.as_str())
    }

    pub fn list(&self) -> Result<Vec<WorkspaceRecord>, RepositoryError> {
        let reader = self.store.open_reader()?;
        let mut statement = reader.connection().prepare(&format!(
            "{WORKSPACE_SELECT} ORDER BY workspace.is_default DESC, workspace.normalized_name ASC"
        ))?;
        let rows = statement.query_map([], workspace_from_row)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(RepositoryError::from)
    }

    pub fn page(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<WorkspacePage, RepositoryError> {
        if limit == 0 || limit > 100 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let cursor = cursor.map(parse_workspace_cursor).transpose()?;
        let (cursor_created_at_ms, cursor_id) = cursor
            .as_ref()
            .map(|(created_at_ms, id)| (Some(*created_at_ms), Some(id.as_str())))
            .unwrap_or((None, None));
        let reader = self.store.open_reader()?;
        let connection = reader.connection();
        let total_count =
            connection.query_row("SELECT COUNT(*) FROM workspaces", [], |row| row.get(0))?;
        let mut statement = connection.prepare(&format!(
            "{WORKSPACE_SELECT}
             WHERE (?1 IS NULL OR workspace.created_at_ms > ?1
                    OR (workspace.created_at_ms = ?1 AND workspace.id > ?2))
             ORDER BY workspace.created_at_ms, workspace.id
             LIMIT ?3"
        ))?;
        let fetch_limit = i64::try_from(limit.saturating_add(1))
            .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
        let mut items = statement
            .query_map(
                params![cursor_created_at_ms, cursor_id, fetch_limit],
                workspace_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = items.len() > limit;
        if has_more {
            items.truncate(limit);
        }
        let next_cursor = has_more.then(|| {
            let item = items.last().expect("lookahead implies a non-empty page");
            format!("{}:{}", item.created_at_ms, item.id)
        });
        Ok(WorkspacePage {
            complete: next_cursor.is_none(),
            items,
            next_cursor,
            total_count,
        })
    }

    pub fn get(&self, workspace_id: &str) -> Result<WorkspaceRecord, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                &format!("{WORKSPACE_SELECT} WHERE workspace.id = ?1"),
                [workspace_id],
                workspace_from_row,
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)
    }

    pub fn require_active(&self, workspace_id: &WorkspaceId) -> Result<(), RepositoryError> {
        let reader = self.store.open_reader()?;
        let state = reader
            .connection()
            .query_row(
                "SELECT state FROM workspaces WHERE id = ?1",
                [workspace_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        match state.as_deref() {
            Some("active") => Ok(()),
            Some(_) => Err(RepositoryError::WorkspaceScopeMismatch),
            None => Err(RepositoryError::NotFound),
        }
    }

    pub fn require_connection(
        &self,
        workspace_id: &WorkspaceId,
        connection_id: &str,
    ) -> Result<(), RepositoryError> {
        self.require_owned_resource("connection", workspace_id.as_str(), connection_id)
    }

    pub fn require_destination(
        &self,
        workspace_id: &WorkspaceId,
        destination_id: &str,
    ) -> Result<(), RepositoryError> {
        self.require_owned_resource("destination", workspace_id.as_str(), destination_id)
    }

    pub fn require_database_target(
        &self,
        workspace_id: &WorkspaceId,
        database_target_id: &str,
    ) -> Result<(), RepositoryError> {
        let reader = self.store.open_reader()?;
        let owned: bool = reader.connection().query_row(
            "SELECT EXISTS(
                 SELECT 1
                 FROM database_targets AS target
                 JOIN workspace_connection_profiles AS ownership
                   ON ownership.connection_profile_id = target.connection_profile_id
                 WHERE ownership.workspace_id = ?1 AND target.id = ?2
             )",
            rusqlite::params![workspace_id.as_str(), database_target_id],
            |row| row.get(0),
        )?;
        if owned {
            Ok(())
        } else {
            Err(RepositoryError::WorkspaceScopeMismatch)
        }
    }

    pub fn require_plan(
        &self,
        workspace_id: &WorkspaceId,
        plan_id: &str,
    ) -> Result<(), RepositoryError> {
        self.require_owned_resource("plan", workspace_id.as_str(), plan_id)
    }

    pub fn require_recovery_point(
        &self,
        workspace_id: &WorkspaceId,
        recovery_point_id: &str,
    ) -> Result<(), RepositoryError> {
        self.require_owned_resource("recovery_point", workspace_id.as_str(), recovery_point_id)
    }

    pub fn require_work_item(
        &self,
        workspace_id: &WorkspaceId,
        work_item_id: &str,
    ) -> Result<(), RepositoryError> {
        self.require_owned_resource("work_item", workspace_id.as_str(), work_item_id)
    }

    pub fn for_work_item(
        &self,
        work_item_id: &WorkItemId,
    ) -> Result<WorkspaceRecord, RepositoryError> {
        let reader = self.store.open_reader()?;
        let workspace_id = reader
            .connection()
            .query_row(
                "SELECT workspace_id FROM workspace_work_items WHERE work_item_id = ?1",
                [work_item_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or(RepositoryError::NotFound)?;
        self.get(&workspace_id)
    }

    pub fn default_id(&self) -> &'static str {
        DEFAULT_WORKSPACE_ID
    }

    fn require_owned_resource(
        &self,
        resource_kind: &str,
        workspace_id: &str,
        resource_id: &str,
    ) -> Result<(), RepositoryError> {
        let sql = match resource_kind {
            "connection" => {
                "SELECT EXISTS(
                     SELECT 1 FROM workspace_connection_profiles
                     WHERE workspace_id = ?1 AND connection_profile_id = ?2
                 )"
            }
            "destination" => {
                "SELECT EXISTS(
                     SELECT 1 FROM workspace_destinations
                     WHERE workspace_id = ?1 AND destination_id = ?2
                 )"
            }
            "plan" => {
                "SELECT EXISTS(
                     SELECT 1 FROM workspace_backup_plans
                     WHERE workspace_id = ?1 AND backup_plan_id = ?2
                 )"
            }
            "recovery_point" => {
                "SELECT EXISTS(
                     SELECT 1 FROM workspace_recovery_points
                     WHERE workspace_id = ?1 AND recovery_point_id = ?2
                 )"
            }
            "work_item" => {
                "SELECT EXISTS(
                     SELECT 1 FROM workspace_work_items
                     WHERE workspace_id = ?1 AND work_item_id = ?2
                 )"
            }
            _ => return Err(RepositoryError::WorkspaceInputInvalid),
        };
        let reader = self.store.open_reader()?;
        let exists: bool =
            reader
                .connection()
                .query_row(sql, params![workspace_id, resource_id], |row| row.get(0))?;
        if exists {
            Ok(())
        } else {
            Err(RepositoryError::NotFound)
        }
    }
}

const WORKSPACE_SELECT: &str = "SELECT workspace.id,
            workspace.display_name,
            workspace.state,
            workspace.is_default,
            workspace.created_at_ms,
            workspace.updated_at_ms,
            (SELECT COUNT(*) FROM workspace_connection_profiles AS ownership
             WHERE ownership.workspace_id = workspace.id),
            (SELECT COUNT(*)
             FROM database_targets AS target
             JOIN workspace_connection_profiles AS ownership
               ON ownership.connection_profile_id = target.connection_profile_id
             WHERE ownership.workspace_id = workspace.id),
            (SELECT COUNT(*) FROM workspace_destinations AS ownership
             WHERE ownership.workspace_id = workspace.id),
            (SELECT COUNT(*) FROM workspace_backup_plans AS ownership
             WHERE ownership.workspace_id = workspace.id),
            (SELECT COUNT(*)
             FROM work_items AS work
             JOIN workspace_work_items AS ownership ON ownership.work_item_id = work.id
             WHERE ownership.workspace_id = workspace.id
               AND work.state IN (
                   'queued', 'delayed', 'leased', 'starting', 'running',
                   'committing', 'cancel_requested'
               )),
            (SELECT MAX(event.occurred_at_ms)
             FROM activity_events AS event
             JOIN workspace_activity_events AS ownership
               ON ownership.activity_event_seq = event.seq
             WHERE ownership.workspace_id = workspace.id)
     FROM workspaces AS workspace";

fn workspace_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        id: row.get(0)?,
        display_name: row.get(1)?,
        state: row.get(2)?,
        is_default: row.get::<_, i64>(3)? == 1,
        created_at_ms: row.get(4)?,
        updated_at_ms: row.get(5)?,
        connection_count: row.get(6)?,
        database_target_count: row.get(7)?,
        destination_count: row.get(8)?,
        plan_count: row.get(9)?,
        active_work_count: row.get(10)?,
        latest_activity_at_ms: row.get(11)?,
    })
}

fn normalize_name(value: &str) -> String {
    value.trim().to_lowercase()
}

fn parse_workspace_cursor(value: &str) -> Result<(i64, String), RepositoryError> {
    if value.is_empty() || value.len() > 256 || value.contains(['\0', '\r', '\n']) {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    let (created_at_ms, id) = value
        .split_once(':')
        .ok_or(RepositoryError::RuntimeStateInvalid)?;
    let created_at_ms = created_at_ms
        .parse::<i64>()
        .map_err(|_| RepositoryError::RuntimeStateInvalid)?;
    if created_at_ms < 0 || !uuid::Uuid::parse_str(id).is_ok_and(|parsed| parsed.to_string() == id)
    {
        return Err(RepositoryError::RuntimeStateInvalid);
    }
    Ok((created_at_ms, id.to_owned()))
}
