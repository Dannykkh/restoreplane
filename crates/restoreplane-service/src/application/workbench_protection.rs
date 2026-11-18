//! Optional metadata guards for existing Scope commands; never user SQL.
use super::*;
use restoreplane_application::canonical_payload_digest;
use restoreplane_catalog::rusqlite::{params, OptionalExtension};
use restoreplane_catalog::WorkbenchBindingRepository;
use restoreplane_domain::workbench::{Engine as WorkbenchEngine, Scope};
use restoreplane_domain::workbench_protection::{
    WorkbenchSelection, WorkbenchSelectionProof, MAX_SELECTION_BYTES,
};

pub(crate) const MAX_PROOF_AGE_MS: i64 = 30_000;
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SelectionAuthority {
    pub owner_sid_hash: String,
    pub scope: Scope,
}

pub(crate) fn selection_digest(
    authority: &SelectionAuthority,
    selection: &WorkbenchSelection,
) -> Result<String, ApplicationError> {
    canonical_payload_digest(
        &json!({"signatureVersion":1,"ownerSidHash":authority.owner_sid_hash,"scope":authority.scope,"descriptorRevision":selection.expected_descriptor_revision,"items":selection.canonical_items()}),
    )
}
impl ServiceApplication {
    pub(crate) fn workbench_descriptor_revision(value: Value) -> Result<String, ApplicationError> {
        super::workbench::descriptor_revision(value)
    }
    pub(super) fn selection_authority(
        &self,
        owner: Option<&str>,
        workspace: &WorkspaceId,
        target: &DatabaseTargetId,
        selection: &WorkbenchSelection,
    ) -> Result<SelectionAuthority, ApplicationError> {
        let owner = owner.ok_or(ApplicationError::Forbidden)?;
        if !selection.is_well_formed()
            || serde_json::to_vec(selection)
                .map_err(|_| ApplicationError::InvalidPayload)?
                .len()
                > MAX_SELECTION_BYTES
        {
            return Err(ApplicationError::InvalidPayload);
        }
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        let target_record = store
            .configuration()
            .selected_database_target(target)
            .map_err(query_error)?;
        let scope = Scope {
            workspace_id: workspace.clone(),
            connection_profile_id: target_record.connection_profile_id().clone(),
            database_target_id: target.clone(),
        };
        let observed = store
            .workbench_binding()
            .snapshot(&scope, owner)
            .map_err(|_| ApplicationError::WorkbenchBindingUnproven)?;
        if observed.descriptor.engine == WorkbenchEngine::Sqlite {
            return Err(ApplicationError::WorkbenchBindingUnproven);
        }
        if Self::workbench_descriptor_revision(observed.revision_input)?
            != selection.expected_descriptor_revision
        {
            return Err(ApplicationError::WorkbenchBindingStale);
        }
        Ok(SelectionAuthority {
            owner_sid_hash: owner.into(),
            scope,
        })
    }
}

