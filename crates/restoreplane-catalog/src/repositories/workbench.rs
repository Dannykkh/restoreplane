//! One read transaction observes Workspace ownership, connection identity and conflicts.

use super::{server_identity_from_capabilities, RepositoryError};
use crate::OperationalStore;
use restoreplane_domain::workbench::{
    ConnectDescriptor, DatabaseIdentity, Engine, RootTrust, Route, Scope, TargetConflict, Tls,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone)]
pub struct WorkbenchBindingRepository {
    store: OperationalStore,
}

pub struct WorkbenchBindingSnapshot {
    pub descriptor: ConnectDescriptor,
    pub revision_input: Value,
    pub observed_conflicts: Vec<TargetConflict>,
}

impl OperationalStore {
    pub fn workbench_binding(&self) -> WorkbenchBindingRepository {
        WorkbenchBindingRepository {
            store: self.clone(),
        }
    }
}

impl WorkbenchBindingRepository {
    pub fn snapshot(
        &self,
        scope: &Scope,
        owner_sid_hash: &str,
    ) -> Result<WorkbenchBindingSnapshot, RepositoryError> {
        let reader = self.store.open_reader()?;
        let tx = reader.connection().unchecked_transaction()?;
        let snapshot = Self::snapshot_in(&tx, scope, owner_sid_hash)?;
        tx.commit()?;
        Ok(snapshot)
    }

