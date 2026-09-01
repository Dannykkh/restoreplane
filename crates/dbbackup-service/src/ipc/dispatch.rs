use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dbbackup_application::{
    canonical_payload_digest, enforce_authority, ApplicationCommand, ApplicationError,
    ApplicationFailure, ApplicationRequest, ApplicationResponse, ApplicationUseCases,
    AuthoritySnapshot, ConfirmationAction, CutoverMode, ExecutionAuthority, MutationClass,
    ResourceRef,
};
use dbbackup_catalog::{
    ConfigCutoverConfirmationIssueResult, ConfigCutoverConfirmationRequestSpec, IpcCompletionSpec,
    IpcLedgerReservation, IpcRepository, IpcRequestSpec, ManualBackupEnqueueResult,
    ManualBackupRequestSpec, ManualVerificationEnqueueResult, ManualVerificationRequestSpec,
    OverwriteRestoreSpec, PersistedCutoverMode, PersistedExecutionAuthority,
    ReplicaRepairEnqueueResult, ReplicaRepairRequestSpec, RepositoryError,
    RestoreOverwriteConfirmationIssueResult, RestoreOverwriteConfirmationRequestSpec,
    RestorePurpose, RetentionConfirmationIssueResult, RetentionConfirmationRequestSpec,
    RetentionDeleteEnqueueResult, RetentionDeleteSpec, RetentionSafetyControlSpec,
    RetentionTombstoneSpec, SafeRestoreSpec, DEFAULT_WORKSPACE_ID,
};
use dbbackup_domain::{
    ArtifactReplicaId, BackupPlanId, ConfirmationTokenId, ConnectionProfileId, DestinationId,
    OperationRunId, RecoveryPointId, RestoreRunId, VerificationRunId, WorkItemId, WorkspaceId,
};
use dbbackup_pipeline::RetentionDeleteAuthorizationV2;
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use uuid::Uuid;

use super::{
    AuthorizedRequest, ConfirmationServiceError, IpcError, RawConfirmationToken, ResponseEnvelope,
    MAX_CONFIRMATION_TTL_MS,
};
use crate::cutover::{CutoverReadinessError, CutoverReadinessInputV1, CutoverReadinessVerifierV1};

