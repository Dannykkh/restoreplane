use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use restoreplane_config::{CanonicalSecretBinding, CanonicalV1ShadowModel, ImportedSecretRef};
use restoreplane_domain::{
    BackupPlanId, ConnectionProfileId, DatabaseTargetId, DestinationId, PlanRevisionId, SecretRefId,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::OperationalStore;

use super::RepositoryError;

const SHADOW_NAMESPACE: Uuid = Uuid::from_u128(0x8e98_ea76_43b1_5e27_9e7c_617f_4b5a_43da);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowImportReport {
    created: bool,
    import_run_id: String,
    diff_json: String,
    diff: ShadowDiff,
}

impl ShadowImportReport {
    pub const fn created(&self) -> bool {
        self.created
    }

    pub fn import_run_id(&self) -> &str {
        &self.import_run_id
    }

    pub fn diff_json(&self) -> &str {
        &self.diff_json
    }

    pub fn relation_digest_blake3(&self) -> &str {
        &self.diff.relation_digest_blake3
    }

    pub const fn connection_count(&self) -> usize {
        self.diff.imported_rows.connections
    }

    pub const fn target_count(&self) -> usize {
        self.diff.imported_rows.targets
    }

    pub const fn destination_count(&self) -> usize {
        self.diff.imported_rows.destinations
    }

    pub const fn plan_count(&self) -> usize {
        self.diff.imported_rows.plans
    }

    pub const fn secret_ref_count(&self) -> usize {
        self.diff.imported_rows.secret_refs
    }
}

#[derive(Clone)]
pub struct ShadowImportRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn shadow_import(&self) -> ShadowImportRepository {
        ShadowImportRepository {
            store: self.clone(),
        }
    }
}

impl ShadowImportRepository {
    pub fn import_v1(
        &self,
        model: CanonicalV1ShadowModel,
        imported_at_ms: i64,
    ) -> Result<ShadowImportReport, RepositoryError> {
        validate_model(&model, imported_at_ms)?;
        let source_model_digest = model
            .relation_fingerprint_blake3()
            .map_err(|_| RepositoryError::ShadowImportInputInvalid)?;
        self.write(move |connection| {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let import_id = stable_import_id(&model.source_checksum_blake3);
            if let Some(diff_json) = transaction
                .query_row(
                    "SELECT diff_json
                     FROM config_import_runs
                     WHERE id = ?1
                       AND source_kind = 'legacy_toml'
                       AND mode = 'shadow'
                       AND status = 'succeeded'",
                    [import_id.as_str()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                let diff = decode_diff(&diff_json)?;
                if diff.source_model_digest_blake3 != source_model_digest {
                    return Err(RepositoryError::ShadowImportConflict);
                }
                transaction.commit()?;
                return Ok(ShadowImportReport {
                    created: false,
                    import_run_id: import_id,
                    diff_json,
                    diff,
                });
            }

            ensure_isolated_shadow(&transaction)?;
            transaction.execute(
                "INSERT INTO config_import_runs(
                     id, source_kind, source_checksum_blake3, mode, status,
                     diff_json, started_at_ms, completed_at_ms, cutover_marker,
                     confirmation_token_id, error_code, created_at_ms
                 ) VALUES (
                     ?1, 'legacy_toml', ?2, 'shadow', 'running',
                     '{}', ?3, NULL, NULL,
                     NULL, NULL, ?3
                 )",
                params![import_id, model.source_checksum_blake3, imported_at_ms],
            )?;

            let ids = import_relations(&transaction, &model, imported_at_ms)?;
            let imported_rows = imported_row_counts(&transaction)?;
            let relation_digest_blake3 = relation_digest(&transaction)?;
            let diff = build_diff(
                &model,
                &ids,
                source_model_digest,
                relation_digest_blake3,
                imported_rows,
            );
            let diff_json = serde_json::to_string(&diff)
                .map_err(|_| RepositoryError::ShadowImportInputInvalid)?;
            let updated = transaction.execute(
                "UPDATE config_import_runs
                 SET status = 'succeeded', diff_json = ?2, completed_at_ms = ?3
                 WHERE id = ?1 AND status = 'running'",
                params![import_id, diff_json, imported_at_ms],
            )?;
            if updated != 1 {
                return Err(RepositoryError::ShadowImportConflict);
            }
            transaction.commit()?;
            Ok(ShadowImportReport {
                created: true,
                import_run_id: import_id,
                diff_json,
                diff,
            })
        })
    }