    /// Reuses an existing local admission transaction; it performs fixed
    /// metadata reads only and never opens a remote database or secret.
    pub fn snapshot_in(
        tx: &rusqlite::Connection,
        scope: &Scope,
        owner_sid_hash: &str,
    ) -> Result<WorkbenchBindingSnapshot, RepositoryError> {
        if owner_sid_hash.len() != 64
            || !owner_sid_hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let raw = tx.query_row(
            "SELECT json_object(
              'workspaceState',w.state,'profileState',p.state,'deletedAtMs',CAST(p.deleted_at_ms AS TEXT),
              'engine',p.engine,'transport',p.transport,'host',p.host,'port',CAST(p.port AS TEXT),'filePath',p.file_path,
              'tlsMode',p.tls_mode,'adapterConfig',json(p.adapter_config_json),'tunnelConfig',json(p.tunnel_config_json),
              'sourceResourceKey',p.source_resource_key,'databaseHint',p.database_hint,
              'profileUpdatedAtMs',CAST(p.updated_at_ms AS TEXT),'ownershipCreatedAtMs',CAST(o.created_at_ms AS TEXT),
              'targetState',t.state,'targetUpdatedAtMs',CAST(t.updated_at_ms AS TEXT),'retiredAtMs',CAST(t.retired_at_ms AS TEXT),
              'engineObjectId',t.engine_object_id,'databaseName',t.display_name,'databaseSearchName',t.normalized_name,'sourceFingerprint',t.source_fingerprint,
              'secretRefId',s.id,'secretPurpose',s.purpose,'secretState',s.state,'secretVersion',CAST(s.version AS TEXT),'secretFingerprint',s.fingerprint,
              'probeCapabilities',(SELECT r.observed_capabilities_json FROM connection_probe_runs r WHERE r.connection_profile_id=p.id AND r.status='passed' ORDER BY r.completed_at_ms DESC,r.id DESC LIMIT 1))
             FROM workspaces w JOIN workspace_connection_profiles o ON o.workspace_id=w.id
             JOIN connection_profiles p ON p.id=o.connection_profile_id
             JOIN database_targets t ON t.connection_profile_id=p.id
             LEFT JOIN connection_secret_bindings b ON b.connection_profile_id=p.id AND b.secret_role='database_auth'
             LEFT JOIN secret_refs s ON s.id=b.secret_ref_id
             WHERE w.id=?1 AND p.id=?2 AND t.id=?3",
            params![scope.workspace_id.as_str(),scope.connection_profile_id.as_str(),scope.database_target_id.as_str()],
            |row| row.get::<_,String>(0),
        ).optional()?.ok_or(RepositoryError::WorkspaceScopeMismatch)?;
        let mut revision_input: Value =
            serde_json::from_str(&raw).map_err(|_| RepositoryError::SecurityInputInvalid)?;
        if revision_input["workspaceState"] != "active"
            || revision_input["profileState"] == "disabled"
            || !revision_input["deletedAtMs"].is_null()
            || revision_input["targetState"] != "active"
            || !revision_input["retiredAtMs"].is_null()
        {
            return Err(RepositoryError::WorkspaceScopeMismatch);
        }
        if !revision_input["secretRefId"].is_null()
            && (revision_input["secretPurpose"] != "database_auth"
                || revision_input["secretState"] != "active")
        {
            return Err(RepositoryError::SecurityInputInvalid);
        }
        let adapter = revision_input["adapterConfig"]
            .as_object()
            .ok_or(RepositoryError::WorkbenchBindingUnproven)?;
        if adapter
            .iter()
            .any(|(key, value)| key != "restoreTestTarget" || !value.is_boolean())
            || !revision_input["tunnelConfig"].is_null()
        {
            return Err(RepositoryError::WorkbenchBindingUnproven);
        }
        let engine = match revision_input["engine"].as_str() {
            Some("postgresql") => Engine::Postgresql,
            Some("mysql") => Engine::Mysql,
            Some("mariadb") => Engine::Mariadb,
            Some("sqlite") => Engine::Sqlite,
            _ => return Err(RepositoryError::WorkbenchBindingUnproven),
        };
        let route = match revision_input["transport"].as_str() {
            Some("tcp") if engine != Engine::Sqlite => {
                let host = text(&revision_input, "host")?;
                let port = text(&revision_input, "port")?
                    .parse::<u16>()
                    .map_err(|_| RepositoryError::WorkbenchBindingUnproven)?;
                if host.is_empty()
                    || host.len() > 1024
                    || host.chars().any(char::is_control)
                    || port == 0
                {
                    return Err(RepositoryError::WorkbenchBindingUnproven);
                }
                Route::Tcp { host, port }
            }
            Some("file") if engine == Engine::Sqlite => Route::File {
                canonical_path: text(&revision_input, "filePath")?,
            },
            _ => return Err(RepositoryError::WorkbenchBindingUnproven),
        };
        let mode = match revision_input["tlsMode"].as_str() {
            Some("disabled") => "disable",
            Some("preferred") => "prefer",
            Some("required") => "require",
            Some("verify_ca") => "verify-ca",
            Some("verify_full") => "verify-full",
            _ => return Err(RepositoryError::WorkbenchBindingUnproven),
        };
        let tls = match &route {
            Route::Tcp { host, .. } => Tls {
                mode: mode.into(),
                server_name: Some(host.clone()),
                root_trust: Some(RootTrust::System),
            },
            Route::File { .. } => Tls {
                mode: "disable".into(),
                server_name: None,
                root_trust: None,
            },
        };
        let expected_server_identity = revision_input["probeCapabilities"]
            .as_str()
            .and_then(|v| server_identity_from_capabilities(engine.as_str(), v));
        let descriptor = ConnectDescriptor {
            engine,
            route,
            database: DatabaseIdentity {
                engine_object_id: text(&revision_input, "engineObjectId")?,
                name: text(&revision_input, "databaseName")?,
                source_fingerprint: text(&revision_input, "sourceFingerprint")?,
            },
            tls,
            transport_options: BTreeMap::new(),
            expected_server_identity,
        };
        let mut statement = tx.prepare("SELECT 'backup',id,state FROM backup_runs WHERE database_target_id=?1 AND state IN('created','queued','running','committing') UNION ALL SELECT 'restore',id,state FROM restore_runs WHERE (target_database_target_id=?1 OR (target_connection_profile_id=?2 AND target_database_name=?3)) AND state IN('created','queued','running','validating','cleanup_required') UNION ALL SELECT 'maintenance',id,state FROM operation_runs WHERE operation_kind IN('control_store_restore','config_cutover','service_drain') AND state IN('created','queued','running') ORDER BY 1,2 LIMIT 33")?;
        let observed_conflicts = statement
            .query_map(
                params![
                    scope.database_target_id.as_str(),
                    scope.connection_profile_id.as_str(),
                    descriptor.database.name
                ],
                |row| {
                    Ok(TargetConflict {
                        kind: row.get(0)?,
                        run_id: row.get(1)?,
                        state: row.get(2)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        if observed_conflicts.len() > 32 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        drop(statement);
        revision_input["scope"] =
            serde_json::to_value(scope).map_err(|_| RepositoryError::SecurityInputInvalid)?;
        revision_input["ownerSidHash"] = json!(owner_sid_hash);
        revision_input["schemaTag"] = json!("restoreplane.workbench.binding.v1");
        revision_input["descriptor"] =
            serde_json::to_value(&descriptor).map_err(|_| RepositoryError::SecurityInputInvalid)?;
        Ok(WorkbenchBindingSnapshot {
            descriptor,
            revision_input,
            observed_conflicts,
        })
    }
}

fn text(value: &Value, key: &str) -> Result<String, RepositoryError> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .ok_or(RepositoryError::WorkbenchBindingUnproven)
}
