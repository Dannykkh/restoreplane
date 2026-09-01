use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use restoreplane_application::{
    ApplicationCommand, ApplicationError, ApplicationRequest, ApplicationResponse,
    ApplicationUseCases, CutoverMode, ExecutionAuthority,
};
use restoreplane_catalog::{
    ActivityEventSpec, ConnectionProfileSpec, ConsoleSnapshot, ConsoleSnapshotQuery,
    ControlBackupSpec, DestinationSecretBindingSpec, DestinationSecretRoleV2, DestinationSpec,
    ExplicitBackupScopeSpec, OperationWorkSpec, OperationalStore,
    PersistedCutoverRecoveryDisposition, PlanDraftSpec, RepositoryError, RestorePurpose,
    SafeRestoreSpec, UnsupportedVerificationSpec, VerificationWorkSpec, WorkspaceSpec,
    DEFAULT_WORKSPACE_ID,
};
use restoreplane_domain::{
    BackupPlanId, BackupScopeId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, DomainError, OperationRunId, PlanRevisionId, RecoveryPointId,
    RestoreRunId, VerificationRunId, WorkItemId, WorkspaceId,
};
use restoreplane_pipeline::{RetentionDispositionV2, RetentionReasonV2};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zeroize::Zeroize;

use crate::backup::ServiceBackupWorkerHandle;
use crate::host::{HostPhase, ServiceHealthProbe};
use crate::operations::ServiceOperationWorkerHandle;
use crate::repair::ServiceCatalogRebuildPreviewer;
use crate::secrets::{SecretBuffer, SecretPurpose, ServiceSecretStore};

pub struct ServiceApplication {
    health: ServiceHealthProbe,
    store: Option<OperationalStore>,
    secrets: Option<Arc<ServiceSecretStore>>,
    _backup_worker: Option<Arc<ServiceBackupWorkerHandle>>,
    _operation_worker: Option<Arc<ServiceOperationWorkerHandle>>,
    catalog_rebuild_previewer: Option<Arc<ServiceCatalogRebuildPreviewer>>,
    maintenance_root: Option<PathBuf>,
    clock: fn() -> i64,
}

impl ServiceApplication {
    pub const fn new(health: ServiceHealthProbe, store: Option<OperationalStore>) -> Self {
        Self {
            health,
            store,
            secrets: None,
            _backup_worker: None,
            _operation_worker: None,
            catalog_rebuild_previewer: None,
            maintenance_root: None,
            clock: unix_time_ms,
        }
    }

    pub const fn with_clock(
        health: ServiceHealthProbe,
        store: Option<OperationalStore>,
        clock: fn() -> i64,
    ) -> Self {
        Self {
            health,
            store,
            secrets: None,
            _backup_worker: None,
            _operation_worker: None,
            catalog_rebuild_previewer: None,
            maintenance_root: None,
            clock,
        }
    }