#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("{0}")]
    Ipc(#[from] IpcError),
    #[error("{0}")]
    Application(#[from] dbbackup_application::ApplicationError),
    #[error("{0}")]
    Repository(#[from] RepositoryError),
    #[error("{0}")]
    Confirmation(#[from] ConfirmationServiceError),
    #[error("{0}")]
    CutoverReadiness(#[from] CutoverReadinessError),
    #[error("IPC_STORED_RESPONSE_INVALID")]
    StoredResponseInvalid,
    #[error("IPC_TYPED_TRANSACTION_REQUIRED")]
    TypedTransactionRequired,
}

impl DispatchError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Ipc(error) => error.code(),
            Self::Application(error) => error.code(),
            Self::Repository(error) => error.code(),
            Self::Confirmation(error) => error.code(),
            Self::CutoverReadiness(error) => error.code(),
            Self::StoredResponseInvalid => "IPC_STORED_RESPONSE_INVALID",
            Self::TypedTransactionRequired => "IPC_TYPED_TRANSACTION_REQUIRED",
        }
    }
}

pub struct DurableIpcDispatcher<U>
where
    U: ApplicationUseCases,
{
    repository: IpcRepository,
    use_cases: Arc<U>,
    authority: AuthoritySnapshot,
    cutover_verifier: Option<Arc<dyn CutoverReadinessVerifierV1>>,
}

impl<U> DurableIpcDispatcher<U>
where
    U: ApplicationUseCases,
{
    pub const fn new(
        repository: IpcRepository,
        use_cases: Arc<U>,
        authority: AuthoritySnapshot,
    ) -> Self {
        Self {
            repository,
            use_cases,
            authority,
            cutover_verifier: None,
        }
    }

    pub fn with_cutover_verifier(
        repository: IpcRepository,
        use_cases: Arc<U>,
        authority: AuthoritySnapshot,
        cutover_verifier: Arc<dyn CutoverReadinessVerifierV1>,
    ) -> Self {
        Self {
            repository,
            use_cases,
            authority,
            cutover_verifier: Some(cutover_verifier),
        }
    }

    pub fn dispatch(
        &self,
        authorized: AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let authority = self.current_authority()?;
        enforce_authority(&authority, request.command())?;
        if request.command() == ApplicationCommand::RetentionControlSet {
            return self.set_retention_control(&authorized);
        }
        if request.command() == ApplicationCommand::RetentionTombstone {
            return self.stage_retention_tombstone(&authorized);
        }
        if request.command() == ApplicationCommand::ConfirmationPrepare {
            return self.prepare_confirmation(&authorized);
        }
        if request.command() == ApplicationCommand::RetentionDelete {
            return self.enqueue_retention_delete(&authorized);
        }
        if request.command() == ApplicationCommand::RestoreEnqueueSafe {
            return self.enqueue_safe_restore(&authorized);
        }
        if request.command() == ApplicationCommand::RestoreEnqueueOverwrite {
            return self.enqueue_overwrite_restore(&authorized);
        }
        if request.command() == ApplicationCommand::RepairEnqueue {
            return self.enqueue_replica_repair(&authorized);
        }
        if request.command() == ApplicationCommand::VerificationEnqueue {
            return self.enqueue_manual_verification(&authorized);
        }
        if request.command() == ApplicationCommand::BackupEnqueue {
            return self.enqueue_manual_backup(&authorized);
        }
        if request.command() == ApplicationCommand::ConfigCutover {
            return self.commit_config_cutover(&authorized);
        }
        let mutation = request.command().descriptor().mutation();
        if mutation == MutationClass::ReadOnly {
            return self.invoke(request, false);
        }
        if matches!(
            mutation,
            MutationClass::Enqueue | MutationClass::Destructive
        ) {
            return Err(DispatchError::TypedTransactionRequired);
        }

        let payload_digest = canonical_payload_digest(request.payload())?;
        let idempotency_key = request
            .idempotency_key()
            .ok_or(IpcError::IdempotencyRequired)?;
        let expired_outcome =
            PersistedOutcome::Failure(ApplicationFailure::from(&ApplicationError::OutcomeUnknown));
        let expired_response_json = serde_json::to_string(&expired_outcome)
            .map_err(|_| DispatchError::StoredResponseInvalid)?;
        let reservation = self.repository.reserve_request_with_expired_completion(
            IpcRequestSpec::new(
                request.request_id().clone(),
                request.command().descriptor().name(),
                authorized.principal().sid_hash(),
                idempotency_key,
                payload_digest.clone(),
                authorized.accepted_at_ms(),
                request.expires_at_ms(),
            ),
            IpcCompletionSpec::failed_terminal(expired_response_json, authorized.accepted_at_ms()),
        )?;

        match reservation {
            IpcLedgerReservation::New => {
                let outcome = match self.use_cases.execute(ApplicationRequest {
                    command: request.command(),
                    payload: request.payload().clone(),
                }) {
                    Ok(response) => PersistedOutcome::Success(response),
                    Err(error) => PersistedOutcome::Failure(ApplicationFailure::from(&error)),
                };
                let response_json = serde_json::to_string(&outcome)
                    .map_err(|_| DispatchError::StoredResponseInvalid)?;
                let completion_at_ms = unix_time_ms();
                let completion = match &outcome {
                    PersistedOutcome::Success(response) => IpcCompletionSpec::succeeded(
                        response_json,
                        response.resource.as_ref().map(|resource| {
                            (resource.resource_type.clone(), resource.resource_id.clone())
                        }),
                        completion_at_ms,
                    ),
                    PersistedOutcome::Failure(_) => {
                        IpcCompletionSpec::failed_terminal(response_json, completion_at_ms)
                    }
                };
                self.repository.complete_request(
                    request.request_id(),
                    &payload_digest,
                    completion,
                )?;
                outcome.into_response(request.request_id().clone(), false)
            }
            IpcLedgerReservation::Completed(stored) => {
                let outcome: PersistedOutcome = serde_json::from_str(stored.response_json())
                    .map_err(|_| DispatchError::StoredResponseInvalid)?;
                if stored.succeeded() != matches!(outcome, PersistedOutcome::Success(_)) {
                    return Err(DispatchError::StoredResponseInvalid);
                }
                outcome.into_response(request.request_id().clone(), true)
            }
        }
    }

    fn current_authority(&self) -> Result<AuthoritySnapshot, DispatchError> {
        let persisted = self.repository.persisted_authority()?;
        if persisted.execution_authority() == PersistedExecutionAuthority::Service {
            return Ok(AuthoritySnapshot::new(
                ExecutionAuthority::Service,
                match persisted.cutover_mode() {
                    PersistedCutoverMode::Production => CutoverMode::Production,
                    PersistedCutoverMode::RehearsalOnly => CutoverMode::RehearsalOnly,
                },
                persisted.g1_proven(),
                persisted.cutover_marker().is_some(),
            ));
        }
        if self.authority.execution_authority() == ExecutionAuthority::Service {
            return Ok(AuthoritySnapshot::new(
                ExecutionAuthority::LegacyDirect,
                CutoverMode::RehearsalOnly,
                false,
                false,
            ));
        }
        Ok(self.authority)
    }

    fn set_retention_control(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input =
            match serde_json::from_value::<RetentionControlSetPayload>(request.payload().clone()) {
                Ok(input)
                    if !input.destination_fingerprint.trim().is_empty()
                        && input.destination_fingerprint.len() <= 512
                        && input.destination_fingerprint.split_once(':').is_some() =>
                {
                    input
                }
                _ => return invalid_payload_response(request),
            };
        let recovery_point_id = match RecoveryPointId::from_str(&input.recovery_point_id) {
            Ok(recovery_point_id) => recovery_point_id,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(workspace_id) => workspace_id,
            Err(_) => return invalid_payload_response(request),
        };
        let payload_digest = canonical_payload_digest(request.payload())?;
        let idempotency_key = request
            .idempotency_key()
            .ok_or(IpcError::IdempotencyRequired)?;
        let response = ApplicationResponse {
            deduplicated: false,
            resource: None,
            body: json!({
                "schemaVersion": 1,
                "recoveryPointId": recovery_point_id.as_str(),
                "destinationFingerprint": input.destination_fingerprint,
                "pinned": input.pinned,
                "legalHold": input.legal_hold,
                "updatedAtMs": authorized.accepted_at_ms(),
            }),
        };
        let outcome = PersistedOutcome::Success(response);
        let response_json =
            serde_json::to_string(&outcome).map_err(|_| DispatchError::StoredResponseInvalid)?;
        let result = self.repository.set_retention_safety_control_in_workspace(
            &workspace_id,
            IpcRequestSpec::new(
                request.request_id().clone(),
                request.command().descriptor().name(),
                authorized.principal().sid_hash(),
                idempotency_key,
                payload_digest,
                authorized.accepted_at_ms(),
                request.expires_at_ms(),
            ),
            RetentionSafetyControlSpec::new(
                recovery_point_id,
                input.destination_fingerprint,
                input.pinned,
                input.legal_hold,
                authorized.accepted_at_ms(),
            ),
            IpcCompletionSpec::succeeded(response_json, None, authorized.accepted_at_ms()),
        )?;
        let stored = result.outcome();
        let stored_outcome: PersistedOutcome = serde_json::from_str(stored.response_json())
            .map_err(|_| DispatchError::StoredResponseInvalid)?;
        if !stored.succeeded() || !matches!(stored_outcome, PersistedOutcome::Success(_)) {
            return Err(DispatchError::StoredResponseInvalid);
        }
        stored_outcome.into_response(request.request_id().clone(), !result.created())
    }

    fn stage_retention_tombstone(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input =
            match serde_json::from_value::<RetentionTombstonePayload>(request.payload().clone()) {
                Ok(input)
                    if !input.destination_fingerprint.trim().is_empty()
                        && input.destination_fingerprint.len() <= 512
                        && input.destination_fingerprint.split_once(':').is_some()
                        && input.grace_period_ms > 0 =>
                {
                    input
                }
                _ => return invalid_payload_response(request),
            };
        let recovery_point_id = match RecoveryPointId::from_str(&input.recovery_point_id) {
            Ok(recovery_point_id) => recovery_point_id,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(workspace_id) => workspace_id,
            Err(_) => return invalid_payload_response(request),
        };
        let eligible_after_ms = match authorized
            .accepted_at_ms()
            .checked_add(input.grace_period_ms)
        {
            Some(value) => value,
            None => return invalid_payload_response(request),
        };
        let operation_run_id = OperationRunId::new();
        let payload_digest = canonical_payload_digest(request.payload())?;
        let idempotency_key = request
            .idempotency_key()
            .ok_or(IpcError::IdempotencyRequired)?;
        let response = ApplicationResponse {
            deduplicated: false,
            resource: Some(ResourceRef {
                resource_type: "operation_run".to_owned(),
                resource_id: operation_run_id.as_str().to_owned(),
            }),
            body: json!({
                "schemaVersion": 1,
                "operationRunId": operation_run_id.as_str(),
                "recoveryPointId": recovery_point_id.as_str(),
                "destinationFingerprint": input.destination_fingerprint,
                "gracePeriodMs": input.grace_period_ms,
                "tombstonedAtMs": authorized.accepted_at_ms(),
                "eligibleAfterMs": eligible_after_ms,
            }),
        };
        let outcome = PersistedOutcome::Success(response);
        let response_json =
            serde_json::to_string(&outcome).map_err(|_| DispatchError::StoredResponseInvalid)?;
        let result = self
            .repository
            .stage_retention_tombstone_request_in_workspace(
                &workspace_id,
                IpcRequestSpec::new(
                    request.request_id().clone(),
                    request.command().descriptor().name(),
                    authorized.principal().sid_hash(),
                    idempotency_key,
                    payload_digest,
                    authorized.accepted_at_ms(),
                    request.expires_at_ms(),
                ),
                RetentionTombstoneSpec::new(
                    operation_run_id.clone(),
                    recovery_point_id,
                    input.destination_fingerprint,
                    input.grace_period_ms,
                    authorized.accepted_at_ms(),
                ),
                IpcCompletionSpec::succeeded(
                    response_json,
                    Some((
                        "operation_run".to_owned(),
                        operation_run_id.as_str().to_owned(),
                    )),
                    authorized.accepted_at_ms(),
                ),
            )?;
        let stored = result.outcome();
        let stored_operation_run_id = stored
            .result_ref_id()
            .filter(|_| stored.result_ref_type() == Some("operation_run"))
            .ok_or(DispatchError::StoredResponseInvalid)?;
        OperationRunId::from_str(stored_operation_run_id)
            .map_err(|_| DispatchError::StoredResponseInvalid)?;
        let stored_outcome: PersistedOutcome = serde_json::from_str(stored.response_json())
            .map_err(|_| DispatchError::StoredResponseInvalid)?;
        let response_matches_resource = matches!(
            &stored_outcome,
            PersistedOutcome::Success(response)
                if response.resource.as_ref().is_some_and(|resource| {
                    resource.resource_type == "operation_run"
                        && resource.resource_id == stored_operation_run_id
                        && response.body.get("operationRunId").and_then(serde_json::Value::as_str)
                            == Some(stored_operation_run_id)
                })
        );
        if !stored.succeeded() || !response_matches_resource {
            return Err(DispatchError::StoredResponseInvalid);
        }
        stored_outcome.into_response(request.request_id().clone(), !result.created())
    }

    fn enqueue_safe_restore(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input =
            match serde_json::from_value::<SafeRestoreEnqueuePayload>(request.payload().clone()) {
                Ok(input)
                    if (1..=3).contains(&input.required_verification_level)
                        && matches!(
                            input.purpose.as_str(),
                            "incident_recovery" | "restore_drill"
                        )
                        && (input.purpose != "restore_drill"
                            || input.required_verification_level == 3) =>
                {
                    input
                }
                _ => return invalid_payload_response(request),
            };
        let purpose = match input.purpose.as_str() {
            "incident_recovery" => RestorePurpose::IncidentRecovery,
            "restore_drill" => RestorePurpose::RestoreDrill,
            _ => return invalid_payload_response(request),
        };
        let recovery_point_id = match RecoveryPointId::from_str(&input.snapshot_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let target_connection_id = match ConnectionProfileId::from_str(&input.target_connection_id)
        {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        self.repository
            .require_workspace_recovery_point(&workspace_id, &recovery_point_id)?;
        self.repository
            .require_workspace_connection(&workspace_id, &target_connection_id)?;
        let payload_digest = canonical_payload_digest(request.payload())?;
        let idempotency_key = request
            .idempotency_key()
            .ok_or(IpcError::IdempotencyRequired)?;
        let ipc_request = IpcRequestSpec::new(
            request.request_id().clone(),
            request.command().descriptor().name(),
            authorized.principal().sid_hash(),
            idempotency_key,
            payload_digest,
            authorized.accepted_at_ms(),
            request.expires_at_ms(),
        );
        if let Some(stored) = self.repository.completed_request(&ipc_request)? {
            let outcome: PersistedOutcome = serde_json::from_str(stored.response_json())
                .map_err(|_| DispatchError::StoredResponseInvalid)?;
            if !stored.succeeded() || !matches!(outcome, PersistedOutcome::Success(_)) {
                return Err(DispatchError::StoredResponseInvalid);
            }
            return outcome.into_response(request.request_id().clone(), true);
        }
        let mut prepared = match self.use_cases.execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "workspaceId": workspace_id.as_str(),
                "snapshotId": recovery_point_id.as_str(),
                "mode": "create_new",
                "purpose": purpose.as_str(),
                "targetConnectionId": target_connection_id.as_str(),
                "targetName": input.target_name.as_str(),
                "sourceReplicaId": input.source_replica_id.as_str(),
            }),
        }) {
            Ok(response) => response.body,
            Err(error) => {
                return ResponseEnvelope::failure(
                    request.request_id().clone(),
                    ApplicationFailure::from(&error),
                )
                .map_err(DispatchError::from)
            }
        };
        if prepared
            .get("eligible")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
            || prepared
                .get("preflightVersion")
                .and_then(serde_json::Value::as_str)
                != Some(input.preflight_version.as_str())
            || prepared.get("purpose").and_then(serde_json::Value::as_str) != Some(purpose.as_str())
        {
            return invalid_payload_response(request);
        }
        let target_fingerprint = prepared
            .get("targetFingerprint")
            .and_then(serde_json::Value::as_str)
            .ok_or(DispatchError::StoredResponseInvalid)?
            .to_owned();
        let source_replica_id = prepared
            .pointer("/source/replicaId")
            .and_then(serde_json::Value::as_str)
            .ok_or(DispatchError::StoredResponseInvalid)?
            .to_owned();
        prepared["requiredVerificationLevel"] = json!(input.required_verification_level);
        let eligibility_snapshot_json =
            serde_json::to_string(&prepared).map_err(|_| DispatchError::StoredResponseInvalid)?;
        let restore_run_id = RestoreRunId::new();
        let work_item_id = WorkItemId::new();
        let restore_spec = SafeRestoreSpec::new(
            restore_run_id.clone(),
            work_item_id.clone(),
            recovery_point_id,
            source_replica_id,
            target_connection_id,
            input.target_name,
            target_fingerprint.clone(),
            input.preflight_version.clone(),
            eligibility_snapshot_json,
            authorized.accepted_at_ms(),
        )
        .with_purpose(purpose);
        let verification_run_id = restore_spec.verification_run_id();
        let response = ApplicationResponse {
            deduplicated: false,
            resource: Some(ResourceRef {
                resource_type: "restore_run".to_owned(),
                resource_id: restore_run_id.as_str().to_owned(),
            }),
            body: json!({
                "restoreRunId": restore_run_id.as_str(),
                "workItemId": work_item_id.as_str(),
                "state": "queued",
                "mode": "create_new",
                "purpose": purpose.as_str(),
                "verificationRunId": verification_run_id.as_ref().map(|id| id.as_str()),
                "targetFingerprint": target_fingerprint.as_str(),
                "preflightVersion": input.preflight_version.as_str(),
            }),
        };
        let outcome = PersistedOutcome::Success(response);
        let response_json =
            serde_json::to_string(&outcome).map_err(|_| DispatchError::StoredResponseInvalid)?;
        let result = self.repository.enqueue_safe_restore_request(
            ipc_request,
            restore_spec,
            IpcCompletionSpec::succeeded(
                response_json,
                Some(("restore_run".to_owned(), restore_run_id.as_str().to_owned())),
                authorized.accepted_at_ms(),
            ),
        )?;
        let stored = result.outcome();
        let stored_outcome: PersistedOutcome = serde_json::from_str(stored.response_json())
            .map_err(|_| DispatchError::StoredResponseInvalid)?;
        let response_matches = matches!(
            &stored_outcome,
            PersistedOutcome::Success(response)
                if response.resource.as_ref().is_some_and(|resource| {
                    resource.resource_type == "restore_run"
                        && resource.resource_id
                            == stored.result_ref_id().unwrap_or_default()
                        && response.body.get("restoreRunId").and_then(serde_json::Value::as_str)
                            == stored.result_ref_id()
                        && response.body.get("workItemId").and_then(serde_json::Value::as_str)
                            .and_then(|value| WorkItemId::from_str(value).ok())
                            .is_some()
                })
        );
        if !stored.succeeded()
            || stored.result_ref_type() != Some("restore_run")
            || !response_matches
        {
            return Err(DispatchError::StoredResponseInvalid);
        }
        stored_outcome.into_response(request.request_id().clone(), !result.created())
    }

    fn enqueue_overwrite_restore(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input = match serde_json::from_value::<RestoreOverwriteEnqueuePayload>(
            request.payload().clone(),
        ) {
            Ok(input)
                if input.confirmed_target_name == input.target_name
                    && (1..=3).contains(&input.required_verification_level) =>
            {
                input
            }
            _ => return invalid_payload_response(request),
        };
        let recovery_point_id = match RecoveryPointId::from_str(&input.snapshot_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let target_connection_id = match ConnectionProfileId::from_str(&input.target_connection_id)
        {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        self.repository
            .require_workspace_recovery_point(&workspace_id, &recovery_point_id)?;
        self.repository
            .require_workspace_connection(&workspace_id, &target_connection_id)?;
        let confirmation_token_id =
            match ConfirmationTokenId::from_str(&input.confirmation_token_id) {
                Ok(value) => value,
                Err(_) => return invalid_payload_response(request),
            };
        let raw_token = RawConfirmationToken::from_encoded(&input.confirmation_token)?;
        let payload_digest = canonical_payload_digest(request.payload())?;
        let idempotency_key = request
            .idempotency_key()
            .ok_or(IpcError::IdempotencyRequired)?;
        let ipc_request = IpcRequestSpec::new(
            request.request_id().clone(),
            request.command().descriptor().name(),
            authorized.principal().sid_hash(),
            idempotency_key,
            payload_digest,
            authorized.accepted_at_ms(),
            request.expires_at_ms(),
        );
        if let Some(stored) = self.repository.completed_request(&ipc_request)? {
            let outcome: PersistedOutcome = serde_json::from_str(stored.response_json())
                .map_err(|_| DispatchError::StoredResponseInvalid)?;
            if !stored.succeeded() || !matches!(outcome, PersistedOutcome::Success(_)) {
                return Err(DispatchError::StoredResponseInvalid);
            }
            return outcome.into_response(request.request_id().clone(), true);
        }
        let mut prepared = match self.use_cases.execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "workspaceId": workspace_id.as_str(),
                "snapshotId": recovery_point_id.as_str(),
                "mode": "overwrite",
                "purpose": "incident_recovery",
                "targetConnectionId": target_connection_id.as_str(),
                "targetName": input.target_name.as_str(),
                "sourceReplicaId": input.source_replica_id.as_str(),
            }),
        }) {
            Ok(response) => response.body,
            Err(error) => {
                return ResponseEnvelope::failure(
                    request.request_id().clone(),
                    ApplicationFailure::from(&error),
                )
                .map_err(DispatchError::from)
            }
        };
        if prepared
            .get("eligible")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
            || prepared.get("mode").and_then(serde_json::Value::as_str) != Some("overwrite")
            || prepared
                .get("targetExists")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            || prepared
                .get("preflightVersion")
                .and_then(serde_json::Value::as_str)
                != Some(input.preflight_version.as_str())
        {
            return invalid_payload_response(request);
        }
        let target_fingerprint = prepared
            .get("targetFingerprint")
            .and_then(serde_json::Value::as_str)
            .ok_or(DispatchError::StoredResponseInvalid)?
            .to_owned();
        let source_replica_id = prepared
            .pointer("/source/replicaId")
            .and_then(serde_json::Value::as_str)
            .ok_or(DispatchError::StoredResponseInvalid)?
            .to_owned();
        if source_replica_id != input.source_replica_id {
            return invalid_payload_response(request);
        }
        prepared["requiredVerificationLevel"] = json!(input.required_verification_level);
        prepared["confirmationTokenId"] = json!(confirmation_token_id.as_str());
        let eligibility_snapshot_json =
            serde_json::to_string(&prepared).map_err(|_| DispatchError::StoredResponseInvalid)?;
        let restore_run_id = RestoreRunId::new();
        let work_item_id = WorkItemId::new();
        let response = ApplicationResponse {
            deduplicated: false,
            resource: Some(ResourceRef {
                resource_type: "restore_run".to_owned(),
                resource_id: restore_run_id.as_str().to_owned(),
            }),
            body: json!({
                "restoreRunId": restore_run_id.as_str(),
                "workItemId": work_item_id.as_str(),
                "state": "queued",
                "mode": "overwrite",
                "targetFingerprint": target_fingerprint.as_str(),
                "preflightVersion": input.preflight_version.as_str(),
                "cleanupPolicy": "preserve_target_and_require_explicit_cleanup",
            }),
        };
        let outcome = PersistedOutcome::Success(response);
        let response_json =
            serde_json::to_string(&outcome).map_err(|_| DispatchError::StoredResponseInvalid)?;
        let result = self.repository.enqueue_overwrite_restore(
            ipc_request,
            &confirmation_token_id,
            &raw_token.digest_blake3(),
            OverwriteRestoreSpec::new(
                restore_run_id.clone(),
                work_item_id,
                recovery_point_id,
                source_replica_id,
                target_connection_id,
                input.target_name,
                target_fingerprint,
                input.preflight_version,
                input.required_verification_level,
                eligibility_snapshot_json,
                authorized.accepted_at_ms(),
            ),
            IpcCompletionSpec::succeeded(
                response_json,
                Some(("restore_run".to_owned(), restore_run_id.as_str().to_owned())),
                authorized.accepted_at_ms(),
            ),
        )?;
        let stored = result.outcome();
        let stored_outcome: PersistedOutcome = serde_json::from_str(stored.response_json())
            .map_err(|_| DispatchError::StoredResponseInvalid)?;
        let response_matches = matches!(
            &stored_outcome,
            PersistedOutcome::Success(response)
                if response.resource.as_ref().is_some_and(|resource| {
                    resource.resource_type == "restore_run"
                        && resource.resource_id
                            == stored.result_ref_id().unwrap_or_default()
                        && response.body.get("restoreRunId").and_then(serde_json::Value::as_str)
                            == stored.result_ref_id()
                        && response.body.get("workItemId").and_then(serde_json::Value::as_str)
                            .and_then(|value| WorkItemId::from_str(value).ok())
                            .is_some()
                })
        );
        if !stored.succeeded()
            || stored.result_ref_type() != Some("restore_run")
            || !response_matches
        {
            return Err(DispatchError::StoredResponseInvalid);
        }
        stored_outcome.into_response(request.request_id().clone(), !result.created())
    }

    fn enqueue_replica_repair(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input = match serde_json::from_value::<ReplicaRepairEnqueuePayload>(
            request.payload().clone(),
        ) {
            Ok(input) => input,
            Err(_) => return invalid_payload_response(request),
        };
        let recovery_point_id = match RecoveryPointId::from_str(&input.snapshot_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let source_replica_id = match ArtifactReplicaId::from_str(&input.source_replica_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let target_destination_id = match DestinationId::from_str(&input.target_destination_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        self.repository
            .require_workspace_recovery_point(&workspace_id, &recovery_point_id)?;
        self.repository
            .require_workspace_destination(&workspace_id, &target_destination_id)?;
        let result = self.repository.enqueue_replica_repair_request(
            IpcRequestSpec::new(
                request.request_id().clone(),
                request.command().descriptor().name(),
                authorized.principal().sid_hash(),
                request
                    .idempotency_key()
                    .ok_or(IpcError::IdempotencyRequired)?,
                canonical_payload_digest(request.payload())?,
                authorized.accepted_at_ms(),
                request.expires_at_ms(),
            ),
            ReplicaRepairRequestSpec::new(
                recovery_point_id,
                source_replica_id,
                target_destination_id,
                authorized.accepted_at_ms(),
            ),
        )?;
        PersistedOutcome::Success(replica_repair_response(&result))
            .into_response(request.request_id().clone(), !result.created())
    }

    fn enqueue_manual_verification(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input = match serde_json::from_value::<ManualVerificationEnqueuePayload>(
            request.payload().clone(),
        ) {
            Ok(input) if (1..=3).contains(&input.level) && input.purpose == "manual" => input,
            _ => return invalid_payload_response(request),
        };
        let recovery_point_id = match RecoveryPointId::from_str(&input.snapshot_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        self.repository
            .require_workspace_recovery_point(&workspace_id, &recovery_point_id)?;
        let source_replica_id = match input
            .source_replica_id
            .as_deref()
            .map(ArtifactReplicaId::from_str)
            .transpose()
        {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let result = self.repository.enqueue_manual_verification_request(
            IpcRequestSpec::new(
                request.request_id().clone(),
                request.command().descriptor().name(),
                authorized.principal().sid_hash(),
                request
                    .idempotency_key()
                    .ok_or(IpcError::IdempotencyRequired)?,
                canonical_payload_digest(request.payload())?,
                authorized.accepted_at_ms(),
                request.expires_at_ms(),
            ),
            ManualVerificationRequestSpec::new(
                VerificationRunId::new(),
                WorkItemId::new(),
                recovery_point_id,
                source_replica_id,
                input.level,
                authorized.accepted_at_ms(),
            ),
        )?;
        PersistedOutcome::Success(manual_verification_response(&result))
            .into_response(request.request_id().clone(), !result.created())
    }

    fn enqueue_manual_backup(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input =
            match serde_json::from_value::<ManualBackupEnqueuePayload>(request.payload().clone()) {
                Ok(input) => input,
                Err(_) => return invalid_payload_response(request),
            };
        let plan_id = match BackupPlanId::from_str(&input.plan_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let result = self.repository.enqueue_manual_backup_request(
            IpcRequestSpec::new(
                request.request_id().clone(),
                request.command().descriptor().name(),
                authorized.principal().sid_hash(),
                request
                    .idempotency_key()
                    .ok_or(IpcError::IdempotencyRequired)?,
                canonical_payload_digest(request.payload())?,
                authorized.accepted_at_ms(),
                request.expires_at_ms(),
            ),
            ManualBackupRequestSpec::new(workspace_id, plan_id, authorized.accepted_at_ms()),
        )?;
        PersistedOutcome::Success(manual_backup_response(&result))
            .into_response(request.request_id().clone(), !result.created())
    }

    fn prepare_confirmation(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let action = match serde_json::from_value::<ConfirmationPrepareActionPayload>(
            authorized.request().payload().clone(),
        ) {
            Ok(input) => input.action,
            Err(_) => return invalid_payload_response(authorized.request()),
        };
        match action {
            ConfirmationAction::RetentionDelete => self.prepare_retention_confirmation(authorized),
            ConfirmationAction::RestoreOverwrite => {
                self.prepare_restore_overwrite_confirmation(authorized)
            }
            ConfirmationAction::ConfigCutover => self.prepare_config_cutover(authorized),
            _ => invalid_payload_response(authorized.request()),
        }
    }

    fn prepare_retention_confirmation(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input =
            match serde_json::from_value::<ConfirmationPreparePayload>(request.payload().clone()) {
                Ok(input)
                    if input.action == ConfirmationAction::RetentionDelete
                        && !input.destination_fingerprint.trim().is_empty()
                        && input.destination_fingerprint.len() <= 512
                        && input.destination_fingerprint.split_once(':').is_some()
                        && input.grace_period_ms > 0
                        && input.ttl_ms > 0
                        && input.ttl_ms <= MAX_CONFIRMATION_TTL_MS =>
                {
                    input
                }
                _ => return invalid_payload_response(request),
            };
        let recovery_point_id = match RecoveryPointId::from_str(&input.recovery_point_id) {
            Ok(recovery_point_id) => recovery_point_id,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(workspace_id) => workspace_id,
            Err(_) => return invalid_payload_response(request),
        };
        let expires_at_ms = match authorized.accepted_at_ms().checked_add(input.ttl_ms) {
            Some(value) => value,
            None => return invalid_payload_response(request),
        };
        let confirmation_token_id = ConfirmationTokenId::new();
        let raw_token = RawConfirmationToken::generate()?;
        let token_digest_blake3 = raw_token.digest_blake3();
        let request_payload_digest = canonical_payload_digest(request.payload())?;
        let result = self
            .repository
            .issue_retention_confirmation_request_in_workspace(
                &workspace_id,
                IpcRequestSpec::new(
                    request.request_id().clone(),
                    request.command().descriptor().name(),
                    authorized.principal().sid_hash(),
                    request
                        .idempotency_key()
                        .ok_or(IpcError::IdempotencyRequired)?,
                    request_payload_digest,
                    authorized.accepted_at_ms(),
                    request.expires_at_ms(),
                ),
                RetentionConfirmationRequestSpec::new(
                    confirmation_token_id.clone(),
                    token_digest_blake3,
                    recovery_point_id,
                    input.destination_fingerprint,
                    input.grace_period_ms,
                    expires_at_ms,
                ),
            )?;
        if result.created() && result.confirmation_token_id() != &confirmation_token_id {
            return Err(DispatchError::StoredResponseInvalid);
        }
        let encoded_token = result.created().then(|| raw_token.encode());
        PersistedOutcome::Success(confirmation_response(
            &result,
            input.action,
            encoded_token.as_ref().map(|value| value.as_str()),
        )?)
        .into_response(request.request_id().clone(), !result.created())
    }

    fn prepare_restore_overwrite_confirmation(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input = match serde_json::from_value::<RestoreOverwriteConfirmationPreparePayload>(
            request.payload().clone(),
        ) {
            Ok(input)
                if input.action == ConfirmationAction::RestoreOverwrite
                    && input.confirmed_target_name == input.target_name
                    && (1..=3).contains(&input.required_verification_level)
                    && input.ttl_ms > 0
                    && input.ttl_ms <= MAX_CONFIRMATION_TTL_MS =>
            {
                input
            }
            _ => return invalid_payload_response(request),
        };
        let recovery_point_id = match RecoveryPointId::from_str(&input.snapshot_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let target_connection_id = match ConnectionProfileId::from_str(&input.target_connection_id)
        {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        self.repository
            .require_workspace_recovery_point(&workspace_id, &recovery_point_id)?;
        self.repository
            .require_workspace_connection(&workspace_id, &target_connection_id)?;
        let expires_at_ms = match authorized.accepted_at_ms().checked_add(input.ttl_ms) {
            Some(value) => value,
            None => return invalid_payload_response(request),
        };
        let prepared = match self.use_cases.execute(ApplicationRequest {
            command: ApplicationCommand::RestorePreflight,
            payload: json!({
                "workspaceId": workspace_id.as_str(),
                "snapshotId": recovery_point_id.as_str(),
                "mode": "overwrite",
                "purpose": "incident_recovery",
                "targetConnectionId": target_connection_id.as_str(),
                "targetName": input.target_name.as_str(),
                "sourceReplicaId": input.source_replica_id.as_str(),
            }),
        }) {
            Ok(response) => response.body,
            Err(error) => {
                return ResponseEnvelope::failure(
                    request.request_id().clone(),
                    ApplicationFailure::from(&error),
                )
                .map_err(DispatchError::from)
            }
        };
        if prepared
            .get("eligible")
            .and_then(serde_json::Value::as_bool)
            != Some(true)
            || prepared.get("mode").and_then(serde_json::Value::as_str) != Some("overwrite")
            || prepared
                .get("targetExists")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            || prepared
                .get("preflightVersion")
                .and_then(serde_json::Value::as_str)
                != Some(input.preflight_version.as_str())
        {
            return invalid_payload_response(request);
        }
        let target_fingerprint = prepared
            .get("targetFingerprint")
            .and_then(serde_json::Value::as_str)
            .ok_or(DispatchError::StoredResponseInvalid)?
            .to_owned();
        let source_replica_id = prepared
            .pointer("/source/replicaId")
            .and_then(serde_json::Value::as_str)
            .ok_or(DispatchError::StoredResponseInvalid)?
            .to_owned();
        if source_replica_id != input.source_replica_id {
            return invalid_payload_response(request);
        }

        let confirmation_token_id = ConfirmationTokenId::new();
        let raw_token = RawConfirmationToken::generate()?;
        let token_digest_blake3 = raw_token.digest_blake3();
        let encoded_token = raw_token.encode();
        let overwrite_payload = restore_overwrite_request_payload(
            &input,
            &confirmation_token_id,
            encoded_token.as_str(),
        );
        let overwrite_payload_digest = canonical_payload_digest(&overwrite_payload)?;
        let prepare_payload_digest = canonical_payload_digest(request.payload())?;
        let result = self
            .repository
            .issue_restore_overwrite_confirmation_request(
                IpcRequestSpec::new(
                    request.request_id().clone(),
                    request.command().descriptor().name(),
                    authorized.principal().sid_hash(),
                    request
                        .idempotency_key()
                        .ok_or(IpcError::IdempotencyRequired)?,
                    prepare_payload_digest,
                    authorized.accepted_at_ms(),
                    request.expires_at_ms(),
                ),
                RestoreOverwriteConfirmationRequestSpec::new(
                    confirmation_token_id.clone(),
                    token_digest_blake3,
                    recovery_point_id.clone(),
                    source_replica_id,
                    target_connection_id.clone(),
                    input.target_name.clone(),
                    target_fingerprint.clone(),
                    input.preflight_version.clone(),
                    input.required_verification_level,
                    overwrite_payload_digest.clone(),
                    expires_at_ms,
                ),
            )?;
        if result.recovery_point_id() != &recovery_point_id
            || result.target_connection_profile_id() != &target_connection_id
            || result.target_database_name() != input.target_name
            || result.target_fingerprint() != target_fingerprint
            || result.preflight_version() != input.preflight_version
            || result.required_verification_level() != input.required_verification_level
            || (result.created()
                && (result.confirmation_token_id() != &confirmation_token_id
                    || result.payload_digest_blake3() != overwrite_payload_digest
                    || result.expires_at_ms() != expires_at_ms))
        {
            return Err(DispatchError::StoredResponseInvalid);
        }
        let response = restore_overwrite_confirmation_response(
            &result,
            result.created().then_some(encoded_token.as_str()),
            result.created().then_some(overwrite_payload),
        );
        PersistedOutcome::Success(response)
            .into_response(request.request_id().clone(), !result.created())
    }

    fn prepare_config_cutover(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input = match serde_json::from_value::<ConfigCutoverConfirmationPreparePayload>(
            request.payload().clone(),
        ) {
            Ok(input)
                if input.action == ConfirmationAction::ConfigCutover
                    && input.ttl_ms > 0
                    && input.ttl_ms <= MAX_CONFIRMATION_TTL_MS =>
            {
                input
            }
            _ => return invalid_payload_response(request),
        };
        let cutover_run_id = match canonical_uuid(&input.cutover_run_id) {
            Some(value) => value,
            None => return invalid_payload_response(request),
        };
        let shadow_import_id = match canonical_uuid(&input.shadow_import_id) {
            Some(value) => value,
            None => return invalid_payload_response(request),
        };
        let expires_at_ms = match authorized.accepted_at_ms().checked_add(input.ttl_ms) {
            Some(value) => value,
            None => return invalid_payload_response(request),
        };
        let verifier = self
            .cutover_verifier
            .as_ref()
            .ok_or(DispatchError::TypedTransactionRequired)?;
        let readiness_input = input.readiness_input(shadow_import_id);
        let readiness = verifier.verify(&readiness_input, authorized.accepted_at_ms())?;
        let cutover = readiness.config_cutover_spec(cutover_run_id, authorized.accepted_at_ms());
        let confirmation_token_id = ConfirmationTokenId::new();
        let raw_token = RawConfirmationToken::generate()?;
        let token_digest_blake3 = raw_token.digest_blake3();
        let encoded_token = raw_token.encode();
        let cutover_payload = config_cutover_request_payload(
            &input,
            &confirmation_token_id,
            encoded_token.as_str(),
            cutover.cutover_marker(),
        );
        let cutover_payload_digest = canonical_payload_digest(&cutover_payload)?;
        let prepare_payload_digest = canonical_payload_digest(request.payload())?;
        let result = self.repository.issue_config_cutover_confirmation_request(
            IpcRequestSpec::new(
                request.request_id().clone(),
                request.command().descriptor().name(),
                authorized.principal().sid_hash(),
                request
                    .idempotency_key()
                    .ok_or(IpcError::IdempotencyRequired)?,
                prepare_payload_digest,
                authorized.accepted_at_ms(),
                request.expires_at_ms(),
            ),
            ConfigCutoverConfirmationRequestSpec::new(
                confirmation_token_id.clone(),
                token_digest_blake3,
                cutover_run_id,
                shadow_import_id,
                cutover.cutover_marker(),
                cutover_payload_digest.clone(),
                expires_at_ms,
            ),
        )?;
        if result.cutover_run_id() != cutover_run_id
            || result.shadow_import_id() != shadow_import_id
            || result.cutover_marker() != cutover.cutover_marker()
            || (result.created()
                && (result.confirmation_token_id() != &confirmation_token_id
                    || result.payload_digest_blake3() != cutover_payload_digest
                    || result.expires_at_ms() != expires_at_ms))
        {
            return Err(DispatchError::StoredResponseInvalid);
        }
        let response = config_cutover_confirmation_response(
            &result,
            result.created().then_some(encoded_token.as_str()),
            result.created().then_some(cutover_payload),
        );
        PersistedOutcome::Success(response)
            .into_response(request.request_id().clone(), !result.created())
    }

    fn commit_config_cutover(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input = match serde_json::from_value::<ConfigCutoverPayload>(request.payload().clone())
        {
            Ok(input) => input,
            Err(_) => return invalid_payload_response(request),
        };
        let cutover_run_id = match canonical_uuid(&input.cutover_run_id) {
            Some(value) => value,
            None => return invalid_payload_response(request),
        };
        let shadow_import_id = match canonical_uuid(&input.shadow_import_id) {
            Some(value) => value,
            None => return invalid_payload_response(request),
        };
        let confirmation_token_id =
            match ConfirmationTokenId::from_str(&input.confirmation_token_id) {
                Ok(value) => value,
                Err(_) => return invalid_payload_response(request),
            };
        let raw_token = RawConfirmationToken::from_encoded(&input.confirmation_token)?;
        let verifier = self
            .cutover_verifier
            .as_ref()
            .ok_or(DispatchError::TypedTransactionRequired)?;
        let readiness_input = input.readiness_input(shadow_import_id);
        let readiness = verifier.verify(&readiness_input, authorized.accepted_at_ms())?;
        let cutover = readiness.config_cutover_spec(cutover_run_id, authorized.accepted_at_ms());
        if cutover.cutover_marker() != input.cutover_marker {
            return Err(CutoverReadinessError::Invalid.into());
        }
        let payload_digest = canonical_payload_digest(request.payload())?;
        let result = self.repository.commit_config_cutover(
            IpcRequestSpec::new(
                request.request_id().clone(),
                request.command().descriptor().name(),
                authorized.principal().sid_hash(),
                request
                    .idempotency_key()
                    .ok_or(IpcError::IdempotencyRequired)?,
                payload_digest,
                authorized.accepted_at_ms(),
                request.expires_at_ms(),
            ),
            &confirmation_token_id,
            &raw_token.digest_blake3(),
            cutover,
        )?;
        if result.cutover_marker() != input.cutover_marker {
            return Err(DispatchError::StoredResponseInvalid);
        }
        PersistedOutcome::Success(ApplicationResponse {
            deduplicated: false,
            resource: Some(ResourceRef {
                resource_type: "config_cutover".to_owned(),
                resource_id: result.cutover_marker().to_owned(),
            }),
            body: json!({
                "schemaVersion": 1,
                "cutoverMarker": result.cutover_marker(),
                "cutoverMode": "production",
                "executionAuthority": "service",
            }),
        })
        .into_response(request.request_id().clone(), !result.created())
    }

    fn enqueue_retention_delete(
        &self,
        authorized: &AuthorizedRequest,
    ) -> Result<ResponseEnvelope, DispatchError> {
        let request = authorized.request();
        let input =
            match serde_json::from_value::<RetentionDeletePayload>(request.payload().clone()) {
                Ok(input) => input,
                Err(_) => return invalid_payload_response(request),
            };
        let workspace_id = match WorkspaceId::from_str(&input.workspace_id) {
            Ok(workspace_id) => workspace_id,
            Err(_) => return invalid_payload_response(request),
        };
        let operation_run_id = match OperationRunId::from_str(&input.operation_run_id) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let confirmation_token_id =
            match ConfirmationTokenId::from_str(&input.confirmation_token_id) {
                Ok(value) => value,
                Err(_) => return invalid_payload_response(request),
            };
        let authorization_json = serde_json::to_string(&input.authorization)
            .map_err(|_| DispatchError::StoredResponseInvalid)?;
        let authorization = match RetentionDeleteAuthorizationV2::from_json(&authorization_json) {
            Ok(value) => value,
            Err(_) => return invalid_payload_response(request),
        };
        let raw_token = RawConfirmationToken::from_encoded(&input.confirmation_token)?;
        let token_digest_blake3 = raw_token.digest_blake3();
        let request_payload_digest = canonical_payload_digest(request.payload())?;
        let generated_work_item_id = WorkItemId::new();
        let result = self.repository.enqueue_retention_delete_in_workspace(
            &workspace_id,
            IpcRequestSpec::new(
                request.request_id().clone(),
                request.command().descriptor().name(),
                authorized.principal().sid_hash(),
                request
                    .idempotency_key()
                    .ok_or(IpcError::IdempotencyRequired)?,
                request_payload_digest,
                authorized.accepted_at_ms(),
                request.expires_at_ms(),
            ),
            &confirmation_token_id,
            &token_digest_blake3,
            RetentionDeleteSpec::new(
                operation_run_id.clone(),
                generated_work_item_id.clone(),
                authorization,
            ),
        )?;
        if result.created() && result.work_item_id() != &generated_work_item_id {
            return Err(DispatchError::StoredResponseInvalid);
        }
        PersistedOutcome::Success(retention_delete_response(&operation_run_id, &result))
            .into_response(request.request_id().clone(), !result.created())
    }

    fn invoke(
        &self,
        request: &super::RequestEnvelope,
        deduplicated: bool,
    ) -> Result<ResponseEnvelope, DispatchError> {
        match self.use_cases.execute(ApplicationRequest {
            command: request.command(),
            payload: request.payload().clone(),
        }) {
            Ok(mut response) => {
                response.deduplicated = deduplicated;
                ResponseEnvelope::success(request.request_id().clone(), response)
                    .map_err(DispatchError::from)
            }
            Err(error) => ResponseEnvelope::failure_with_deduplication(
                request.request_id().clone(),
                ApplicationFailure::from(&error),
                deduplicated,
            )
            .map_err(DispatchError::from),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmationPrepareActionPayload {
    action: ConfirmationAction,
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

fn default_restore_purpose() -> String {
    "incident_recovery".to_owned()
}

fn default_workspace_id_string() -> String {
    DEFAULT_WORKSPACE_ID.to_owned()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreOverwriteConfirmationPreparePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    action: ConfirmationAction,
    snapshot_id: String,
    source_replica_id: String,
    target_connection_id: String,
    target_name: String,
    confirmed_target_name: String,
    preflight_version: String,
    required_verification_level: i64,
    ttl_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreOverwriteEnqueuePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    confirmation_token_id: String,
    confirmation_token: String,
    snapshot_id: String,
    source_replica_id: String,
    target_connection_id: String,
    target_name: String,
    confirmed_target_name: String,
    preflight_version: String,
    required_verification_level: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplicaRepairEnqueuePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    snapshot_id: String,
    source_replica_id: String,
    target_destination_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManualVerificationEnqueuePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    snapshot_id: String,
    level: i64,
    purpose: String,
    #[serde(default)]
    source_replica_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManualBackupEnqueuePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    plan_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigCutoverConfirmationPreparePayload {
    action: ConfirmationAction,
    cutover_run_id: String,
    shadow_import_id: String,
    source_checksum_blake3: String,
    relation_digest_blake3: String,
    g1_evidence_path: String,
    recovery_bundle_manifest_path: String,
    freeze_receipt_path: String,
    ttl_ms: i64,
}

impl ConfigCutoverConfirmationPreparePayload {
    fn readiness_input(&self, shadow_import_id: Uuid) -> CutoverReadinessInputV1 {
        CutoverReadinessInputV1::new(
            shadow_import_id,
            self.source_checksum_blake3.clone(),
            self.relation_digest_blake3.clone(),
            self.g1_evidence_path.clone(),
            self.recovery_bundle_manifest_path.clone(),
            self.freeze_receipt_path.clone(),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigCutoverPayload {
    confirmation_token_id: String,
    confirmation_token: String,
    cutover_run_id: String,
    cutover_marker: String,
    shadow_import_id: String,
    source_checksum_blake3: String,
    relation_digest_blake3: String,
    g1_evidence_path: String,
    recovery_bundle_manifest_path: String,
    freeze_receipt_path: String,
}

impl ConfigCutoverPayload {
    fn readiness_input(&self, shadow_import_id: Uuid) -> CutoverReadinessInputV1 {
        CutoverReadinessInputV1::new(
            shadow_import_id,
            self.source_checksum_blake3.clone(),
            self.relation_digest_blake3.clone(),
            self.g1_evidence_path.clone(),
            self.recovery_bundle_manifest_path.clone(),
            self.freeze_receipt_path.clone(),
        )
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetentionControlSetPayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    recovery_point_id: String,
    destination_fingerprint: String,
    pinned: bool,
    legal_hold: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetentionTombstonePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    recovery_point_id: String,
    destination_fingerprint: String,
    grace_period_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfirmationPreparePayload {
    action: ConfirmationAction,
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    recovery_point_id: String,
    destination_fingerprint: String,
    grace_period_ms: i64,
    ttl_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetentionDeletePayload {
    #[serde(default = "default_workspace_id_string")]
    workspace_id: String,
    operation_run_id: String,
    confirmation_token_id: String,
    confirmation_token: String,
    authorization: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
enum PersistedOutcome {
    Success(ApplicationResponse),
    Failure(ApplicationFailure),
}

impl PersistedOutcome {
    fn into_response(
        self,
        request_id: dbbackup_domain::IpcRequestId,
        deduplicated: bool,
    ) -> Result<ResponseEnvelope, DispatchError> {
        match self {
            Self::Success(mut response) => {
                response.deduplicated = deduplicated;
                ResponseEnvelope::success(request_id, response).map_err(DispatchError::from)
            }
            Self::Failure(failure) => {
                ResponseEnvelope::failure_with_deduplication(request_id, failure, deduplicated)
                    .map_err(DispatchError::from)
            }
        }
    }
}

fn invalid_payload_response(
    request: &super::RequestEnvelope,
) -> Result<ResponseEnvelope, DispatchError> {
    ResponseEnvelope::failure(
        request.request_id().clone(),
        ApplicationFailure::from(&ApplicationError::InvalidPayload),
    )
    .map_err(DispatchError::from)
}

fn restore_overwrite_request_payload(
    input: &RestoreOverwriteConfirmationPreparePayload,
    confirmation_token_id: &ConfirmationTokenId,
    confirmation_token: &str,
) -> serde_json::Value {
    json!({
        "workspaceId": input.workspace_id,
        "confirmationTokenId": confirmation_token_id.as_str(),
        "confirmationToken": confirmation_token,
        "snapshotId": input.snapshot_id,
        "sourceReplicaId": input.source_replica_id,
        "targetConnectionId": input.target_connection_id,
        "targetName": input.target_name,
        "confirmedTargetName": input.confirmed_target_name,
        "preflightVersion": input.preflight_version,
        "requiredVerificationLevel": input.required_verification_level,
    })
}

fn restore_overwrite_confirmation_response(
    result: &RestoreOverwriteConfirmationIssueResult,
    confirmation_token: Option<&str>,
    overwrite_request: Option<serde_json::Value>,
) -> ApplicationResponse {
    ApplicationResponse {
        deduplicated: false,
        resource: Some(ResourceRef {
            resource_type: "confirmation_token".to_owned(),
            resource_id: result.confirmation_token_id().as_str().to_owned(),
        }),
        body: json!({
            "schemaVersion": 1,
            "confirmationTokenId": result.confirmation_token_id().as_str(),
            "confirmationToken": confirmation_token,
            "action": ConfirmationAction::RestoreOverwrite.as_str(),
            "issuedAtMs": result.issued_at_ms(),
            "expiresAtMs": result.expires_at_ms(),
            "tokenAvailable": result.created(),
            "payloadDigestBlake3": result.payload_digest_blake3(),
            "finalSummary": {
                "source": {
                    "snapshotId": result.recovery_point_id().as_str(),
                    "replicaId": result.source_replica_id(),
                },
                "target": {
                    "connectionId": result.target_connection_profile_id().as_str(),
                    "name": result.target_database_name(),
                    "fingerprint": result.target_fingerprint(),
                },
                "mode": "overwrite",
                "destructiveEffect": "The existing target database will be replaced.",
                "cleanupPolicy": "Failure or cancellation preserves the target and requires explicit cleanup.",
                "preflightVersion": result.preflight_version(),
                "requiredVerificationLevel": result.required_verification_level(),
            },
            "overwriteRequest": overwrite_request,
        }),
    }
}

fn config_cutover_request_payload(
    input: &ConfigCutoverConfirmationPreparePayload,
    confirmation_token_id: &ConfirmationTokenId,
    confirmation_token: &str,
    cutover_marker: &str,
) -> serde_json::Value {
    json!({
        "confirmationTokenId": confirmation_token_id.as_str(),
        "confirmationToken": confirmation_token,
        "cutoverRunId": input.cutover_run_id,
        "cutoverMarker": cutover_marker,
        "shadowImportId": input.shadow_import_id,
        "sourceChecksumBlake3": input.source_checksum_blake3,
        "relationDigestBlake3": input.relation_digest_blake3,
        "g1EvidencePath": input.g1_evidence_path,
        "recoveryBundleManifestPath": input.recovery_bundle_manifest_path,
        "freezeReceiptPath": input.freeze_receipt_path,
    })
}

fn config_cutover_confirmation_response(
    result: &ConfigCutoverConfirmationIssueResult,
    confirmation_token: Option<&str>,
    cutover_request: Option<serde_json::Value>,
) -> ApplicationResponse {
    ApplicationResponse {
        deduplicated: false,
        resource: Some(ResourceRef {
            resource_type: "confirmation_token".to_owned(),
            resource_id: result.confirmation_token_id().as_str().to_owned(),
        }),
        body: json!({
            "schemaVersion": 1,
            "confirmationTokenId": result.confirmation_token_id().as_str(),
            "confirmationToken": confirmation_token,
            "action": ConfirmationAction::ConfigCutover.as_str(),
            "cutoverRunId": result.cutover_run_id().to_string(),
            "shadowImportId": result.shadow_import_id().to_string(),
            "cutoverMarker": result.cutover_marker(),
            "issuedAtMs": result.issued_at_ms(),
            "expiresAtMs": result.expires_at_ms(),
            "tokenAvailable": result.created(),
            "payloadDigestBlake3": result.payload_digest_blake3(),
            "cutoverRequest": cutover_request,
        }),
    }
}

fn confirmation_response(
    result: &RetentionConfirmationIssueResult,
    action: ConfirmationAction,
    confirmation_token: Option<&str>,
) -> Result<ApplicationResponse, DispatchError> {
    let authorization = serde_json::to_value(result.authorization())
        .map_err(|_| DispatchError::StoredResponseInvalid)?;
    Ok(ApplicationResponse {
        deduplicated: false,
        resource: Some(ResourceRef {
            resource_type: "confirmation_token".to_owned(),
            resource_id: result.confirmation_token_id().as_str().to_owned(),
        }),
        body: json!({
            "schemaVersion": 1,
            "confirmationTokenId": result.confirmation_token_id().as_str(),
            "confirmationToken": confirmation_token,
            "action": action.as_str(),
            "issuedAtMs": result.issued_at_ms(),
            "expiresAtMs": result.expires_at_ms(),
            "tokenAvailable": result.created(),
            "payloadDigestBlake3": result.authorization().digest_blake3(),
            "authorization": authorization,
        }),
    })
}

fn canonical_uuid(value: &str) -> Option<Uuid> {
    Uuid::parse_str(value)
        .ok()
        .filter(|parsed| parsed.to_string() == value)
}

fn retention_delete_response(
    operation_run_id: &OperationRunId,
    result: &RetentionDeleteEnqueueResult,
) -> ApplicationResponse {
    ApplicationResponse {
        deduplicated: false,
        resource: Some(ResourceRef {
            resource_type: "work_item".to_owned(),
            resource_id: result.work_item_id().as_str().to_owned(),
        }),
        body: json!({
            "schemaVersion": 1,
            "operationRunId": operation_run_id.as_str(),
            "workItemId": result.work_item_id().as_str(),
            "state": "queued",
        }),
    }
}

fn replica_repair_response(result: &ReplicaRepairEnqueueResult) -> ApplicationResponse {
    ApplicationResponse {
        deduplicated: false,
        resource: Some(ResourceRef {
            resource_type: "operation_run".to_owned(),
            resource_id: result.operation_run_id().as_str().to_owned(),
        }),
        body: json!({
            "schemaVersion": 1,
            "operationRunId": result.operation_run_id().as_str(),
            "workItemId": result.work_item_id().as_str(),
            "state": "queued",
            "operationKind": "replica_repair",
            "activityAfterSeq": result.activity_after_seq(),
        }),
    }
}

fn manual_verification_response(result: &ManualVerificationEnqueueResult) -> ApplicationResponse {
    ApplicationResponse {
        deduplicated: false,
        resource: Some(ResourceRef {
            resource_type: "verification_run".to_owned(),
            resource_id: result.verification_run_id().as_str().to_owned(),
        }),
        body: json!({
            "schemaVersion": 1,
            "verificationRunId": result.verification_run_id().as_str(),
            "workItemId": result.work_item_id().map(WorkItemId::as_str),
            "state": result.state(),
            "requestedLevel": result.requested_level(),
            "sourceReplicaId": result.source_replica_id().as_str(),
            "supportStatus": result.support_status(),
            "limitation": result.limitation(),
            "errorCode": result.error_code(),
            "activityAfterSeq": result.activity_after_seq(),
        }),
    }
}

fn manual_backup_response(result: &ManualBackupEnqueueResult) -> ApplicationResponse {
    let work_items = result
        .work_items()
        .iter()
        .map(|item| {
            json!({
                "backupRunId": item.backup_run_id().as_str(),
                "workItemId": item.work_item_id().as_str(),
                "targetId": item.target_id().as_str(),
                "targetName": item.target_name(),
                "engine": item.engine(),
                "state": "queued",
            })
        })
        .collect::<Vec<_>>();
    ApplicationResponse {
        deduplicated: false,
        resource: Some(ResourceRef {
            resource_type: "plan_occurrence".to_owned(),
            resource_id: result.plan_occurrence_id().as_str().to_owned(),
        }),
        body: json!({
            "schemaVersion": 1,
            "planId": result.plan_id().as_str(),
            "planRevisionId": result.plan_revision_id().as_str(),
            "planOccurrenceId": result.plan_occurrence_id().as_str(),
            "planName": result.plan_name(),
            "state": "queued",
            "workItems": work_items,
            "activityAfterSeq": result.activity_after_seq(),
        }),
    }
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}