    pub fn import_run_count(&self) -> Result<i64, RepositoryError> {
        let reader = self.store.open_reader()?;
        reader
            .connection()
            .query_row(
                "SELECT COUNT(*)
                 FROM config_import_runs
                 WHERE source_kind = 'legacy_toml'
                   AND mode = 'shadow'
                   AND status = 'succeeded'",
                [],
                |row| row.get(0),
            )
            .map_err(RepositoryError::from)
    }

    fn write<F, T>(&self, job: F) -> Result<T, RepositoryError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<T, RepositoryError> + Send + 'static,
        T: Send + 'static,
    {
        self.store.run_write(job).map_err(RepositoryError::from)?
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShadowDiff {
    source_checksum_blake3: String,
    source_model_digest_blake3: String,
    relation_digest_blake3: String,
    source_rows: SourceRows,
    imported_rows: ImportedRows,
    skipped_items: Vec<String>,
    ambiguous_items: Vec<String>,
    next_due: Vec<NextDue>,
    secret_bindings: Vec<SafeSecretBinding>,
    execution_authority: String,
    cutover_performed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceRows {
    connection_candidates: usize,
    legacy_databases: usize,
    destinations: usize,
    jobs: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportedRows {
    connections: usize,
    targets: usize,
    destinations: usize,
    plans: usize,
    revisions: usize,
    target_memberships: usize,
    destination_memberships: usize,
    secret_refs: usize,
    secret_bindings: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NextDue {
    plan_id: String,
    schedule_expression: String,
    next_due_at_ms: Option<i64>,
    status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SafeSecretBinding {
    owner_kind: String,
    owner_id: String,
    role: String,
    secret_ref_id: String,
}

#[derive(Default)]
struct StableIds {
    connections: BTreeMap<String, ConnectionProfileId>,
    targets: BTreeMap<String, DatabaseTargetId>,
    destinations: BTreeMap<String, DestinationId>,
    plans: BTreeMap<String, BackupPlanId>,
    revisions: BTreeMap<String, PlanRevisionId>,
}

fn validate_model(
    model: &CanonicalV1ShadowModel,
    imported_at_ms: i64,
) -> Result<(), RepositoryError> {
    if imported_at_ms < 0
        || !is_hex_digest(&model.source_checksum_blake3)
        || model.connections.is_empty()
        || model.targets.is_empty()
        || model.destinations.is_empty()
        || model.plans.is_empty()
    {
        return Err(RepositoryError::ShadowImportInputInvalid);
    }

    let connection_keys = unique_keys(model.connections.iter().map(|item| item.key.as_str()))?;
    let target_keys = unique_keys(model.targets.iter().map(|item| item.key.as_str()))?;
    let destination_keys = unique_keys(model.destinations.iter().map(|item| item.key.as_str()))?;
    unique_keys(model.plans.iter().map(|item| item.key.as_str()))?;
    unique_keys(model.plans.iter().map(|item| item.revision_key.as_str()))?;

    for connection in &model.connections {
        if connection.display_name.is_empty()
            || connection.host.is_empty()
            || connection.user.is_empty()
            || connection.source_resource_key.is_empty()
            || connection.port == 0
        {
            return Err(RepositoryError::ShadowImportInputInvalid);
        }
    }
    for target in &model.targets {
        if !connection_keys.contains(target.connection_key.as_str())
            || target.engine_object_id.is_empty()
            || target.display_name.is_empty()
            || target.normalized_name.is_empty()
            || !is_hex_digest(&target.source_fingerprint)
        {
            return Err(RepositoryError::ShadowImportInputInvalid);
        }
    }
    for destination in &model.destinations {
        if destination.display_name.is_empty()
            || !json_is_valid(&destination.endpoint_json)
            || !json_is_valid(&destination.declared_capabilities_json)
        {
            return Err(RepositoryError::ShadowImportInputInvalid);
        }
    }
    for plan in &model.plans {
        if plan.display_name.is_empty()
            || plan.schedule_expression.is_empty()
            || !target_keys.contains(plan.target_key.as_str())
            || plan.destination_keys.is_empty()
            || !is_hex_digest(&plan.content_hash_blake3)
            || !json_is_valid(&plan.retention_policy_json)
            || !json_is_valid(&plan.verification_policy_json)
            || !json_is_valid(&plan.execution_policy_json)
        {
            return Err(RepositoryError::ShadowImportInputInvalid);
        }
        let plan_destinations = unique_keys(plan.destination_keys.iter().map(String::as_str))?;
        if !plan_destinations.is_subset(&destination_keys) {
            return Err(RepositoryError::ShadowImportInputInvalid);
        }
    }

    let secret_ids = unique_keys(model.secret_refs.iter().map(|item| item.id.as_str()))?;
    for secret in &model.secret_refs {
        validate_secret_ref(secret)?;
    }
    for binding in &model.secret_bindings {
        validate_binding(binding, &connection_keys, &destination_keys, &secret_ids)?;
    }
    Ok(())
}

fn validate_secret_ref(secret: &ImportedSecretRef) -> Result<(), RepositoryError> {
    SecretRefId::from_str(&secret.id)?;
    if !matches!(secret.provider.as_str(), "windows_dpapi" | "external")
        || secret.provider_locator.is_empty()
        || secret.purpose.is_empty()
        || secret.version <= 0
        || !is_hex_digest(&secret.fingerprint)
        || secret.provider_locator.contains("enc:1:")
    {
        return Err(RepositoryError::ShadowImportInputInvalid);
    }
    Ok(())
}

fn validate_binding(
    binding: &CanonicalSecretBinding,
    connection_keys: &BTreeSet<&str>,
    destination_keys: &BTreeSet<&str>,
    secret_ids: &BTreeSet<&str>,
) -> Result<(), RepositoryError> {
    let valid_owner = match binding.owner_kind.as_str() {
        "connection" => {
            connection_keys.contains(binding.owner_key.as_str())
                && matches!(
                    binding.role.as_str(),
                    "database_auth" | "tls_client_key" | "tls_ca" | "tunnel_auth"
                )
        }
        "destination" => {
            destination_keys.contains(binding.owner_key.as_str())
                && matches!(
                    binding.role.as_str(),
                    "backup_writer" | "restore_reader" | "delete_operator" | "host_auth"
                )
        }
        _ => false,
    };
    if !valid_owner || !secret_ids.contains(binding.secret_ref_id.as_str()) {
        return Err(RepositoryError::ShadowImportInputInvalid);
    }
    Ok(())
}

fn unique_keys<'a>(
    values: impl Iterator<Item = &'a str>,
) -> Result<BTreeSet<&'a str>, RepositoryError> {
    let mut keys = BTreeSet::new();
    for value in values {
        if value.is_empty() || !keys.insert(value) {
            return Err(RepositoryError::ShadowImportInputInvalid);
        }
    }
    Ok(keys)
}

fn json_is_valid(value: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(value).is_ok()
}

fn is_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn stable_import_id(source_checksum: &str) -> String {
    Uuid::new_v5(
        &SHADOW_NAMESPACE,
        format!("legacy_toml|shadow|{source_checksum}").as_bytes(),
    )
    .to_string()
}

pub(super) fn verify_shadow_import_for_cutover(
    connection: &Connection,
    import_run_id: &str,
    source_checksum_blake3: &str,
    relation_digest_blake3: &str,
) -> Result<(), RepositoryError> {
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT source_checksum_blake3, diff_json
             FROM config_import_runs
             WHERE id = ?1
               AND source_kind = 'legacy_toml'
               AND mode = 'shadow'
               AND status = 'succeeded'
               AND completed_at_ms IS NOT NULL
               AND cutover_marker IS NULL
               AND confirmation_token_id IS NULL",
            [import_run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_source_checksum, diff_json)) = row else {
        return Err(RepositoryError::CutoverPreconditionFailed);
    };
    let diff = decode_diff(&diff_json).map_err(|_| RepositoryError::CutoverPreconditionFailed)?;
    if stored_source_checksum != source_checksum_blake3
        || diff.source_checksum_blake3 != source_checksum_blake3
        || diff.relation_digest_blake3 != relation_digest_blake3
        || diff.execution_authority != "legacy_direct"
        || diff.cutover_performed
        || !diff.skipped_items.is_empty()
        || !diff.ambiguous_items.is_empty()
        || diff.imported_rows.connections == 0
        || diff.imported_rows.targets == 0
        || diff.imported_rows.destinations == 0
        || diff.imported_rows.plans == 0
    {
        return Err(RepositoryError::CutoverPreconditionFailed);
    }
    Ok(())
}

fn ensure_isolated_shadow(transaction: &Transaction<'_>) -> Result<(), RepositoryError> {
    let row_count: i64 = transaction.query_row(
        "SELECT
             (SELECT COUNT(*) FROM config_import_runs)
           + (SELECT COUNT(*) FROM connection_profiles)
           + (SELECT COUNT(*) FROM database_targets)
           + (SELECT COUNT(*) FROM destinations)
           + (SELECT COUNT(*) FROM backup_plans)
           + (SELECT COUNT(*) FROM backup_plan_revisions)
           + (SELECT COUNT(*) FROM secret_refs)",
        [],
        |row| row.get(0),
    )?;
    if row_count != 0 {
        return Err(RepositoryError::ShadowImportConflict);
    }
    Ok(())
}

fn import_relations(
    transaction: &Transaction<'_>,
    model: &CanonicalV1ShadowModel,
    imported_at_ms: i64,
) -> Result<StableIds, RepositoryError> {
    let mut ids = StableIds::default();

    for secret in &model.secret_refs {
        transaction.execute(
            "INSERT INTO secret_refs(
                 id, provider, provider_locator, purpose, version, state,
                 fingerprint, rotated_from_id, created_at_ms, retired_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, NULL, ?7, NULL)",
            params![
                secret.id,
                secret.provider,
                secret.provider_locator,
                secret.purpose,
                secret.version,
                secret.fingerprint,
                imported_at_ms
            ],
        )?;
    }

    for connection in &model.connections {
        let id = ConnectionProfileId::from_stable_name(SHADOW_NAMESPACE, connection.key.as_bytes());
        transaction.execute(
            "INSERT INTO connection_profiles(
                 id, display_name, engine, transport, host, port, file_path,
                 database_hint, tls_mode, tunnel_config_json, adapter_config_json,
                 state, last_probe_status, last_probe_at_ms, source_resource_key,
                 deleted_at_ms, created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, 'tcp', ?4, ?5, NULL,
                 NULL, 'disabled', NULL, ?6,
                 'draft', NULL, NULL, ?7,
                 NULL, ?8, ?8
             )",
            params![
                id.as_str(),
                connection.display_name,
                connection.engine,
                connection.host,
                connection.port,
                json!({ "legacyUser": connection.user }).to_string(),
                connection.source_resource_key,
                imported_at_ms
            ],
        )?;
        ids.connections.insert(connection.key.clone(), id);
    }

    for target in &model.targets {
        let id = DatabaseTargetId::from_stable_name(SHADOW_NAMESPACE, target.key.as_bytes());
        let connection_id = ids
            .connections
            .get(&target.connection_key)
            .ok_or(RepositoryError::ShadowImportInputInvalid)?;
        transaction.execute(
            "INSERT INTO database_targets(
                 id, connection_profile_id, engine_object_id, display_name,
                 normalized_name, source_fingerprint, state, missing_since_ms,
                 last_seen_discovery_run_id, selected_at_ms, retired_at_ms,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 ?5, ?6, 'active', NULL,
                 NULL, ?7, NULL,
                 ?7, ?7
             )",
            params![
                id.as_str(),
                connection_id.as_str(),
                target.engine_object_id,
                target.display_name,
                target.normalized_name,
                target.source_fingerprint,
                imported_at_ms
            ],
        )?;
        ids.targets.insert(target.key.clone(), id);
    }