    pub fn with_secret_store(mut self, secrets: Arc<ServiceSecretStore>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    pub fn with_operation_worker(
        mut self,
        operation_worker: Arc<ServiceOperationWorkerHandle>,
    ) -> Self {
        self._operation_worker = Some(operation_worker);
        self
    }

    pub fn with_backup_worker(mut self, backup_worker: Arc<ServiceBackupWorkerHandle>) -> Self {
        self._backup_worker = Some(backup_worker);
        self
    }

    pub fn with_catalog_rebuild_previewer(
        mut self,
        previewer: Arc<ServiceCatalogRebuildPreviewer>,
    ) -> Self {
        self.catalog_rebuild_previewer = Some(previewer);
        self
    }

    pub fn with_maintenance_root(mut self, maintenance_root: PathBuf) -> Self {
        self.maintenance_root = Some(maintenance_root);
        self
    }

    fn service_health(&self) -> Result<ApplicationResponse, ApplicationError> {
        let health = self.health.snapshot();
        let (scheduler, retention) = match &self.store {
            Some(store) => (
                serde_json::to_value(
                    store
                        .console_query()
                        .scheduler_health()
                        .map_err(query_error)?,
                )
                .map_err(|_| ApplicationError::Internal)?,
                serde_json::to_value(
                    store
                        .console_query()
                        .retention_health()
                        .map_err(query_error)?,
                )
                .map_err(|_| ApplicationError::Internal)?,
            ),
            None => (
                json!({
                    "state": "unknown",
                    "configuredPlanCount": 0,
                    "observedCursorCount": 0,
                    "activeWorkCount": 0,
                    "errorPlanCount": 0,
                    "lastTickAtMs": null,
                    "lastMaterializedForMs": null,
                    "lastErrorCode": null,
                    "lastErrorAtMs": null,
                    "evidenceAtMs": null,
                }),
                json!({
                    "state": "unknown",
                    "configuredPolicyCount": 0,
                    "activeTombstoneCount": 0,
                    "queuedDeleteCount": 0,
                    "failedOperationCount": 0,
                    "lastScanAtMs": null,
                    "lastErrorCode": null,
                    "lastErrorAtMs": null,
                    "evidenceAtMs": null,
                }),
            ),
        };
        Ok(response(json!({
            "bootId": health.boot_id().to_string(),
            "phase": phase_name(health.phase()),
            "schemaVersion": health.schema_version(),
            "applicationTableCount": health.application_table_count(),
            "executionAuthority": authority_name(health.execution_authority()),
            "cutoverMode": cutover_name(health.cutover_mode()),
            "cutoverMarker": health.cutover_marker(),
            "recoveryDisposition": health.recovery_disposition().map(recovery_disposition_name),
            "currentBinaryVersion": health.current_binary_version(),
            "minimumReaderVersion": health.minimum_reader_version(),
            "updatePending": health.update_pending(),
            "claimsEnabled": health.claims_enabled(),
            "acceptsIpc": health.accepts_ipc(),
            "diagnosticCode": health.diagnostic_code(),
            "scheduler": scheduler,
            "retention": retention,
        })))
    }

    fn schema_status(&self) -> Result<ApplicationResponse, ApplicationError> {
        let health = self.health.snapshot();
        let body = if let Some(store) = &self.store {
            let diagnostics = store
                .writer_diagnostics()
                .map_err(|_| ApplicationError::Internal)?;
            json!({
                "userVersion": diagnostics.user_version,
                "applicationTableCount": diagnostics.application_table_count,
                "journalMode": diagnostics.journal_mode.to_ascii_lowercase(),
                "synchronous": synchronous_name(diagnostics.synchronous),
                "foreignKeys": diagnostics.foreign_keys == 1,
                "busyTimeoutMs": diagnostics.busy_timeout_ms,
                "diagnosticCode": health.diagnostic_code(),
            })
        } else {
            json!({
                "userVersion": health.schema_version(),
                "applicationTableCount": health.application_table_count(),
                "journalMode": Value::Null,
                "synchronous": Value::Null,
                "foreignKeys": Value::Null,
                "busyTimeoutMs": Value::Null,
                "diagnosticCode": health.diagnostic_code(),
            })
        };
        Ok(response(body))
    }

    fn workspace_list(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: WorkspaceListPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspaces = self
            .store
            .as_ref()
            .ok_or(ApplicationError::Internal)?
            .workspace()
            .page(input.cursor.as_deref(), input.limit)
            .map_err(query_error)?;
        serialize_response(workspaces)
    }

    fn workspace_create(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: WorkspaceCreatePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        if input.display_name.trim().is_empty()
            || input.display_name.chars().count() > 96
            || input.display_name.chars().any(char::is_control)
        {
            return Err(ApplicationError::InvalidPayload);
        }
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let workspace = self
            .store
            .as_ref()
            .ok_or(ApplicationError::Internal)?
            .workspace()
            .create(WorkspaceSpec::new(
                WorkspaceId::new(),
                input.display_name.trim(),
                now_ms,
            ))
            .map_err(query_error)?;
        serialize_response(workspace)
    }

    fn service_diagnostics(&self) -> Result<ApplicationResponse, ApplicationError> {
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let diagnostics = self
            .store
            .as_ref()
            .ok_or(ApplicationError::Internal)?
            .diagnostics()
            .collect(now_ms)
            .map_err(query_error)?;
        let body = serde_json::to_value(diagnostics).map_err(|_| ApplicationError::Internal)?;
        Ok(response(body))
    }

    fn retention_dry_run(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: RetentionDryRunPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        if input.destination_fingerprint.trim().is_empty()
            || input.destination_fingerprint.len() > 512
            || input.destination_fingerprint.split_once(':').is_none()
            || input.grace_period_ms <= 0
        {
            return Err(ApplicationError::InvalidPayload);
        }
        let recovery_point_id = RecoveryPointId::from_str(&input.recovery_point_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_recovery_point(&workspace_id, recovery_point_id.as_str())
            .map_err(query_error)?;
        let plan = store
            .retention_planner()
            .prepare_delete_plan(
                &recovery_point_id,
                &input.destination_fingerprint,
                now_ms,
                input.grace_period_ms,
            )
            .map_err(|_| ApplicationError::Internal)?;
        let authorization = plan
            .preview_json()
            .map_err(|_| ApplicationError::Internal)?;
        let authorization = serde_json::from_str::<Value>(&authorization)
            .map_err(|_| ApplicationError::Internal)?;
        Ok(response(json!({
            "schemaVersion": 1,
            "recoveryPointId": recovery_point_id.as_str(),
            "destinationFingerprint": input.destination_fingerprint,
            "evaluatedAtMs": now_ms,
            "disposition": disposition_name(plan.decision().disposition),
            "reasons": plan
                .decision()
                .reasons
                .iter()
                .copied()
                .map(reason_name)
                .collect::<Vec<_>>(),
            "eligibleAfterMs": plan.decision().eligible_after_ms,
            "wouldBreakCompliance": plan.decision().would_break_compliance,
            "digestBlake3": plan.digest_blake3(),
            "authorization": authorization,
        })))
    }

    fn connection_list(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input = decode_page(payload)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let page = self
            .console_query()?
            .connections_in_workspace_at(
                input.workspace_id.as_str(),
                input.cursor.as_deref(),
                input.limit,
                now_ms,
            )
            .map_err(query_error)?;
        serialize_response(page)
    }

    fn target_list(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: TargetListPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let page = self
            .console_query()?
            .database_targets_in_workspace_at(
                &input.workspace_id,
                input.cursor.as_deref(),
                input.limit,
                input.connection_id.as_deref(),
                input.state.as_deref(),
                input.search.as_deref(),
                now_ms,
            )
            .map_err(query_error)?;
        serialize_response(page)
    }

    fn destination_list(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input = decode_page(payload)?;
        let page = self
            .console_query()?
            .destinations_in_workspace(
                input.workspace_id.as_str(),
                input.cursor.as_deref(),
                input.limit,
            )
            .map_err(query_error)?;
        serialize_response(page)
    }

    fn plan_list(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input = decode_page(payload)?;
        let page = self
            .console_query()?
            .plans_in_workspace(
                input.workspace_id.as_str(),
                input.cursor.as_deref(),
                input.limit,
            )
            .map_err(query_error)?;
        serialize_response(page)
    }

    fn engine_matrix(&self) -> Result<ApplicationResponse, ApplicationError> {
        let now_ms = (self.clock)();
        let matrix = self
            .console_query()?
            .engine_matrix(now_ms)
            .map_err(query_error)?;
        serialize_response(matrix)
    }

    fn snapshot_list(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: SnapshotListPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let page = self
            .console_query()?
            .snapshots_in_workspace(
                &input.workspace_id,
                ConsoleSnapshotQuery {
                    cursor: input.cursor,
                    limit: input.limit,
                    scope: input.scope,
                    search: input.search,
                    connection_id: input.connection_id,
                    engine: input.engine,
                    from_ms: input.from_ms,
                    to_ms: input.to_ms,
                    artifact_completeness: input.artifact_completeness,
                    replica_durability: input.replica_durability,
                    protection_compliance: input.protection_compliance,
                    min_verification_level: input.min_verification_level,
                    restore_eligibility: input.restore_eligibility,
                    signature_status: input.signature_status,
                },
            )
            .map_err(query_error)?;
        serialize_response(page)
    }

    fn snapshot_get(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: SnapshotGetPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let recovery_point_id = RecoveryPointId::from_str(&input.snapshot_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        let snapshot =
            snapshot_summary_in_workspace(store, workspace_id.as_str(), &recovery_point_id)?;
        let page = store
            .snapshot_detail()
            .page_in_workspace(
                workspace_id.as_str(),
                recovery_point_id.as_str(),
                &input.section,
                input.cursor.as_deref(),
                input.limit,
            )
            .map_err(query_error)?;
        let mut body = serde_json::to_value(page).map_err(|_| ApplicationError::Internal)?;
        body["snapshot"] =
            serde_json::to_value(snapshot).map_err(|_| ApplicationError::Internal)?;
        Ok(response(body))
    }

    fn catalog_rebuild_preview(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        let input: CatalogRebuildPreviewPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let destination_id = DestinationId::from_str(&input.destination_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        self.store
            .as_ref()
            .ok_or(ApplicationError::Internal)?
            .workspace()
            .require_destination(&workspace_id, destination_id.as_str())
            .map_err(query_error)?;
        let observed_at_ms = (self.clock)();
        if observed_at_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let Some(previewer) = self.catalog_rebuild_previewer.as_ref() else {
            return Ok(response(json!({
                "schemaVersion": 1,
                "available": false,
                "destinationId": destination_id.as_str(),
                "reasonCode": "CATALOG_REBUILD_PREVIEW_NOT_CONFIGURED",
                "candidates": [],
            })));
        };
        match previewer.preview(&destination_id, input.job_filter.as_deref(), observed_at_ms) {
            Ok(preview) => {
                let mut body =
                    serde_json::to_value(preview).map_err(|_| ApplicationError::Internal)?;
                body["available"] = json!(true);
                body["reasonCode"] = Value::Null;
                Ok(response(body))
            }
            Err(error) => Ok(response(json!({
                "schemaVersion": 1,
                "available": false,
                "destinationId": destination_id.as_str(),
                "reasonCode": error.code(),
                "candidates": [],
            }))),
        }
    }

    fn verification_enqueue(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        let input: VerificationEnqueuePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        if input.purpose != "manual" || !(1..=3).contains(&input.level) {
            return Err(ApplicationError::InvalidPayload);
        }
        let recovery_point_id = RecoveryPointId::from_str(&input.snapshot_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_recovery_point(&workspace_id, recovery_point_id.as_str())
            .map_err(query_error)?;
        let snapshot =
            snapshot_summary_in_workspace(store, workspace_id.as_str(), &recovery_point_id)?;
        if snapshot.state != "complete"
            || snapshot.signature_status != "valid"
            || snapshot.artifact_completeness != "complete"
        {
            return Err(ApplicationError::InvalidPayload);
        }
        let source_replica = store
            .snapshot_detail()
            .eligible_replica_in_workspace(
                workspace_id.as_str(),
                recovery_point_id.as_str(),
                input.source_replica_id.as_deref(),
            )
            .map_err(|_| ApplicationError::InvalidPayload)?
            .id;
        let source_replica_id = source_replica.clone();
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let verification_run_id = VerificationRunId::new();
        let capability_name = match input.level {
            1 => "verify_l1",
            2 => "verify_l2",
            3 => "restore_l3",
            _ => return Err(ApplicationError::InvalidPayload),
        };
        let capability = store
            .certification()
            .effective_capability(&snapshot.engine, capability_name, now_ms)
            .map_err(query_error)?;
        let unsupported_reason = if input.level == 3 {
            Some("VERIFICATION_RESTORE_RUN_REQUIRED")
        } else if input.level == 2 && !matches!(snapshot.engine.as_str(), "postgresql" | "sqlite") {
            Some("VERIFY_L2_NATIVE_UNSUPPORTED")
        } else if capability.is_none() {
            Some("VERIFY_LEVEL_UNSUPPORTED")
        } else {
            None
        };
        let (work_item_id, state, error_code) = if let Some(reason) = unsupported_reason {
            store
                .verification()
                .record_unsupported(UnsupportedVerificationSpec::manual(
                    verification_run_id.clone(),
                    recovery_point_id.clone(),
                    Some(source_replica_id),
                    input.level,
                    reason,
                    now_ms,
                ))
                .map_err(query_error)?;
            (None, "unsupported", Some(reason))
        } else {
            let work_item_id = WorkItemId::new();
            store
                .verification()
                .enqueue(VerificationWorkSpec::manual(
                    verification_run_id.clone(),
                    work_item_id.clone(),
                    recovery_point_id.clone(),
                    Some(source_replica_id),
                    input.level,
                    now_ms,
                ))
                .map_err(query_error)?;
            (Some(work_item_id), "queued", None)
        };
        let sequence = store
            .activity()
            .append_batch(vec![ActivityEventSpec::info(
                uuid::Uuid::new_v4().to_string(),
                now_ms,
                if state == "queued" {
                    "VERIFICATION_QUEUED"
                } else {
                    "VERIFICATION_UNSUPPORTED"
                },
                "verification_run",
                verification_run_id.as_str(),
                if state == "queued" {
                    "The service accepted an exact-level manual verification request."
                } else {
                    "The requested manual verification level is explicitly unsupported."
                },
            )])
            .map_err(query_error)?
            .into_iter()
            .next()
            .ok_or(ApplicationError::Internal)?;
        Ok(response(json!({
            "verificationRunId": verification_run_id.as_str(),
            "workItemId": work_item_id.as_ref().map(WorkItemId::as_str),
            "state": state,
            "requestedLevel": input.level,
            "sourceReplicaId": source_replica,
            "supportStatus": capability.as_ref().map(|value| value.support_status()),
            "limitation": capability.as_ref().and_then(|value| value.limitation_text()),
            "errorCode": error_code,
            "activityAfterSeq": sequence,
        })))
    }

    fn restore_preflight(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: RestorePreflightPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        if !matches!(input.mode.as_str(), "create_new" | "overwrite")
            || !matches!(
                input.purpose.as_str(),
                "incident_recovery" | "restore_drill"
            )
            || (input.purpose == "restore_drill" && input.mode != "create_new")
            || !valid_restore_target_name(input.target_name.trim())
        {
            return Err(ApplicationError::InvalidPayload);
        }
        let recovery_point_id = RecoveryPointId::from_str(&input.snapshot_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let connection_id = ConnectionProfileId::from_str(&input.target_connection_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_recovery_point(&workspace_id, recovery_point_id.as_str())
            .map_err(query_error)?;
        store
            .workspace()
            .require_connection(&workspace_id, connection_id.as_str())
            .map_err(query_error)?;
        let snapshot =
            snapshot_summary_in_workspace(store, workspace_id.as_str(), &recovery_point_id)?;
        let source_replica = store
            .snapshot_detail()
            .eligible_replica_in_workspace(
                workspace_id.as_str(),
                recovery_point_id.as_str(),
                input.source_replica_id.as_deref(),
            )
            .map_err(|_| ApplicationError::InvalidPayload)?
            .id;
        let target_name = input.target_name.trim();
        let facts = store
            .console_query()
            .restore_target_facts(connection_id.as_str(), target_name)
            .map_err(query_error)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let restore_capability = store
            .certification()
            .effective_capability(&snapshot.engine, "restore_create_new", now_ms)
            .map_err(query_error)?;
        let validation_capability = store
            .certification()
            .effective_capability(&snapshot.engine, "validation", now_ms)
            .map_err(query_error)?;
        let trusted_tool = match restore_tool_for_engine(&snapshot.engine) {
            Some(expected) => store
                .certification()
                .has_trusted_tool(expected)
                .map_err(query_error)?,
            None => snapshot.engine == "sqlite",
        };
        let source_valid = snapshot.state == "complete"
            && snapshot.signature_status == "valid"
            && snapshot.artifact_completeness == "complete"
            && snapshot.restore_eligibility != "blocked";
        let source_state = if !source_valid {
            "failed"
        } else if snapshot.restore_eligibility == "conditional" {
            "warning"
        } else {
            "passed"
        };
        let tool_state = match (
            restore_capability.as_ref(),
            validation_capability.as_ref(),
            trusted_tool,
        ) {
            (Some(restore), Some(validation), true)
                if restore.support_status() == "passed"
                    && validation.support_status() == "passed" =>
            {
                "passed"
            }
            (Some(_), Some(_), true) => "warning",
            _ => "failed",
        };
        let required_spool_bytes = snapshot
            .plaintext_bytes
            .filter(|value| *value > 0)
            .and_then(|value| value.checked_mul(3));
        let space_passed = matches!(
            (required_spool_bytes, facts.available_spool_bytes),
            (Some(required), Some(available)) if available >= required
        );
        let probe_fresh = facts
            .probed_at_ms
            .is_some_and(|observed| observed <= now_ms && now_ms - observed <= 5 * 60 * 1_000);
        let connection_passed = facts.connection_state == "ready"
            && facts.probe_status.as_deref() == Some("passed")
            && probe_fresh;
        let server_passed = facts
            .server_version
            .as_deref()
            .is_some_and(|version| !version.trim().is_empty());
        let existence_passed = matches!(
            (input.mode.as_str(), facts.target_exists),
            ("create_new", Some(false)) | ("overwrite", Some(true))
        );
        let policy_passed = facts.engine == snapshot.engine
            && !is_protected_database_name(&snapshot.engine, target_name);
        let restore_target_role_passed =
            input.purpose != "restore_drill" || facts.restore_test_target;
        let destructive_identity_passed = if snapshot.engine == "sqlite" {
            true
        } else {
            input.mode == "create_new" && facts.server_identity.is_some()
        };
        let checks = vec![
            preflight_check(
                "source_snapshot",
                source_state,
                "Signed Recovery Point completeness and restore eligibility.",
                "Choose a complete Recovery Point with a valid signature.",
            ),
            preflight_check(
                "source_replica",
                "passed",
                "A committed reader-eligible Replica is selected.",
                "Probe or repair a restore-reader Replica.",
            ),
            preflight_check(
                "tool_and_validation",
                tool_state,
                "Current restore/validation certification and trusted tool inventory.",
                "Install, verify, and certify the required restore tool.",
            ),
            preflight_check(
                "space",
                if space_passed { "passed" } else { "failed" },
                "Observed ready-spool capacity for materialization and validation.",
                "Free space or register a ready spool volume.",
            ),
            preflight_check(
                "permission",
                if snapshot.engine == "sqlite" || facts.credential_bound {
                    "passed"
                } else {
                    "failed"
                },
                "Active database-auth Secret Ref binding.",
                "Store a target database credential and rerun preflight.",
            ),
            preflight_check(
                "connection",
                if connection_passed {
                    "passed"
                } else {
                    "failed"
                },
                "Recent service-owned target Connection probe.",
                "Run the target Connection probe again.",
            ),
            preflight_check(
                "server_version",
                if server_passed { "passed" } else { "failed" },
                "Observed target server identity and version.",
                "Probe with a certified adapter.",
            ),
            preflight_check(
                "destructive_target_identity",
                if destructive_identity_passed {
                    "passed"
                } else {
                    "failed"
                },
                if input.mode == "overwrite" && snapshot.engine != "sqlite" {
                    "Network overwrite remains disabled even when a strong live server identity is available."
                } else if snapshot.engine != "sqlite" && facts.server_identity.is_none() {
                    "The latest probe did not publish a strong immutable server identity."
                } else {
                    "The target has a strong destructive-operation identity binding."
                },
                "Run a current certified probe that observes the immutable server identity, or use SQLite's file-identity restore path.",
            ),
            preflight_check(
                "target_existence",
                if existence_passed { "passed" } else { "failed" },
                match (input.mode.as_str(), facts.target_exists) {
                    ("create_new", Some(false)) => "The requested new target is absent.",
                    ("create_new", Some(true)) => "The requested new target already exists.",
                    ("overwrite", Some(true)) => "The exact overwrite target exists.",
                    ("overwrite", Some(false)) => "The overwrite target does not exist.",
                    _ => "Target existence is unknown.",
                },
                "Run discovery or choose a name matching the restore mode.",
            ),
            preflight_check(
                "engine_policy",
                if policy_passed { "passed" } else { "failed" },
                "Same-engine mapping and protected-database policy.",
                "Choose a same-engine non-system target.",
            ),
            preflight_check(
                "restore_target_role",
                if restore_target_role_passed {
                    "passed"
                } else {
                    "failed"
                },
                if input.purpose == "restore_drill" {
                    "The Connection is registered as an isolated restore-test target."
                } else {
                    "No isolated restore-test target is required for incident recovery."
                },
                "Register a non-production Connection as a restore-test target.",
            ),
        ];
        let eligible = checks
            .iter()
            .all(|check| check["state"] == "passed" || check["state"] == "warning");
        let target_fingerprint = restore_target_fingerprint(
            connection_id.as_str(),
            &facts.engine,
            target_name,
            facts.server_version.as_deref(),
            facts.server_identity.as_deref(),
        );
        let preflight_version = blake3::hash(
            format!(
                "restore-preflight-v1\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
                recovery_point_id.as_str(),
                snapshot.last_ingested_at_ms,
                source_replica,
                target_fingerprint,
                input.mode,
                facts.probed_at_ms.unwrap_or(-1),
                facts.spool_observed_at_ms.unwrap_or(-1),
                facts.target_exists.map(i64::from).unwrap_or(-1),
                eligible
            )
            .as_bytes(),
        )
        .to_hex()
        .to_string();
        Ok(response(json!({
            "schemaVersion": 1,
            "preflightVersion": preflight_version,
            "evaluatedAtMs": now_ms,
            "expiresAtMs": now_ms.checked_add(5 * 60 * 1_000)
                .ok_or(ApplicationError::Internal)?,
            "eligible": eligible,
            "mode": input.mode,
            "purpose": input.purpose,
            "source": {
                "snapshotId": recovery_point_id.as_str(),
                "replicaId": source_replica,
                "displayName": snapshot.source_display_name,
                "engine": snapshot.engine,
                "restoreEligibility": snapshot.restore_eligibility,
            },
            "targetConnectionId": facts.connection_id,
            "targetConnectionName": facts.connection_name,
            "targetName": target_name,
            "targetFingerprint": target_fingerprint,
            "targetExists": facts.target_exists,
            "restoreTestTarget": facts.restore_test_target,
            "targetRole": if facts.restore_test_target { "restore_test" } else { "general" },
            "serverVersion": facts.server_version,
            "serverIdentity": facts.server_identity,
            "requiredSpoolBytes": required_spool_bytes,
            "availableSpoolBytes": facts.available_spool_bytes,
            "checks": checks,
        })))
    }

    fn restore_enqueue_safe(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        let input: SafeRestoreEnqueuePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        if !(1..=3).contains(&input.required_verification_level)
            || !matches!(
                input.purpose.as_str(),
                "incident_recovery" | "restore_drill"
            )
            || (input.purpose == "restore_drill" && input.required_verification_level != 3)
            || !is_digest(&input.preflight_version)
        {
            return Err(ApplicationError::InvalidPayload);
        }
        let recovery_point_id = RecoveryPointId::from_str(&input.snapshot_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let connection_id = ConnectionProfileId::from_str(&input.target_connection_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let purpose = match input.purpose.as_str() {
            "incident_recovery" => RestorePurpose::IncidentRecovery,
            "restore_drill" => RestorePurpose::RestoreDrill,
            _ => return Err(ApplicationError::InvalidPayload),
        };
        let mut prepared = self
            .restore_preflight(&json!({
                "workspaceId": workspace_id.as_str(),
                "snapshotId": recovery_point_id.as_str(),
                "mode": "create_new",
                "purpose": purpose.as_str(),
                "targetConnectionId": connection_id.as_str(),
                "targetName": input.target_name.as_str(),
                "sourceReplicaId": input.source_replica_id.as_str(),
            }))?
            .body;
        if prepared.get("eligible").and_then(Value::as_bool) != Some(true)
            || prepared.get("preflightVersion").and_then(Value::as_str)
                != Some(input.preflight_version.as_str())
            || prepared.get("purpose").and_then(Value::as_str) != Some(purpose.as_str())
        {
            return Err(ApplicationError::InvalidPayload);
        }
        let target_fingerprint = prepared
            .get("targetFingerprint")
            .and_then(Value::as_str)
            .ok_or(ApplicationError::Internal)?
            .to_owned();
        let source_replica_id = prepared
            .pointer("/source/replicaId")
            .and_then(Value::as_str)
            .ok_or(ApplicationError::Internal)?
            .to_owned();
        prepared["requiredVerificationLevel"] = json!(input.required_verification_level);
        let eligibility_snapshot_json =
            serde_json::to_string(&prepared).map_err(|_| ApplicationError::Internal)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let restore_run_id = RestoreRunId::new();
        let work_item_id = WorkItemId::new();
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        let result = store
            .recovery()
            .enqueue_safe(
                SafeRestoreSpec::new(
                    restore_run_id,
                    work_item_id,
                    recovery_point_id,
                    source_replica_id,
                    connection_id,
                    input.target_name.trim(),
                    target_fingerprint.clone(),
                    input.preflight_version.clone(),
                    eligibility_snapshot_json,
                    now_ms,
                )
                .with_purpose(purpose),
            )
            .map_err(query_error)?;
        let sequence = store
            .activity()
            .append_batch(vec![ActivityEventSpec::info(
                uuid::Uuid::new_v4().to_string(),
                now_ms,
                if purpose == RestorePurpose::RestoreDrill {
                    "RESTORE_DRILL_QUEUED"
                } else {
                    "RESTORE_CREATE_NEW_QUEUED"
                },
                "restore_run",
                result.restore_run_id().as_str(),
                if purpose == RestorePurpose::RestoreDrill {
                    "The service accepted an isolated restore drill after authoritative preflight."
                } else {
                    "The service accepted a create-new restore after authoritative preflight."
                },
            )])
            .map_err(query_error)?
            .into_iter()
            .next()
            .ok_or(ApplicationError::Internal)?;
        Ok(response(json!({
            "restoreRunId": result.restore_run_id().as_str(),
            "workItemId": result.work_item_id().as_str(),
            "state": "queued",
            "mode": "create_new",
            "purpose": purpose.as_str(),
            "verificationRunId": result.verification_run_id().map(|id| id.as_str()),
            "targetFingerprint": target_fingerprint,
            "preflightVersion": input.preflight_version,
            "activityAfterSeq": sequence,
        })))
    }

    fn activity_list(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: ActivityPagePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let page = self
            .console_query()?
            .activity_in_workspace(
                &input.workspace_id,
                input.cursor.as_deref(),
                input.limit,
                input.severity.as_deref(),
            )
            .map_err(query_error)?;
        serialize_response(page)
    }

    fn settings_read(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        validate_empty_payload(payload)?;
        let settings = self.console_query()?.settings().map_err(query_error)?;
        serialize_response(settings)
    }

    fn connection_save(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let mut input: ConnectionSavePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        input.validate()?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let connection_id = ConnectionProfileId::new();
        let spec = match input.transport.as_str() {
            "tcp" => ConnectionProfileSpec::tcp(
                connection_id.clone(),
                input.display_name.trim(),
                &input.engine,
                input.host.trim(),
                input.port,
                now_ms,
            )
            .with_tls_mode(&input.tls_mode)
            .with_restore_test_target(input.restore_test_target),
            "file" => ConnectionProfileSpec::file(
                connection_id.clone(),
                input.display_name.trim(),
                &input.engine,
                input.file_path.trim(),
                now_ms,
            )
            .with_restore_test_target(input.restore_test_target),
            _ => return Err(ApplicationError::InvalidPayload),
        };

        let secret_ref = if input.transport == "tcp" {
            let bytes = serde_json::to_vec(&DatabaseCredentialSecret {
                username: &input.username,
                password: &input.password,
            })
            .map_err(|_| ApplicationError::Internal)?;
            input.username.zeroize();
            input.password.zeroize();
            let secrets = self.secrets.as_ref().ok_or(ApplicationError::Internal)?;
            Some(
                secrets
                    .create(
                        SecretPurpose::DatabaseAuth,
                        SecretBuffer::new(bytes).map_err(|_| ApplicationError::InvalidPayload)?,
                        now_ms,
                    )
                    .map_err(|_| ApplicationError::Internal)?,
            )
        } else {
            input.username.zeroize();
            input.password.zeroize();
            None
        };

        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let insert = store
            .configuration()
            .insert_connection_with_database_auth_in_workspace(
                &workspace_id,
                spec,
                secret_ref.as_ref().map(|metadata| metadata.id().clone()),
            );
        if let Err(error) = insert {
            if let (Some(secrets), Some(metadata)) = (&self.secrets, secret_ref.as_ref()) {
                let _ = secrets.revoke(metadata.id(), now_ms);
            }
            return Err(query_error(error));
        }
        Ok(response(json!({
            "connectionId": connection_id.as_str(),
            "state": "draft",
        })))
    }

    fn target_select(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: TargetSelectPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let connection_id = ConnectionProfileId::from_str(&input.connection_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_connection(&workspace_id, connection_id.as_str())
            .map_err(query_error)?;
        let selected = store
            .configuration()
            .select_discovered_targets(
                &connection_id,
                &input.discovery_run_id,
                &input.engine_object_ids,
                now_ms,
            )
            .map_err(query_error)?;
        let connection = store
            .console_query()
            .connection_in_workspace(workspace_id.as_str(), connection_id.as_str())
            .map_err(query_error)?;
        let targets = selected
            .iter()
            .map(|target| {
                json!({
                    "id": target.id().as_str(),
                    "connectionId": target.connection_profile_id().as_str(),
                    "connectionName": connection.display_name.as_str(),
                    "engine": connection.engine.as_str(),
                    "displayName": target.display_name(),
                    "state": "active",
                    "missingSinceMs": null,
                    "sourceFingerprint": target.source_fingerprint(),
                    "latestRecoveryPointId": null,
                    "latestSuccessAtMs": null,
                    "latestFailureAtMs": null,
                    "artifactCompleteness": "unknown",
                    "replicaDurability": "unknown",
                    "protectionScope": "none",
                    "protectionCompliance": "not_configured",
                    "rpoViolation": false,
                    "verificationState": "not_configured",
                    "verificationOverdue": false,
                    "evidenceAtMs": null,
                })
            })
            .collect::<Vec<_>>();
        serialize_response(json!({ "targets": targets }))
    }

    fn destination_save(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let mut input: DestinationSavePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        input.validate()?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let destination_id = DestinationId::new();
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let endpoint_json =
            serde_json::to_string(&input.endpoint).map_err(|_| ApplicationError::Internal)?;
        let spec = match input.kind.as_str() {
            "local" => DestinationSpec::local(
                destination_id.clone(),
                input.display_name.trim(),
                endpoint_json,
                now_ms,
            ),
            "s3" => DestinationSpec::s3(
                destination_id.clone(),
                input.display_name.trim(),
                endpoint_json,
                now_ms,
            ),
            "ssh_sftp" => DestinationSpec::ssh_sftp(
                destination_id.clone(),
                input.display_name.trim(),
                endpoint_json,
                now_ms,
            ),
            _ => return Err(ApplicationError::InvalidPayload),
        };

        let mut created = Vec::new();
        for (role, purpose, credential) in input.credentials.take() {
            let bytes = serde_json::to_vec(&credential).map_err(|_| ApplicationError::Internal)?;
            let secrets = self.secrets.as_ref().ok_or(ApplicationError::Internal)?;
            match secrets.create(
                purpose,
                SecretBuffer::new(bytes).map_err(|_| ApplicationError::InvalidPayload)?,
                now_ms,
            ) {
                Ok(metadata) => created.push((role, metadata)),
                Err(_) => {
                    for (_, metadata) in &created {
                        let _ = secrets.revoke(metadata.id(), now_ms);
                    }
                    return Err(ApplicationError::Internal);
                }
            }
        }
        let bindings = created
            .iter()
            .map(|(role, metadata)| {
                DestinationSecretBindingSpec::new(
                    destination_id.clone(),
                    *role,
                    metadata.id().clone(),
                    now_ms,
                )
            })
            .collect::<Vec<_>>();
        let insert = self
            .store
            .as_ref()
            .ok_or(ApplicationError::Internal)?
            .configuration()
            .insert_destination_with_secret_bindings_in_workspace(&workspace_id, spec, bindings);
        if let Err(error) = insert {
            if let Some(secrets) = &self.secrets {
                for (_, metadata) in &created {
                    let _ = secrets.revoke(metadata.id(), now_ms);
                }
            }
            return Err(query_error(error));
        }
        Ok(response(json!({
            "destinationId": destination_id.as_str(),
            "state": "draft",
        })))
    }

    fn connection_operation(
        &self,
        payload: &Value,
        operation_kind: &'static str,
    ) -> Result<ApplicationResponse, ApplicationError> {
        let input: ConnectionOperationPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let connection_id = ConnectionProfileId::from_str(&input.connection_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .console_query()
            .connection_in_workspace(workspace_id.as_str(), connection_id.as_str())
            .map_err(query_error)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let operation_run_id = OperationRunId::new();
        let work_item_id = WorkItemId::new();
        let request_json = json!({
            "schemaVersion": 1,
            "connectionId": connection_id.as_str(),
        })
        .to_string();
        store
            .runtime()
            .enqueue_operation_work(OperationWorkSpec::new(
                operation_run_id.clone(),
                work_item_id.clone(),
                operation_kind,
                "connection_profile",
                connection_id.as_str(),
                request_json,
                now_ms,
            ))
            .map_err(query_error)?;
        let event_code = if operation_kind == "connection_probe" {
            "CONNECTION_PROBE_QUEUED"
        } else {
            "CONNECTION_DISCOVERY_QUEUED"
        };
        let sequence = store
            .activity()
            .append_batch(vec![ActivityEventSpec::info(
                uuid::Uuid::new_v4().to_string(),
                now_ms,
                event_code,
                "work_item",
                work_item_id.as_str(),
                "The service accepted the connection operation and queued durable work.",
            )])
            .map_err(query_error)?
            .into_iter()
            .next()
            .ok_or(ApplicationError::Internal)?;
        let mut body = json!({
            "workItemId": work_item_id.as_str(),
            "state": "queued",
            "activityAfterSeq": sequence,
        });
        if operation_kind == "connection_discovery" {
            body["discoveryRunId"] = json!(operation_run_id.as_str());
            body["discovered"] = json!([]);
        }
        Ok(response(body))
    }

    fn destination_probe(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: DestinationProbePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let destination_id = DestinationId::from_str(&input.destination_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_destination(&workspace_id, destination_id.as_str())
            .map_err(query_error)?;
        store
            .configuration()
            .destination_probe_binding(&destination_id)
            .map_err(query_error)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let operation_run_id = OperationRunId::new();
        let work_item_id = WorkItemId::new();
        store
            .runtime()
            .enqueue_operation_work(OperationWorkSpec::new(
                operation_run_id,
                work_item_id.clone(),
                "destination_probe",
                "destination",
                destination_id.as_str(),
                json!({
                    "schemaVersion": 1,
                    "destinationId": destination_id.as_str(),
                })
                .to_string(),
                now_ms,
            ))
            .map_err(query_error)?;
        let sequence = store
            .activity()
            .append_batch(vec![ActivityEventSpec::info(
                uuid::Uuid::new_v4().to_string(),
                now_ms,
                "DESTINATION_PROBE_QUEUED",
                "work_item",
                work_item_id.as_str(),
                "The service accepted the role-separated Destination probe.",
            )])
            .map_err(query_error)?
            .into_iter()
            .next()
            .ok_or(ApplicationError::Internal)?;
        Ok(response(json!({
            "workItemId": work_item_id.as_str(),
            "state": "queued",
            "activityAfterSeq": sequence,
        })))
    }

    fn support_bundle_create(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        validate_empty_payload(payload)?;
        if self._operation_worker.is_none() {
            return Err(ApplicationError::Internal);
        }
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let operation_run_id = OperationRunId::new();
        let work_item_id = WorkItemId::new();
        store
            .runtime()
            .enqueue_operation_work(OperationWorkSpec::new(
                operation_run_id.clone(),
                work_item_id.clone(),
                "support_bundle",
                "service_instance",
                "local",
                json!({"schemaVersion": 1}).to_string(),
                now_ms,
            ))
            .map_err(query_error)?;
        let sequence = store
            .activity()
            .append_batch(vec![ActivityEventSpec::info(
                uuid::Uuid::new_v4().to_string(),
                now_ms,
                "SUPPORT_BUNDLE_QUEUED",
                "work_item",
                work_item_id.as_str(),
                "The service accepted a bounded redacted support bundle operation.",
            )])
            .map_err(query_error)?
            .into_iter()
            .next()
            .ok_or(ApplicationError::Internal)?;
        Ok(response(json!({
            "operationRunId": operation_run_id.as_str(),
            "workItemId": work_item_id.as_str(),
            "state": "queued",
            "activityAfterSeq": sequence,
        })))
    }

    fn service_drain(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        validate_empty_payload(payload)?;
        self.health
            .begin_drain()
            .map_err(|_| ApplicationError::Internal)?;
        if let Some(worker) = &self._operation_worker {
            worker.request_stop();
        }
        if let Some(worker) = &self._backup_worker {
            worker.request_stop();
        }
        Ok(response(json!({
            "state": "draining",
            "claimsEnabled": false,
            "recoveryAction": "wait_for_active_leases_then_create_control_backup",
        })))
    }

    fn control_store_backup(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        validate_empty_payload(payload)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let root = self
            .maintenance_root
            .as_ref()
            .ok_or(ApplicationError::Internal)?
            .join("control-backups");
        std::fs::create_dir_all(&root).map_err(|_| ApplicationError::Internal)?;
        let backup_id = uuid::Uuid::new_v4().to_string();
        let backup_path = root.join(format!("control-{now_ms}-{backup_id}.db"));
        let rehearsal_path = root.join(format!("control-{now_ms}-{backup_id}.rehearsal.db"));
        let report = self
            .store
            .as_ref()
            .ok_or(ApplicationError::Internal)?
            .control_store()
            .backup_and_rehearse(ControlBackupSpec::new(
                backup_id.clone(),
                backup_path.clone(),
                rehearsal_path.clone(),
                now_ms,
            ))
            .map_err(query_error)?;
        Ok(response(json!({
            "backupId": backup_id,
            "backupPath": backup_path.to_string_lossy(),
            "rehearsalPath": rehearsal_path.to_string_lossy(),
            "checksumBlake3": report.checksum_blake3(),
            "integrityResult": report.integrity_result(),
            "rehearsalResult": report.rehearsal_result(),
            "schemaVersion": report.schema_version(),
            "applicationTableCount": report.application_table_count(),
        })))
    }

    fn service_update_commit(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        validate_empty_payload(payload)?;
        let health = self.health.snapshot();
        if health.phase() != HostPhase::Draining || !health.update_pending() {
            return Err(ApplicationError::Forbidden);
        }
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let pending_path = self
            .maintenance_root
            .as_ref()
            .ok_or(ApplicationError::Internal)?
            .join("updates")
            .join("update-pending.json");
        let metadata =
            std::fs::symlink_metadata(&pending_path).map_err(|_| ApplicationError::Internal)?;
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() == 0
            || metadata.len() > 64 * 1024
            || metadata_is_reparse_point(&metadata)
        {
            return Err(ApplicationError::Forbidden);
        }
        let document: UpdatePendingDocument = serde_json::from_slice(
            &std::fs::read(&pending_path).map_err(|_| ApplicationError::Internal)?,
        )
        .map_err(|_| ApplicationError::InvalidPayload)?;
        if document.schema_version != 1
            || document.target_version != health.current_binary_version()
            || crate::update::ReaderVersion::parse(&document.target_version).is_err()
            || document.created_at_ms < 0
            || document.created_at_ms > now_ms
            || uuid::Uuid::parse_str(&document.update_id)
                .map(|parsed| parsed.to_string() != document.update_id)
                .unwrap_or(true)
        {
            return Err(ApplicationError::InvalidPayload);
        }
        let minimum_advanced = match health.cutover_marker() {
            Some(marker) => self
                .store
                .as_ref()
                .ok_or(ApplicationError::Internal)?
                .cutover()
                .advance_minimum_reader_version(health.current_binary_version(), marker, now_ms)
                .map_err(query_error)?,
            None => false,
        };
        std::fs::remove_file(&pending_path).map_err(|_| ApplicationError::Internal)?;
        Ok(response(json!({
            "updateId": document.update_id,
            "targetVersion": document.target_version,
            "minimumReaderAdvanced": minimum_advanced,
            "restartRequired": true,
            "state": "committed",
        })))
    }

    fn work_get(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: WorkGetPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        WorkItemId::from_str(&input.work_item_id).map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let work = self
            .console_query()?
            .work_item_in_workspace(workspace_id.as_str(), &input.work_item_id)
            .map_err(query_error)?;
        serialize_response(work)
    }

    fn discovery_result_list(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        let input: DiscoveryResultListPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let page = self
            .console_query()?
            .discovery_results_in_workspace(
                &input.workspace_id,
                &input.discovery_run_id,
                input.cursor.as_deref(),
                input.limit,
                input.search.as_deref(),
                input.eligibility.as_deref(),
            )
            .map_err(query_error)?;
        serialize_response(page)
    }

    fn backup_scope_list(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: BackupScopeListPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        let target_id = input
            .database_target_id
            .as_deref()
            .map(DatabaseTargetId::from_str)
            .transpose()
            .map_err(|_| ApplicationError::InvalidPayload)?;
        if let Some(target_id) = &target_id {
            store
                .workspace()
                .require_database_target(&workspace_id, target_id.as_str())
                .map_err(query_error)?;
        }
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let scopes = store
            .console_query()
            .backup_scopes_page_in_workspace(
                workspace_id.as_str(),
                target_id.as_ref().map(DatabaseTargetId::as_str),
                input.cursor.as_deref(),
                input.limit,
                input.state.as_deref(),
                input.search.as_deref(),
                now_ms,
            )
            .map_err(query_error)?;
        serialize_response(scopes)
    }

    fn backup_scope_object_list(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        let input: BackupScopeObjectListPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let target_id = DatabaseTargetId::from_str(&input.database_target_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_database_target(&workspace_id, target_id.as_str())
            .map_err(query_error)?;
        let discovery_run_id = OperationRunId::from_str(&input.discovery_run_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let page = store
            .backup_scopes()
            .object_page_filtered(
                &target_id,
                &discovery_run_id,
                input.cursor.as_deref(),
                input.limit,
                input.search.as_deref(),
                input.schema_name.as_deref(),
                input.eligibility.as_deref(),
            )
            .map_err(query_error)?;
        serialize_response(page)
    }

    fn backup_scope_save(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: BackupScopeSavePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        input.validate()?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let target_id = DatabaseTargetId::from_str(&input.database_target_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_database_target(&workspace_id, target_id.as_str())
            .map_err(query_error)?;
        let scope = match input.mode.as_str() {
            "whole_database" => {
                let scope_id = BackupScopeId::from_str(target_id.as_str())
                    .map_err(|_| ApplicationError::InvalidPayload)?;
                store.backup_scopes().get(&scope_id).map_err(query_error)?
            }
            "explicit_tables" => {
                let discovery_id = input
                    .discovery_run_id
                    .as_deref()
                    .ok_or(ApplicationError::InvalidPayload)
                    .and_then(|value| {
                        OperationRunId::from_str(value)
                            .map_err(|_| ApplicationError::InvalidPayload)
                    })?;
                let now_ms = (self.clock)();
                if now_ms < 0 {
                    return Err(ApplicationError::Internal);
                }
                store
                    .backup_scopes()
                    .create_explicit(ExplicitBackupScopeSpec::new(
                        BackupScopeId::new(),
                        target_id,
                        input.display_name,
                        discovery_id,
                        input.engine_object_ids,
                        now_ms,
                    ))
                    .map_err(query_error)?
            }
            _ => return Err(ApplicationError::InvalidPayload),
        };
        serialize_response(scope)
    }

    fn backup_scope_discovery_run(
        &self,
        payload: &Value,
    ) -> Result<ApplicationResponse, ApplicationError> {
        let input: BackupScopeTargetPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let target_id = DatabaseTargetId::from_str(&input.database_target_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_database_target(&workspace_id, target_id.as_str())
            .map_err(query_error)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let operation_run_id = OperationRunId::new();
        let work_item_id = WorkItemId::new();
        store
            .runtime()
            .enqueue_operation_work(OperationWorkSpec::new(
                operation_run_id.clone(),
                work_item_id.clone(),
                "connection_discovery",
                "database_target",
                target_id.as_str(),
                json!({
                    "schemaVersion": 2,
                    "kind": "backup_scope_object_discovery",
                    "databaseTargetId": target_id.as_str(),
                })
                .to_string(),
                now_ms,
            ))
            .map_err(query_error)?;
        let sequence = store
            .activity()
            .append_batch(vec![ActivityEventSpec::info(
                uuid::Uuid::new_v4().to_string(),
                now_ms,
                "BACKUP_SCOPE_DISCOVERY_QUEUED",
                "work_item",
                work_item_id.as_str(),
                "The service queued table discovery for the selected database.",
            )])
            .map_err(query_error)?
            .into_iter()
            .next()
            .ok_or(ApplicationError::Internal)?;
        Ok(response(json!({
            "discoveryRunId": operation_run_id.as_str(),
            "workItemId": work_item_id.as_str(),
            "state": "queued",
            "activityAfterSeq": sequence,
        })))
    }

    fn work_cancel(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: WorkCancelPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let work_item_id = WorkItemId::from_str(&input.work_item_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_work_item(&workspace_id, work_item_id.as_str())
            .map_err(query_error)?;
        store
            .runtime()
            .request_cancel(&work_item_id, input.fencing_token, now_ms)
            .map_err(query_error)?;
        let work = self
            .console_query()?
            .work_item_in_workspace(workspace_id.as_str(), work_item_id.as_str())
            .map_err(query_error)?;
        serialize_response(work)
    }

    fn plan_save(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: PlanSavePayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        input.validate()?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let scope_ids = input
            .scope_ids
            .iter()
            .map(|id| BackupScopeId::from_str(id).map_err(|_| ApplicationError::InvalidPayload))
            .collect::<Result<Vec<_>, _>>()?;
        let destinations = input
            .destinations
            .iter()
            .map(|destination| {
                Ok((
                    DestinationId::from_str(&destination.destination_id)
                        .map_err(|_| ApplicationError::InvalidPayload)?,
                    DestinationRequirement::from_str(&destination.requirement)
                        .map_err(|_| ApplicationError::InvalidPayload)?,
                ))
            })
            .collect::<Result<Vec<_>, ApplicationError>>()?;
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        for (destination_id, _) in &destinations {
            store
                .workspace()
                .require_destination(&workspace_id, destination_id.as_str())
                .map_err(query_error)?;
        }
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let plan_id = BackupPlanId::new();
        let revision_id = PlanRevisionId::new();
        let canonical = serde_json::to_vec(&input).map_err(|_| ApplicationError::Internal)?;
        let content_hash = blake3::hash(&canonical).to_hex().to_string();
        let retention_policy =
            serde_json::to_string(&input.retention).map_err(|_| ApplicationError::Internal)?;
        let verification_policy =
            serde_json::to_string(&input.verification).map_err(|_| ApplicationError::Internal)?;
        let execution_policy = json!({
            "catchupPolicy": "bounded_latest_one",
            "overlapPolicy": "coalesce_one",
        })
        .to_string();
        let spec = PlanDraftSpec::cron(
            plan_id.clone(),
            revision_id.clone(),
            input.display_name.trim(),
            1,
            input.schedule.expression.trim(),
            input.schedule.timezone.trim(),
            content_hash,
            now_ms,
        )
        .with_policies(
            Some(input.rpo_ms),
            retention_policy,
            verification_policy,
            execution_policy,
        );
        store
            .configuration()
            .insert_plan_draft_with_scope_membership_in_workspace(
                &workspace_id,
                spec,
                scope_ids,
                destinations,
            )
            .map_err(query_error)?;
        Ok(response(json!({
            "planId": plan_id.as_str(),
            "revisionId": revision_id.as_str(),
            "state": "draft",
        })))
    }

    fn plan_publish(&self, payload: &Value) -> Result<ApplicationResponse, ApplicationError> {
        let input: PlanPublishPayload = serde_json::from_value(payload.clone())
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let plan_id =
            BackupPlanId::from_str(&input.plan_id).map_err(|_| ApplicationError::InvalidPayload)?;
        let revision_id = PlanRevisionId::from_str(&input.revision_id)
            .map_err(|_| ApplicationError::InvalidPayload)?;
        let workspace_id = parse_workspace_id(&input.workspace_id)?;
        let now_ms = (self.clock)();
        if now_ms < 0 {
            return Err(ApplicationError::Internal);
        }
        let store = self.store.as_ref().ok_or(ApplicationError::Internal)?;
        store
            .workspace()
            .require_plan(&workspace_id, plan_id.as_str())
            .map_err(query_error)?;
        store
            .configuration()
            .publish_plan_revision(&plan_id, &revision_id, now_ms)
            .map_err(query_error)?;
        Ok(response(json!({
            "planId": plan_id.as_str(),
            "revisionId": revision_id.as_str(),
            "state": "enabled",
            "publishedAtMs": now_ms,
        })))
    }

    fn console_query(
        &self,
    ) -> Result<restoreplane_catalog::ConsoleQueryRepository, ApplicationError> {
        self.store
            .as_ref()
            .map(OperationalStore::console_query)
            .ok_or(ApplicationError::Internal)
    }
}

impl ApplicationUseCases for ServiceApplication {
    fn execute(
        &self,
        request: ApplicationRequest,
    ) -> Result<ApplicationResponse, ApplicationError> {
        if self.health.snapshot().phase() == HostPhase::Draining
            && request.command.descriptor().mutation()
                != restoreplane_application::MutationClass::ReadOnly
            && request.command != ApplicationCommand::ControlStoreBackup
            && request.command != ApplicationCommand::ServiceUpdateCommit
        {
            return Err(ApplicationError::RehearsalOnly);
        }
        match request.command {
            ApplicationCommand::ServiceHealth => {
                validate_empty_payload(&request.payload)?;
                self.service_health()
            }
            ApplicationCommand::ServiceDiagnostics => {
                validate_empty_payload(&request.payload)?;
                self.service_diagnostics()
            }
            ApplicationCommand::SchemaStatus => {
                validate_empty_payload(&request.payload)?;
                self.schema_status()
            }
            ApplicationCommand::WorkspaceList => self.workspace_list(&request.payload),
            ApplicationCommand::WorkspaceCreate => self.workspace_create(&request.payload),
            ApplicationCommand::ConnectionList => self.connection_list(&request.payload),
            ApplicationCommand::TargetList => self.target_list(&request.payload),
            ApplicationCommand::DiscoveryResultList => self.discovery_result_list(&request.payload),
            ApplicationCommand::DestinationList => self.destination_list(&request.payload),
            ApplicationCommand::PlanList => self.plan_list(&request.payload),
            ApplicationCommand::EngineMatrix => self.engine_matrix(),
            ApplicationCommand::SnapshotList => self.snapshot_list(&request.payload),
            ApplicationCommand::SnapshotGet => self.snapshot_get(&request.payload),
            ApplicationCommand::CatalogRebuildPreview => {
                self.catalog_rebuild_preview(&request.payload)
            }
            ApplicationCommand::ActivityList => self.activity_list(&request.payload),
            ApplicationCommand::SettingsRead => self.settings_read(&request.payload),
            ApplicationCommand::ConnectionSave => self.connection_save(&request.payload),
            ApplicationCommand::ConnectionProbe => {
                self.connection_operation(&request.payload, "connection_probe")
            }
            ApplicationCommand::DiscoveryRun => {
                self.connection_operation(&request.payload, "connection_discovery")
            }
            ApplicationCommand::TargetSelect => self.target_select(&request.payload),
            ApplicationCommand::BackupScopeList => self.backup_scope_list(&request.payload),
            ApplicationCommand::BackupScopeDiscoveryRun => {
                self.backup_scope_discovery_run(&request.payload)
            }
            ApplicationCommand::BackupScopeObjectList => {
                self.backup_scope_object_list(&request.payload)
            }
            ApplicationCommand::BackupScopeSave => self.backup_scope_save(&request.payload),
            ApplicationCommand::DestinationSave => self.destination_save(&request.payload),
            ApplicationCommand::DestinationProbe => self.destination_probe(&request.payload),
            ApplicationCommand::SupportBundleCreate => self.support_bundle_create(&request.payload),
            ApplicationCommand::ServiceDrain => self.service_drain(&request.payload),
            ApplicationCommand::ServiceUpdateCommit => self.service_update_commit(&request.payload),
            ApplicationCommand::ControlStoreBackup => self.control_store_backup(&request.payload),
            ApplicationCommand::PlanSave => self.plan_save(&request.payload),
            ApplicationCommand::PlanPublish => self.plan_publish(&request.payload),
            ApplicationCommand::VerificationEnqueue => self.verification_enqueue(&request.payload),
            ApplicationCommand::RestorePreflight => self.restore_preflight(&request.payload),
            ApplicationCommand::RestoreEnqueueSafe => self.restore_enqueue_safe(&request.payload),
            ApplicationCommand::WorkGet => self.work_get(&request.payload),
            ApplicationCommand::WorkCancel => self.work_cancel(&request.payload),
            ApplicationCommand::RetentionDryRun => self.retention_dry_run(&request.payload),
            _ => Err(ApplicationError::UnknownCommand),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PagePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    cursor: Option<String>,
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceListPayload {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_inventory_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TargetListPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    #[serde(default)]
    cursor: Option<String>,
    limit: usize,
    #[serde(default)]
    connection_id: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    search: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiscoveryResultListPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    discovery_run_id: String,
    #[serde(default)]
    cursor: Option<String>,
    limit: usize,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    eligibility: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ActivityPagePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    cursor: Option<String>,
    limit: usize,
    severity: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotListPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    cursor: Option<String>,
    limit: usize,
    #[serde(default = "default_snapshot_scope")]
    scope: String,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    connection_id: Option<String>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    from_ms: Option<i64>,
    #[serde(default)]
    to_ms: Option<i64>,
    #[serde(default)]
    artifact_completeness: Option<String>,
    #[serde(default)]
    replica_durability: Option<String>,
    #[serde(default)]
    protection_compliance: Option<String>,
    #[serde(default)]
    min_verification_level: Option<i64>,
    #[serde(default)]
    restore_eligibility: Option<String>,
    #[serde(default)]
    signature_status: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SnapshotGetPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    snapshot_id: String,
    section: String,
    #[serde(default)]
    cursor: Option<String>,
    limit: usize,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogRebuildPreviewPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    destination_id: String,
    #[serde(default)]
    job_filter: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdatePendingDocument {
    schema_version: u32,
    update_id: String,
    target_version: String,
    created_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerificationEnqueuePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    snapshot_id: String,
    level: i64,
    purpose: String,
    #[serde(default)]
    source_replica_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestorePreflightPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    snapshot_id: String,
    mode: String,
    target_connection_id: String,
    target_name: String,
    #[serde(default = "default_restore_purpose")]
    purpose: String,
    #[serde(default)]
    source_replica_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SafeRestoreEnqueuePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    snapshot_id: String,
    source_replica_id: String,
    target_connection_id: String,
    target_name: String,
    preflight_version: String,
    required_verification_level: i64,
    #[serde(default = "default_restore_purpose")]
    purpose: String,
}

fn default_snapshot_scope() -> String {
    "complete".to_owned()
}

const fn default_inventory_limit() -> usize {
    100
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetentionDryRunPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    recovery_point_id: String,
    destination_fingerprint: String,
    grace_period_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConnectionSavePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    display_name: String,
    engine: String,
    transport: String,
    host: String,
    port: u16,
    file_path: String,
    tls_mode: String,
    username: String,
    password: String,
    #[serde(default)]
    restore_test_target: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceCreatePayload {
    display_name: String,
}

fn default_restore_purpose() -> String {
    "incident_recovery".to_owned()
}

fn default_workspace_id_string() -> String {
    DEFAULT_WORKSPACE_ID.to_owned()
}

fn parse_workspace_id(value: &str) -> Result<WorkspaceId, ApplicationError> {
    WorkspaceId::from_str(value).map_err(|_| ApplicationError::InvalidPayload)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TargetSelectPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    connection_id: String,
    #[serde(alias = "discoveryWorkItemId")]
    discovery_run_id: String,
    engine_object_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DestinationSavePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    display_name: String,
    kind: String,
    endpoint: Value,
    #[serde(default)]
    credentials: DestinationCredentialsPayload,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConnectionOperationPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    connection_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DestinationProbePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    destination_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkGetPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    work_item_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkCancelPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    work_item_id: String,
    fencing_token: i64,
}

impl DestinationSavePayload {
    fn validate(&self) -> Result<(), ApplicationError> {
        let display_valid = !self.display_name.trim().is_empty()
            && self.display_name.len() <= 256
            && !self.display_name.contains(['\0', '\r', '\n']);
        let endpoint_valid = validate_destination_endpoint(&self.kind, &self.endpoint);
        let credentials_valid = match self.kind.as_str() {
            "local" => self.credentials.is_empty(),
            "s3" | "ssh_sftp" => {
                valid_destination_secret(&self.kind, self.credentials.backup_writer.as_ref())
                    && valid_destination_secret(
                        &self.kind,
                        self.credentials.restore_reader.as_ref(),
                    )
                    && self
                        .credentials
                        .delete_operator
                        .as_ref()
                        .is_none_or(|value| valid_destination_secret(&self.kind, Some(value)))
            }
            _ => false,
        };
        if display_valid && endpoint_valid && credentials_valid {
            Ok(())
        } else {
            Err(ApplicationError::InvalidPayload)
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DestinationCredentialsPayload {
    backup_writer: Option<Value>,
    restore_reader: Option<Value>,
    delete_operator: Option<Value>,
}

impl DestinationCredentialsPayload {
    fn is_empty(&self) -> bool {
        self.backup_writer.is_none()
            && self.restore_reader.is_none()
            && self.delete_operator.is_none()
    }

    fn take(&mut self) -> Vec<(DestinationSecretRoleV2, SecretPurpose, Value)> {
        [
            (
                DestinationSecretRoleV2::BackupWriter,
                SecretPurpose::BackupWriter,
                self.backup_writer.take(),
            ),
            (
                DestinationSecretRoleV2::RestoreReader,
                SecretPurpose::RestoreReader,
                self.restore_reader.take(),
            ),
            (
                DestinationSecretRoleV2::DeleteOperator,
                SecretPurpose::RetentionDelete,
                self.delete_operator.take(),
            ),
        ]
        .into_iter()
        .filter_map(|(role, purpose, value)| value.map(|value| (role, purpose, value)))
        .collect()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanSavePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    display_name: String,
    #[serde(alias = "targetIds")]
    scope_ids: Vec<String>,
    destinations: Vec<PlanDestinationPayload>,
    schedule: PlanSchedulePayload,
    rpo_ms: i64,
    retention: PlanRetentionPayload,
    verification: PlanVerificationPayload,
}

impl PlanSavePayload {
    fn validate(&self) -> Result<(), ApplicationError> {
        let valid = !self.display_name.trim().is_empty()
            && self.display_name.len() <= 256
            && !self.display_name.contains(['\0', '\r', '\n'])
            && !self.scope_ids.is_empty()
            && self.scope_ids.len() <= 1_000
            && !self.destinations.is_empty()
            && self.destinations.len() <= 128
            && self.schedule.kind == "cron"
            && !self.schedule.expression.trim().is_empty()
            && self.schedule.expression.len() <= 256
            && !self.schedule.timezone.trim().is_empty()
            && self.schedule.timezone.len() <= 128
            && self.rpo_ms > 0
            && self.rpo_ms <= 365 * 24 * 60 * 60 * 1_000
            && self.retention.valid()
            && (1..=3).contains(&self.verification.requested_level);
        if valid {
            Ok(())
        } else {
            Err(ApplicationError::InvalidPayload)
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanDestinationPayload {
    destination_id: String,
    requirement: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanSchedulePayload {
    kind: String,
    expression: String,
    timezone: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanRetentionPayload {
    daily: i64,
    weekly: i64,
    monthly: i64,
}

impl PlanRetentionPayload {
    fn valid(&self) -> bool {
        [self.daily, self.weekly, self.monthly]
            .into_iter()
            .all(|value| (0..=36_500).contains(&value))
            && self.daily + self.weekly + self.monthly > 0
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanVerificationPayload {
    requested_level: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanPublishPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    plan_id: String,
    revision_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupScopeListPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    #[serde(default)]
    database_target_id: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    limit: usize,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    search: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupScopeTargetPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    database_target_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupScopeObjectListPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    database_target_id: String,
    discovery_run_id: String,
    #[serde(default)]
    cursor: Option<String>,
    limit: u32,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    schema_name: Option<String>,
    #[serde(default)]
    eligibility: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BackupScopeSavePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    database_target_id: String,
    display_name: String,
    mode: String,
    #[serde(default)]
    discovery_run_id: Option<String>,
    #[serde(default)]
    engine_object_ids: Vec<String>,
}

impl BackupScopeSavePayload {
    fn validate(&self) -> Result<(), ApplicationError> {
        let name_valid = !self.display_name.trim().is_empty()
            && self.display_name.chars().count() <= 96
            && !self.display_name.chars().any(char::is_control);
        let mode_valid = match self.mode.as_str() {
            "whole_database" => {
                self.discovery_run_id.is_none() && self.engine_object_ids.is_empty()
            }
            "explicit_tables" => {
                self.discovery_run_id.is_some()
                    && !self.engine_object_ids.is_empty()
                    && self.engine_object_ids.len() <= 1_000
            }
            _ => false,
        };
        if name_valid && mode_valid {
            Ok(())
        } else {
            Err(ApplicationError::InvalidPayload)
        }
    }
}

impl ConnectionSavePayload {
    fn validate(&self) -> Result<(), ApplicationError> {
        let common_valid = !self.display_name.trim().is_empty()
            && self.display_name.len() <= 256
            && matches!(
                self.tls_mode.as_str(),
                "disabled" | "preferred" | "required" | "verify_ca" | "verify_full"
            )
            && self.username.len() <= 256
            && self.password.len() <= 60 * 1024
            && !self.display_name.contains(['\0', '\r', '\n'])
            && !self.username.contains(['\0', '\r', '\n'])
            && !self.password.contains('\0');
        let transport_valid = match self.transport.as_str() {
            "tcp" => {
                matches!(
                    self.engine.as_str(),
                    "postgresql" | "mysql" | "mariadb" | "sqlserver"
                ) && !self.host.trim().is_empty()
                    && self.host.len() <= 1_024
                    && self.file_path.trim().is_empty()
                    && !self.username.trim().is_empty()
                    && !self.password.is_empty()
            }
            "file" => {
                self.engine == "sqlite"
                    && self.host.trim().is_empty()
                    && !self.file_path.trim().is_empty()
                    && self.file_path.len() <= 4_096
                    && self.username.is_empty()
                    && self.password.is_empty()
                    && self.tls_mode == "disabled"
            }
            _ => false,
        };
        if common_valid && transport_valid {
            Ok(())
        } else {
            Err(ApplicationError::InvalidPayload)
        }
    }
}

#[derive(Serialize)]
struct DatabaseCredentialSecret<'a> {
    username: &'a str,
    password: &'a str,
}

fn validate_destination_endpoint(kind: &str, endpoint: &Value) -> bool {
    let Some(object) = endpoint.as_object() else {
        return false;
    };
    let text = |name: &str, maximum: usize| {
        object
            .get(name)
            .and_then(Value::as_str)
            .is_some_and(|value| {
                !value.trim().is_empty()
                    && value.len() <= maximum
                    && !value.contains(['\0', '\r', '\n'])
            })
    };
    match kind {
        "local" => object.keys().all(|key| key == "path") && text("path", 4_096),
        "s3" => {
            object.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "bucket"
                        | "prefix"
                        | "region"
                        | "endpointUrl"
                        | "forcePathStyle"
                        | "provider"
                        | "objectLockMode"
                        | "retentionDays"
                        | "minDwellDays"
                        | "checksumAlgo"
                        | "complianceOptIn"
                )
            }) && text("bucket", 255)
                && text("region", 128)
                && object.get("prefix").is_none_or(|value| {
                    value.as_str().is_some_and(|text| {
                        text.len() <= 1_024 && !text.contains(['\0', '\r', '\n'])
                    })
                })
                && object.get("endpointUrl").is_none_or(|value| {
                    value.as_str().is_some_and(|text| {
                        (text.starts_with("https://") || text.starts_with("http://"))
                            && text.len() <= 2_048
                            && !text.contains(['\0', '\r', '\n'])
                    })
                })
                && object.get("forcePathStyle").is_none_or(Value::is_boolean)
                && object.get("provider").is_none_or(|value| {
                    value.as_str().is_some_and(|provider| {
                        matches!(provider, "aws" | "minio" | "backblaze_b2" | "cloudflare_r2")
                    })
                })
                && object.get("objectLockMode").is_none_or(|value| {
                    value
                        .as_str()
                        .is_some_and(|mode| matches!(mode, "none" | "governance" | "compliance"))
                })
                && object
                    .get("retentionDays")
                    .is_none_or(|value| value.as_u64().is_some_and(|days| days <= 36_500))
                && object
                    .get("minDwellDays")
                    .is_none_or(|value| value.as_u64().is_some_and(|days| days <= 36_500))
                && object
                    .get("retentionDays")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    >= object
                        .get("minDwellDays")
                        .and_then(Value::as_u64)
                        .unwrap_or(0)
                && object.get("checksumAlgo").is_none_or(|value| {
                    value
                        .as_str()
                        .is_some_and(|algorithm| algorithm == "crc32c")
                })
                && object.get("complianceOptIn").is_none_or(Value::is_boolean)
                && (object.get("objectLockMode").and_then(Value::as_str) != Some("compliance")
                    || object.get("complianceOptIn").and_then(Value::as_bool) == Some(true))
        }
        "ssh_sftp" => {
            object.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "host"
                        | "port"
                        | "user"
                        | "username"
                        | "path"
                        | "hostKeyFingerprint"
                        | "hostKeySha256"
                        | "appendOnly"
                )
            }) && text("host", 1_024)
                && (text("user", 256) ^ text("username", 256))
                && text("path", 4_096)
                && object
                    .get("port")
                    .and_then(Value::as_u64)
                    .is_some_and(|port| (1..=65_535).contains(&port))
                && (text("hostKeyFingerprint", 128) ^ text("hostKeySha256", 128))
                && object.get("appendOnly").is_none_or(Value::is_boolean)
        }
        _ => false,
    }
}

fn valid_destination_secret(kind: &str, value: Option<&Value>) -> bool {
    let Some(object) = value.and_then(Value::as_object) else {
        return false;
    };
    if serde_json::to_vec(object)
        .map(|bytes| bytes.len() > 60 * 1024 || bytes.contains(&0))
        .unwrap_or(true)
        || object.get("schemaVersion").and_then(Value::as_u64) != Some(1)
    {
        return false;
    }
    match kind {
        "s3" => {
            object.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "schemaVersion" | "accessKeyId" | "secretAccessKey"
                )
            }) && secret_text(object.get("accessKeyId"), 512)
                && secret_text(object.get("secretAccessKey"), 60 * 1024)
        }
        "ssh_sftp" => {
            if !object
                .keys()
                .all(|key| matches!(key.as_str(), "schemaVersion" | "auth"))
            {
                return false;
            }
            let Some(auth) = object.get("auth").and_then(Value::as_object) else {
                return false;
            };
            match auth.get("kind").and_then(Value::as_str) {
                Some("password") => {
                    auth.keys()
                        .all(|key| matches!(key.as_str(), "kind" | "password"))
                        && secret_text(auth.get("password"), 60 * 1024)
                }
                Some("public_key") => {
                    auth.keys()
                        .all(|key| matches!(key.as_str(), "kind" | "identityPem" | "passphrase"))
                        && secret_text(auth.get("identityPem"), 60 * 1024)
                        && auth.get("passphrase").is_none_or(|value| {
                            value
                                .as_str()
                                .is_some_and(|text| text.len() <= 60 * 1024 && !text.contains('\0'))
                        })
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn secret_text(value: Option<&Value>, maximum: usize) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|text| !text.is_empty() && text.len() <= maximum && !text.contains('\0'))
}

fn valid_restore_target_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    value.len() <= 128
        && (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn restore_tool_for_engine(engine: &str) -> Option<&'static str> {
    match engine {
        "postgresql" => Some("pg_restore"),
        "mysql" => Some("mysql"),
        "mariadb" => Some("mariadb"),
        "sqlserver" => None,
        "sqlite" => None,
        _ => None,
    }
}

fn restore_target_fingerprint(
    connection_id: &str,
    engine: &str,
    target_name: &str,
    server_version: Option<&str>,
    server_identity: Option<&str>,
) -> String {
    let identity = if engine == "sqlite" {
        "embedded-sqlite"
    } else {
        server_identity.unwrap_or("unavailable")
    };
    blake3::hash(
        format!(
            "restore-target-v2\0{connection_id}\0{engine}\0{}\0{}\0{identity}",
            target_name.to_ascii_lowercase(),
            server_version.unwrap_or("unknown")
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

fn is_protected_database_name(engine: &str, name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    match engine {
        "postgresql" => matches!(name.as_str(), "postgres" | "template0" | "template1"),
        "mysql" | "mariadb" => matches!(
            name.as_str(),
            "mysql" | "information_schema" | "performance_schema" | "sys"
        ),
        "sqlserver" => matches!(name.as_str(), "master" | "model" | "msdb" | "tempdb"),
        _ => false,
    }
}

fn preflight_check(id: &str, state: &str, detail: &str, action: &str) -> Value {
    json!({
        "id": id,
        "state": state,
        "detail": detail,
        "action": action,
    })
}

fn validate_empty_payload(payload: &Value) -> Result<(), ApplicationError> {
    match payload {
        Value::Object(object) if object.is_empty() => Ok(()),
        _ => Err(ApplicationError::InvalidPayload),
    }
}

fn decode_page(payload: &Value) -> Result<PagePayload, ApplicationError> {
    serde_json::from_value(payload.clone()).map_err(|_| ApplicationError::InvalidPayload)
}

fn snapshot_summary_in_workspace(
    store: &OperationalStore,
    workspace_id: &str,
    recovery_point_id: &RecoveryPointId,
) -> Result<ConsoleSnapshot, ApplicationError> {
    store
        .console_query()
        .snapshots_in_workspace(
            workspace_id,
            ConsoleSnapshotQuery {
                cursor: None,
                limit: 2,
                scope: "all".to_owned(),
                search: Some(recovery_point_id.as_str().to_owned()),
                connection_id: None,
                engine: None,
                from_ms: None,
                to_ms: None,
                artifact_completeness: None,
                replica_durability: None,
                protection_compliance: None,
                min_verification_level: None,
                restore_eligibility: None,
                signature_status: None,
            },
        )
        .map_err(query_error)?
        .items
        .into_iter()
        .find(|snapshot| snapshot.id == recovery_point_id.as_str())
        .ok_or(ApplicationError::InvalidPayload)
}

fn query_error(error: RepositoryError) -> ApplicationError {
    match error {
        RepositoryError::Domain(DomainError::PlanRevisionTargetNotReady) => {
            ApplicationError::PlanRevisionTargetNotReady
        }
        RepositoryError::Domain(DomainError::PlanRevisionEngineCapabilityUnavailable) => {
            ApplicationError::PlanRevisionEngineCapabilityUnavailable
        }
        RepositoryError::Domain(DomainError::PlanRevisionDestinationNotReady) => {
            ApplicationError::PlanRevisionDestinationNotReady
        }
        RepositoryError::Domain(DomainError::PlanRevisionVerificationUnsupported) => {
            ApplicationError::PlanRevisionVerificationUnsupported
        }
        RepositoryError::RuntimeStateInvalid
        | RepositoryError::NotFound
        | RepositoryError::WorkspaceInputInvalid
        | RepositoryError::WorkspaceNameConflict
        | RepositoryError::WorkspaceScopeMismatch
        | RepositoryError::BackupScopeInputInvalid
        | RepositoryError::BackupScopeDiscoveryInvalid
        | RepositoryError::BackupScopeObjectMissing
        | RepositoryError::BackupScopeConflict => ApplicationError::InvalidPayload,
        RepositoryError::StaleFence => ApplicationError::WorkItemFenceStale,
        _ => ApplicationError::Internal,
    }
}

fn serialize_response(
    value: impl serde::Serialize,
) -> Result<ApplicationResponse, ApplicationError> {
    serde_json::to_value(value)
        .map(response)
        .map_err(|_| ApplicationError::Internal)
}

fn response(body: Value) -> ApplicationResponse {
    ApplicationResponse {
        deduplicated: false,
        resource: None,
        body,
    }
}

const fn phase_name(phase: HostPhase) -> &'static str {
    match phase {
        HostPhase::Starting => "starting",
        HostPhase::RehearsalReady => "rehearsal_ready",
        HostPhase::ProductionReady => "production_ready",
        HostPhase::DiagnosticsOnly => "diagnostics_only",
        HostPhase::Draining => "draining",
        HostPhase::Stopped => "stopped",
    }
}

const fn authority_name(authority: ExecutionAuthority) -> &'static str {
    match authority {
        ExecutionAuthority::LegacyDirect => "legacy_direct",
        ExecutionAuthority::Service => "service",
    }
}

const fn cutover_name(mode: CutoverMode) -> &'static str {
    match mode {
        CutoverMode::RehearsalOnly => "rehearsal_only",
        CutoverMode::Production => "production",
    }
}

const fn recovery_disposition_name(
    disposition: PersistedCutoverRecoveryDisposition,
) -> &'static str {
    match disposition {
        PersistedCutoverRecoveryDisposition::RollbackEligible => "rollback_eligible",
        PersistedCutoverRecoveryDisposition::ForwardFixRequired => "forward_fix_required",
    }
}

const fn synchronous_name(value: i64) -> &'static str {
    match value {
        0 => "off",
        1 => "normal",
        2 => "full",
        3 => "extra",
        _ => "unknown",
    }
}

const fn disposition_name(value: RetentionDispositionV2) -> &'static str {
    match value {
        RetentionDispositionV2::Preserve => "preserve",
        RetentionDispositionV2::TombstoneRequired => "tombstone_required",
        RetentionDispositionV2::WaitForGrace => "wait_for_grace",
        RetentionDispositionV2::DeleteEligible => "delete_eligible",
    }
}

const fn reason_name(value: RetentionReasonV2) -> &'static str {
    match value {
        RetentionReasonV2::PolicyNotExpired => "policy_not_expired",
        RetentionReasonV2::Pinned => "pinned",
        RetentionReasonV2::LegalHold => "legal_hold",
        RetentionReasonV2::ObjectLockActive => "object_lock_active",
        RetentionReasonV2::ObjectLockEvidenceMissing => "object_lock_evidence_missing",
        RetentionReasonV2::ServerAppendOnly => "server_append_only",
        RetentionReasonV2::LastCompliantRecoveryPoint => "last_compliant_recovery_point",
        RetentionReasonV2::ActiveRestoreSource => "active_restore_source",
        RetentionReasonV2::ActiveRepairSource => "active_repair_source",
    }
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn metadata_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(i64::MAX)
}