/// Called inside the existing create_explicit IMMEDIATE transaction.
#[derive(Clone)]
pub(super) struct SelectionSaveGuard {
    pub owner: String,
    pub workspace: WorkspaceId,
    pub target: DatabaseTargetId,
    pub discovery: OperationRunId,
    pub expected: String,
    pub selected_ids: Vec<String>,
    pub now: i64,
}
pub(super) fn validate_selection_save(
    tx: &restoreplane_catalog::rusqlite::Transaction<'_>,
    guard: &SelectionSaveGuard,
) -> Result<(), RepositoryError> {
    let owner = guard.owner.as_str();
    let workspace = &guard.workspace;
    let target = &guard.target;
    let discovery = &guard.discovery;
    let expected = guard.expected.as_str();
    let selected_ids = &guard.selected_ids;
    let now = guard.now;
    let invalid = || RepositoryError::BackupScopeDiscoveryInvalid;
    let (request, result, latest): (String,String,String) = tx.query_row(
        "SELECT request_json,result_json,(SELECT newest.id FROM operation_runs newest WHERE newest.operation_kind='connection_discovery' AND newest.subject_type='database_target' AND newest.subject_id=?2 AND newest.state='succeeded' AND json_extract(newest.result_json,'$.kind')='backup_scope_object_discovery' AND json_extract(newest.result_json,'$.workbenchSelection.selectionDigest')=?3 ORDER BY CAST(json_extract(newest.result_json,'$.observedAtMs') AS INTEGER) DESC,newest.id DESC LIMIT 1) FROM operation_runs WHERE id=?1 AND operation_kind='connection_discovery' AND subject_type='database_target' AND subject_id=?2 AND state='succeeded'",
        params![discovery.as_str(),target.as_str(),expected], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?.ok_or_else(invalid)?;
    if latest != discovery.as_str() || request.len() > 1024 * 1024 || result.len() > 1024 * 1024 {
        return Err(invalid());
    }
    let request: Value = serde_json::from_str(&request).map_err(|_| invalid())?;
    let result: Value = serde_json::from_str(&result).map_err(|_| invalid())?;
    let authority: SelectionAuthority =
        serde_json::from_value(request["workbenchAuthority"].clone()).map_err(|_| invalid())?;
    let selection: WorkbenchSelection =
        serde_json::from_value(request["workbenchSelection"].clone()).map_err(|_| invalid())?;
    let proof: WorkbenchSelectionProof =
        serde_json::from_value(result["workbenchSelection"].clone()).map_err(|_| invalid())?;
    let verified = i64::try_from(proof.verified_at_ms.value()).map_err(|_| invalid())?;
    if !selection.is_well_formed()
        || !proof.is_well_formed()
        || now < 0
        || verified > now
        || now.saturating_sub(verified) > MAX_PROOF_AGE_MS
        || authority.owner_sid_hash != owner
        || authority.scope.workspace_id != *workspace
        || authority.scope.database_target_id != *target
        || proof.selection_digest != expected
        || proof.descriptor_revision != selection.expected_descriptor_revision
        || proof.items != selection.canonical_items()
        || selection_digest(&authority, &selection).map_err(|_| invalid())? != expected
        || request["inventoryCoverage"] != "selected"
        || result["inventoryCoverage"] != "selected"
    {
        return Err(invalid());
    }
    let mut ids = selected_ids.to_vec();
    ids.sort();
    let mut expected_ids = proof
        .items
        .iter()
        .map(|item| item.engine_object_id.clone())
        .collect::<Vec<_>>();
    expected_ids.sort();
    if ids != expected_ids {
        return Err(invalid());
    }
    let observed = WorkbenchBindingRepository::snapshot_in(tx, &authority.scope, owner)?;
    if ServiceApplication::workbench_descriptor_revision(observed.revision_input)
        .map_err(|_| invalid())?
        != proof.descriptor_revision
    {
        return Err(invalid());
    }
    Ok(())
}

pub(super) fn record_scope_link(
    tx: &restoreplane_catalog::rusqlite::Transaction<'_>,
    guard: &SelectionSaveGuard,
    scope: &BackupScopeId,
    fingerprint: &str,
    selector_hash: &str,
) -> Result<(), RepositoryError> {
    let invalid = || RepositoryError::BackupScopeDiscoveryInvalid;
    if scope.as_str() != guard.discovery.as_str() {
        return Err(invalid());
    }
    let raw:String=tx.query_row("SELECT result_json FROM operation_runs WHERE id=?1 AND subject_type='database_target' AND subject_id=?2 AND state='succeeded'",params![guard.discovery.as_str(),guard.target.as_str()],|row|row.get(0))?;
    let mut result: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
    let expected = json!({"scopeId":scope.as_str(),"scopeFingerprint":fingerprint,"selectorHash":selector_hash,"selectionDigest":guard.expected,"linkedAtMs":guard.now.to_string()});
    if result.get("workbenchScopeLinks").is_none() {
        result["workbenchScopeLinks"] = json!([]);
    }
    let links = result["workbenchScopeLinks"]
        .as_array_mut()
        .ok_or_else(invalid)?;
    if let Some(link) = links.iter().find(|link| link["scopeId"] == scope.as_str()) {
        return if link == &expected {
            Ok(())
        } else {
            Err(invalid())
        };
    }
    if links.len() >= 100 {
        return Err(RepositoryError::BackupScopeInputInvalid);
    }
    links.push(expected);
    let encoded = serde_json::to_string(&result).map_err(|_| invalid())?;
    if encoded.len() > 1024 * 1024 {
        return Err(RepositoryError::BackupScopeInputInvalid);
    }
    if tx.execute("UPDATE operation_runs SET result_json=?2,updated_at_ms=MAX(updated_at_ms,?3) WHERE id=?1 AND subject_type='database_target' AND subject_id=?4 AND state='succeeded'",params![guard.discovery.as_str(),encoded,guard.now,guard.target.as_str()])?!=1 { return Err(invalid()); }
    Ok(())
}