    for destination in &model.destinations {
        let id = DestinationId::from_stable_name(SHADOW_NAMESPACE, destination.key.as_bytes());
        transaction.execute(
            "INSERT INTO destinations(
                 id, display_name, kind, endpoint_json, state,
                 declared_capabilities_json, observed_capabilities_json,
                 immutability_capability, last_probe_status, last_probe_at_ms,
                 deleted_at_ms, created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, 'draft',
                 ?5, '{}',
                 ?6, NULL, NULL,
                 NULL, ?7, ?7
             )",
            params![
                id.as_str(),
                destination.display_name,
                destination.kind,
                destination.endpoint_json,
                destination.declared_capabilities_json,
                destination.immutability_capability,
                imported_at_ms
            ],
        )?;
        ids.destinations.insert(destination.key.clone(), id);
    }

    import_secret_bindings(transaction, &model.secret_bindings, &ids, imported_at_ms)?;

    for plan in &model.plans {
        let plan_id = BackupPlanId::from_stable_name(SHADOW_NAMESPACE, plan.key.as_bytes());
        let revision_id =
            PlanRevisionId::from_stable_name(SHADOW_NAMESPACE, plan.revision_key.as_bytes());
        transaction.execute(
            "INSERT INTO backup_plans(
                 id, display_name, state, current_revision_id,
                 deleted_at_ms, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'draft', NULL, NULL, ?3, ?3)",
            params![plan_id.as_str(), plan.display_name, imported_at_ms],
        )?;
        transaction.execute(
            "INSERT INTO backup_plan_revisions(
                 id, backup_plan_id, revision_no, schedule_kind,
                 schedule_expression, timezone, catchup_policy, overlap_policy,
                 rpo_ms, retention_policy_json, verification_policy_json,
                 execution_policy_json, content_hash_blake3, published_at_ms,
                 created_at_ms
             ) VALUES (
                 ?1, ?2, 1, 'cron',
                 ?3, 'UTC', 'skip', 'coalesce_one',
                 NULL, ?4, ?5,
                 ?6, ?7, NULL,
                 ?8
             )",
            params![
                revision_id.as_str(),
                plan_id.as_str(),
                plan.schedule_expression,
                plan.retention_policy_json,
                plan.verification_policy_json,
                plan.execution_policy_json,
                plan.content_hash_blake3,
                imported_at_ms
            ],
        )?;
        let target_id = ids
            .targets
            .get(&plan.target_key)
            .ok_or(RepositoryError::ShadowImportInputInvalid)?;
        transaction.execute(
            "INSERT INTO plan_revision_targets(
                 plan_revision_id, database_target_id, target_options_json, created_at_ms
             ) VALUES (?1, ?2, '{}', ?3)",
            params![revision_id.as_str(), target_id.as_str(), imported_at_ms],
        )?;
        for destination_key in &plan.destination_keys {
            let destination_id = ids
                .destinations
                .get(destination_key)
                .ok_or(RepositoryError::ShadowImportInputInvalid)?;
            transaction.execute(
                "INSERT INTO plan_revision_destinations(
                     plan_revision_id, destination_id, requirement,
                     retention_override_json, created_at_ms
                 ) VALUES (?1, ?2, 'required', NULL, ?3)",
                params![
                    revision_id.as_str(),
                    destination_id.as_str(),
                    imported_at_ms
                ],
            )?;
        }
        transaction.execute(
            "UPDATE backup_plan_revisions
             SET published_at_ms = ?2
             WHERE id = ?1 AND published_at_ms IS NULL",
            params![revision_id.as_str(), imported_at_ms],
        )?;
        let plan_state = if plan.enabled { "enabled" } else { "paused" };
        transaction.execute(
            "UPDATE backup_plans
             SET state = ?2, current_revision_id = ?3, updated_at_ms = ?4
             WHERE id = ?1",
            params![
                plan_id.as_str(),
                plan_state,
                revision_id.as_str(),
                imported_at_ms
            ],
        )?;
        ids.plans.insert(plan.key.clone(), plan_id);
        ids.revisions.insert(plan.revision_key.clone(), revision_id);
    }

    Ok(ids)
}

fn import_secret_bindings(
    transaction: &Transaction<'_>,
    bindings: &[CanonicalSecretBinding],
    ids: &StableIds,
    imported_at_ms: i64,
) -> Result<(), RepositoryError> {
    for binding in bindings {
        match binding.owner_kind.as_str() {
            "connection" => {
                let owner_id = ids
                    .connections
                    .get(&binding.owner_key)
                    .ok_or(RepositoryError::ShadowImportInputInvalid)?;
                transaction.execute(
                    "INSERT INTO connection_secret_bindings(
                         connection_profile_id, secret_role, secret_ref_id, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        owner_id.as_str(),
                        binding.role,
                        binding.secret_ref_id,
                        imported_at_ms
                    ],
                )?;
            }
            "destination" => {
                let owner_id = ids
                    .destinations
                    .get(&binding.owner_key)
                    .ok_or(RepositoryError::ShadowImportInputInvalid)?;
                transaction.execute(
                    "INSERT INTO destination_secret_bindings(
                         destination_id, secret_role, secret_ref_id, created_at_ms
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        owner_id.as_str(),
                        binding.role,
                        binding.secret_ref_id,
                        imported_at_ms
                    ],
                )?;
            }
            _ => return Err(RepositoryError::ShadowImportInputInvalid),
        }
    }
    Ok(())
}

fn imported_row_counts(transaction: &Transaction<'_>) -> Result<ImportedRows, RepositoryError> {
    transaction
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM connection_profiles),
                 (SELECT COUNT(*) FROM database_targets),
                 (SELECT COUNT(*) FROM destinations),
                 (SELECT COUNT(*) FROM backup_plans),
                 (SELECT COUNT(*) FROM backup_plan_revisions),
                 (SELECT COUNT(*) FROM plan_revision_targets),
                 (SELECT COUNT(*) FROM plan_revision_destinations),
                 (SELECT COUNT(*) FROM secret_refs),
                 (SELECT COUNT(*) FROM connection_secret_bindings)
                   + (SELECT COUNT(*) FROM destination_secret_bindings)",
            [],
            |row| {
                Ok(ImportedRows {
                    connections: checked_count(row, 0)?,
                    targets: checked_count(row, 1)?,
                    destinations: checked_count(row, 2)?,
                    plans: checked_count(row, 3)?,
                    revisions: checked_count(row, 4)?,
                    target_memberships: checked_count(row, 5)?,
                    destination_memberships: checked_count(row, 6)?,
                    secret_refs: checked_count(row, 7)?,
                    secret_bindings: checked_count(row, 8)?,
                })
            },
        )
        .map_err(RepositoryError::from)
}

fn checked_count(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<usize> {
    let value: i64 = row.get(index)?;
    usize::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn relation_digest(transaction: &Transaction<'_>) -> Result<String, RepositoryError> {
    let mut statement = transaction.prepare(
        "SELECT row_json
         FROM (
             SELECT json_array(
                 'secret_ref', id, provider, provider_locator, purpose, version, state, fingerprint
             ) AS row_json FROM secret_refs
             UNION ALL
             SELECT json_array(
                 'connection', id, display_name, engine, transport, host, port,
                 tls_mode, adapter_config_json, state, source_resource_key
             ) FROM connection_profiles
             UNION ALL
             SELECT json_array(
                 'target', id, connection_profile_id, engine_object_id,
                 display_name, normalized_name, source_fingerprint, state
             ) FROM database_targets
             UNION ALL
             SELECT json_array(
                 'destination', id, display_name, kind, endpoint_json, state,
                 declared_capabilities_json, observed_capabilities_json,
                 immutability_capability
             ) FROM destinations
             UNION ALL
             SELECT json_array(
                 'connection_secret', connection_profile_id, secret_role, secret_ref_id
             ) FROM connection_secret_bindings
             UNION ALL
             SELECT json_array(
                 'destination_secret', destination_id, secret_role, secret_ref_id
             ) FROM destination_secret_bindings
             UNION ALL
             SELECT json_array(
                 'plan', id, display_name, state, current_revision_id
             ) FROM backup_plans
             UNION ALL
             SELECT json_array(
                 'revision', id, backup_plan_id, revision_no, schedule_kind,
                 schedule_expression, timezone, catchup_policy, overlap_policy,
                 retention_policy_json, verification_policy_json,
                 execution_policy_json, content_hash_blake3, published_at_ms IS NOT NULL
             ) FROM backup_plan_revisions
             UNION ALL
             SELECT json_array(
                 'target_membership', plan_revision_id, database_target_id, target_options_json
             ) FROM plan_revision_targets
             UNION ALL
             SELECT json_array(
                 'destination_membership', plan_revision_id, destination_id,
                 requirement, retention_override_json
             ) FROM plan_revision_destinations
         )
         ORDER BY row_json",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut hasher = blake3::Hasher::new();
    for row in rows {
        let row = row?;
        hasher.update(&(row.len() as u64).to_le_bytes());
        hasher.update(row.as_bytes());
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn build_diff(
    model: &CanonicalV1ShadowModel,
    ids: &StableIds,
    source_model_digest_blake3: String,
    relation_digest_blake3: String,
    imported_rows: ImportedRows,
) -> ShadowDiff {
    let next_due = model
        .plans
        .iter()
        .map(|plan| NextDue {
            plan_id: ids.plans[&plan.key].to_string(),
            schedule_expression: plan.schedule_expression.clone(),
            next_due_at_ms: None,
            status: "not_computed_shadow".to_owned(),
        })
        .collect();
    let secret_bindings = model
        .secret_bindings
        .iter()
        .map(|binding| SafeSecretBinding {
            owner_kind: binding.owner_kind.clone(),
            owner_id: match binding.owner_kind.as_str() {
                "connection" => ids.connections[&binding.owner_key].to_string(),
                "destination" => ids.destinations[&binding.owner_key].to_string(),
                _ => unreachable!("validated shadow owner kind"),
            },
            role: binding.role.clone(),
            secret_ref_id: binding.secret_ref_id.clone(),
        })
        .collect();
    ShadowDiff {
        source_checksum_blake3: model.source_checksum_blake3.clone(),
        source_model_digest_blake3,
        relation_digest_blake3,
        source_rows: SourceRows {
            connection_candidates: model.connections.len(),
            legacy_databases: model.targets.len(),
            destinations: model.destinations.len(),
            jobs: model.plans.len(),
        },
        imported_rows,
        skipped_items: Vec::new(),
        ambiguous_items: Vec::new(),
        next_due,
        secret_bindings,
        execution_authority: "legacy_direct".to_owned(),
        cutover_performed: false,
    }
}

fn decode_diff(diff_json: &str) -> Result<ShadowDiff, RepositoryError> {
    serde_json::from_str(diff_json).map_err(|_| RepositoryError::ShadowImportConflict)
}
