use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dbbackup_catalog::{
    rusqlite::{params, Connection},
    ClaimRequest, ClaimTerminalOutcome, ConfirmationSpec, ConnectionProfileSpec,
    DatabaseTargetSpec, DestinationSpec, IpcRequestSpec, OperationalStore, PlanDraftSpec,
    RetentionDeleteSettlementOutcome, RetentionDeleteSpec, RetentionTombstoneSpec,
    RetryDisposition,
};
use dbbackup_domain::{
    BackupPlanId, ConfirmationTokenId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, IpcRequestId, OperationRunId, PlanOccurrenceId, PlanRevisionId,
    RecoveryPointId, WorkItemId,
};
use dbbackup_pipeline::{DeleteErrorV2, DestinationDeleteV2, RetentionDeleteAuthorizationV2};
use dbbackup_runtime::{
    RetentionDeleteCancellationToken, RetentionDeleteOperatorResolverV2, RetentionDeleteWorkerV2,
    RuntimeClock,
};
use tempfile::TempDir;
use uuid::Uuid;

type DeleteCalls = Arc<Mutex<Vec<(String, Option<String>)>>>;

#[tokio::test]
async fn restarted_worker_executes_the_durable_exact_object_set_and_settles_truth() {
    let fixture = Fixture::new();
    let authorization = fixture.enqueue_delete();
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(ClaimRequest::new(
            "retention-worker",
            "boot-after-restart",
            501,
            1_000,
        ))
        .expect("claim durable delete")
        .expect("retention work");
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let resolver = Arc::new(RecordingResolver {
        expected_destination: fixture.destination_fingerprint.clone(),
        deleted: Arc::clone(&deleted),
    });
    let clock = Arc::new(SequenceClock::new([502, 503, 504, 510]));
    let worker = RetentionDeleteWorkerV2::new(repository, resolver, clock, 1_000, 100)
        .expect("retention worker");

    let outcome = worker
        .execute(&claim)
        .await
        .expect("execute durable delete");

    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Terminal(ClaimTerminalOutcome::Succeeded)
    );
    assert_eq!(
        *deleted.lock().expect("delete observations"),
        vec![
            (
                "set/manifest.json".to_owned(),
                Some("manifest-version".to_owned())
            ),
            (
                "payload/object.bin".to_owned(),
                Some("payload-version".to_owned())
            ),
        ],
        "the signed commit marker must be deleted first and exact versions must be preserved"
    );
    assert_ne!(
        authorization.digest_blake3(),
        fixture.persisted_authorization_digest().as_str(),
        "delete acceptance must persist a fresh server-time authorization"
    );
    assert_eq!(
        authorization.digest_blake3(),
        fixture.persisted_confirmation_digest().as_str()
    );
    assert_eq!(
        fixture.object_states(),
        vec![
            (
                "payload/object.bin".to_owned(),
                Some("payload-version".to_owned()),
                "deleted".to_owned(),
            ),
            (
                "set/manifest.json".to_owned(),
                Some("manifest-version".to_owned()),
                "deleted".to_owned(),
            ),
        ]
    );
    assert_eq!(
        fixture.runtime_states(),
        (
            "succeeded".to_owned(),
            "succeeded".to_owned(),
            "succeeded".to_owned(),
        )
    );
    assert_eq!(
        fixture.last_heartbeat_at_ms(),
        504,
        "the claim must be fenced and renewed before each destructive object request"
    );
}

#[tokio::test]
async fn cancellation_between_objects_is_fenced_before_the_next_remote_delete() {
    let fixture = Fixture::new();
    fixture.enqueue_delete();
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(ClaimRequest::new(
            "retention-worker",
            "boot-cancel",
            501,
            1_000,
        ))
        .expect("claim durable delete")
        .expect("retention work");
    let deleted = Arc::new(Mutex::new(Vec::new()));
    let resolver = Arc::new(CancelAfterFirstResolver {
        repository: repository.clone(),
        work_item_id: claim.work_item_id().clone(),
        fencing_token: claim.fencing_token(),
        deleted: Arc::clone(&deleted),
    });
    let clock = Arc::new(SequenceClock::new([502, 503, 506, 510]));
    let worker = RetentionDeleteWorkerV2::new(repository, resolver, clock, 1_000, 100)
        .expect("retention worker");

    let outcome = worker.execute(&claim).await.expect("settle cancellation");

    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Terminal(ClaimTerminalOutcome::Cancelled)
    );
    assert_eq!(
        *deleted.lock().expect("actual destination deletes"),
        vec![(
            "set/manifest.json".to_owned(),
            Some("manifest-version".to_owned())
        )],
        "the payload request must not reach the destination after cancellation"
    );
    assert_eq!(
        fixture.object_states(),
        vec![
            (
                "payload/object.bin".to_owned(),
                Some("payload-version".to_owned()),
                "present".to_owned(),
            ),
            (
                "set/manifest.json".to_owned(),
                Some("manifest-version".to_owned()),
                "deleted".to_owned(),
            ),
        ]
    );
    assert_eq!(
        fixture.runtime_states(),
        (
            "cancelled".to_owned(),
            "cancelled".to_owned(),
            "cancelled".to_owned(),
        )
    );
}

#[tokio::test]
async fn absolute_remote_delete_deadline_requeues_a_blackhole_claim_and_releases_its_lease() {
    let fixture = Fixture::new();
    fixture.enqueue_delete();
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(ClaimRequest::new(
            "retention-worker",
            "boot-deadline",
            501,
            1_000,
        ))
        .expect("claim durable delete")
        .expect("retention work");
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(BlackholeResolver {
        calls: Arc::clone(&calls),
    });
    let clock = Arc::new(SequenceClock::new([502, 503, 510]));
    let worker = RetentionDeleteWorkerV2::new(repository, resolver, clock, 1_000, 100)
        .expect("retention worker")
        .with_remote_delete_deadline(Duration::from_millis(20))
        .expect("bounded remote-delete deadline");

    let outcome = tokio::time::timeout(Duration::from_millis(500), worker.execute(&claim))
        .await
        .expect("blackhole delete must be bounded")
        .expect("deadline must settle durably");

    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Retry(RetryDisposition::Requeued)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "payload stays marker-blocked"
    );
    fixture.assert_retryable_release("RETENTION_DELETE_DEADLINE_EXCEEDED", 610);
}

#[tokio::test]
async fn shutdown_cancellation_requeues_a_blackhole_claim_without_waiting_for_the_deadline() {
    let fixture = Fixture::new();
    fixture.enqueue_delete();
    let repository = fixture.store.runtime();
    let claim = repository
        .claim_next(ClaimRequest::new(
            "retention-worker",
            "boot-shutdown",
            501,
            1_000,
        ))
        .expect("claim durable delete")
        .expect("retention work");
    let calls = Arc::new(AtomicUsize::new(0));
    let resolver = Arc::new(BlackholeResolver {
        calls: Arc::clone(&calls),
    });
    let clock = Arc::new(SequenceClock::new([502, 503, 510]));
    let worker = RetentionDeleteWorkerV2::new(repository, resolver, clock, 1_000, 100)
        .expect("retention worker")
        .with_remote_delete_deadline(Duration::from_millis(900))
        .expect("bounded remote-delete deadline");
    let cancellation = RetentionDeleteCancellationToken::default();
    let execution_cancellation = cancellation.clone();

    let execution = tokio::spawn(async move {
        worker
            .execute_with_cancellation(&claim, &execution_cancellation)
            .await
    });
    tokio::time::timeout(Duration::from_millis(500), async {
        while calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("blackhole delete entered");
    cancellation.cancel();

    let outcome = tokio::time::timeout(Duration::from_millis(500), execution)
        .await
        .expect("shutdown cancellation must wake the delete")
        .expect("retention task join")
        .expect("shutdown cancellation must settle durably");
    assert_eq!(
        outcome,
        RetentionDeleteSettlementOutcome::Retry(RetryDisposition::Requeued)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "payload stays marker-blocked"
    );
    fixture.assert_retryable_release("RETENTION_DELETE_SHUTDOWN_REQUESTED", 610);
}

struct RecordingResolver {
    expected_destination: String,
    deleted: DeleteCalls,
}

struct BlackholeResolver {
    calls: Arc<AtomicUsize>,
}

impl RetentionDeleteOperatorResolverV2 for BlackholeResolver {
    fn resolve(
        &self,
        _destination_fingerprint: &str,
        _delete_secret_ref_id: Option<&str>,
    ) -> Result<Box<dyn DestinationDeleteV2>, dbbackup_runtime::RetentionDeleteResolutionErrorV2>
    {
        Ok(Box::new(BlackholeDeleteOperator {
            calls: Arc::clone(&self.calls),
        }))
    }
}

struct BlackholeDeleteOperator {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl DestinationDeleteV2 for BlackholeDeleteOperator {
    async fn delete_object(&self, _key: &str, _version: Option<&str>) -> Result<(), DeleteErrorV2> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

impl RetentionDeleteOperatorResolverV2 for RecordingResolver {
    fn resolve(
        &self,
        destination_fingerprint: &str,
        delete_secret_ref_id: Option<&str>,
    ) -> Result<Box<dyn DestinationDeleteV2>, dbbackup_runtime::RetentionDeleteResolutionErrorV2>
    {
        assert_eq!(destination_fingerprint, self.expected_destination);
        assert_eq!(delete_secret_ref_id, None);
        Ok(Box::new(RecordingDeleteOperator {
            deleted: Arc::clone(&self.deleted),
        }))
    }
}

struct CancelAfterFirstResolver {
    repository: dbbackup_catalog::RuntimeRepository,
    work_item_id: WorkItemId,
    fencing_token: i64,
    deleted: DeleteCalls,
}

impl RetentionDeleteOperatorResolverV2 for CancelAfterFirstResolver {
    fn resolve(
        &self,
        _destination_fingerprint: &str,
        delete_secret_ref_id: Option<&str>,
    ) -> Result<Box<dyn DestinationDeleteV2>, dbbackup_runtime::RetentionDeleteResolutionErrorV2>
    {
        assert_eq!(delete_secret_ref_id, None);
        Ok(Box::new(CancelAfterFirstOperator {
            repository: self.repository.clone(),
            work_item_id: self.work_item_id.clone(),
            fencing_token: self.fencing_token,
            deleted: Arc::clone(&self.deleted),
        }))
    }
}

struct RecordingDeleteOperator {
    deleted: DeleteCalls,
}

struct CancelAfterFirstOperator {
    repository: dbbackup_catalog::RuntimeRepository,
    work_item_id: WorkItemId,
    fencing_token: i64,
    deleted: DeleteCalls,
}

#[async_trait]
impl DestinationDeleteV2 for CancelAfterFirstOperator {
    async fn delete_object(&self, key: &str, version: Option<&str>) -> Result<(), DeleteErrorV2> {
        let should_cancel = {
            let mut deleted = self.deleted.lock().expect("destination deletes");
            deleted.push((key.to_owned(), version.map(str::to_owned)));
            deleted.len() == 1
        };
        if should_cancel {
            self.repository
                .request_cancel(&self.work_item_id, self.fencing_token, 505)
                .expect("request cancellation");
        }
        Ok(())
    }
}

#[async_trait]
impl DestinationDeleteV2 for RecordingDeleteOperator {
    async fn delete_object(&self, key: &str, version: Option<&str>) -> Result<(), DeleteErrorV2> {
        self.deleted
            .lock()
            .expect("delete observations")
            .push((key.to_owned(), version.map(str::to_owned)));
        Ok(())
    }
}

struct SequenceClock {
    values: Mutex<VecDeque<i64>>,
}

impl SequenceClock {
    fn new(values: impl IntoIterator<Item = i64>) -> Self {
        Self {
            values: Mutex::new(values.into_iter().collect()),
        }
    }
}

impl RuntimeClock for SequenceClock {
    fn now_ms(&self) -> i64 {
        self.values
            .lock()
            .expect("clock")
            .pop_front()
            .expect("clock value")
    }
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    recovery_point_id: RecoveryPointId,
    destination_fingerprint: String,
}

struct RetryReleaseState {
    work_state: String,
    lease_owner: Option<String>,
    lease_expires_at_ms: Option<i64>,
    available_at_ms: i64,
    attempt_state: String,
    retry_class: Option<String>,
    retry_after_ms: Option<i64>,
    allocation_count: i64,
    released_allocations: i64,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("operational store");
        let configuration = store.configuration();
        let connection_profile_id = ConnectionProfileId::new();
        let target_id = DatabaseTargetId::new();
        let destination_id = DestinationId::new();
        let revision_id = PlanRevisionId::new();
        configuration
            .insert_connection(ConnectionProfileSpec::tcp(
                connection_profile_id.clone(),
                "retention-source",
                "postgresql",
                "127.0.0.1",
                5432,
                1,
            ))
            .expect("insert connection");
        configuration
            .insert_target(DatabaseTargetSpec::new(
                target_id.clone(),
                connection_profile_id,
                "orders",
                "orders",
                "orders",
                "source:orders",
                2,
            ))
            .expect("insert target");
        configuration
            .insert_destination(DestinationSpec::local(
                destination_id.clone(),
                "retention-vault",
                r#"{"path":"C:\\retention-worker"}"#,
                3,
            ))
            .expect("insert destination");
        configuration
            .insert_plan_draft(PlanDraftSpec::manual(
                BackupPlanId::new(),
                revision_id.clone(),
                "retention-worker",
                1,
                "a".repeat(64),
                4,
            ))
            .expect("insert plan");
        configuration
            .add_revision_target(&revision_id, &target_id, 5)
            .expect("add target");
        configuration
            .add_revision_destination(
                &revision_id,
                &destination_id,
                DestinationRequirement::Required,
                6,
            )
            .expect("add destination");
        {
            let connection = Connection::open(&db_path).expect("open configuration");
            connection
                .execute(
                    "UPDATE backup_plan_revisions
                     SET retention_policy_json = '{\"daily\":1,\"weekly\":0,\"monthly\":0}'
                     WHERE id = ?1",
                    [revision_id.as_str()],
                )
                .expect("set retention policy");
            connection
                .execute(
                    "UPDATE destinations SET state = 'ready' WHERE id = ?1",
                    [destination_id.as_str()],
                )
                .expect("mark destination ready");
        }
        configuration
            .publish_revision(&revision_id, 7)
            .expect("publish plan");
        let recovery_point_id = RecoveryPointId::new();
        let destination_fingerprint = format!("local:{}", destination_id.as_str());
        seed_projection(
            &db_path,
            &revision_id,
            &target_id,
            &recovery_point_id,
            &destination_fingerprint,
        );
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
            recovery_point_id,
            destination_fingerprint,
        }
    }

    fn enqueue_delete(&self) -> RetentionDeleteAuthorizationV2 {
        let token_id = ConfirmationTokenId::new();
        let token_digest = "8".repeat(64);
        let sid_hash = "9".repeat(64);
        let repository = self.store.ipc();
        let operation_run_id = OperationRunId::new();
        repository
            .stage_retention_tombstone(RetentionTombstoneSpec::new(
                operation_run_id.clone(),
                self.recovery_point_id.clone(),
                self.destination_fingerprint.clone(),
                300,
                200,
            ))
            .expect("stage retention tombstone");
        let authorization = self
            .store
            .retention_planner()
            .prepare_delete_plan(
                &self.recovery_point_id,
                &self.destination_fingerprint,
                500,
                300,
            )
            .expect("authoritative retention plan")
            .authorization()
            .clone();
        let digest = authorization.digest_blake3().to_owned();
        repository
            .issue_confirmation(ConfirmationSpec::new(
                token_id.clone(),
                token_digest.clone(),
                "retention_delete",
                sid_hash.clone(),
                "artifact_replica",
                self.recovery_point_id.as_str(),
                self.destination_fingerprint.clone(),
                digest.clone(),
                490,
                1_000,
            ))
            .expect("issue confirmation");
        repository
            .enqueue_retention_delete(
                IpcRequestSpec::new(
                    IpcRequestId::new(),
                    "retention.delete",
                    sid_hash,
                    "retention-worker-request",
                    "1".repeat(64),
                    501,
                    2_000,
                ),
                &token_id,
                &token_digest,
                RetentionDeleteSpec::new(
                    operation_run_id,
                    WorkItemId::new(),
                    authorization.clone(),
                ),
            )
            .expect("enqueue retention delete");
        authorization
    }

    fn persisted_authorization_digest(&self) -> String {
        let connection = Connection::open(&self.db_path).expect("open authorization query");
        let request_json: String = connection
            .query_row(
                "SELECT request_json FROM operation_runs WHERE operation_kind = 'retention_delete'",
                [],
                |row| row.get(0),
            )
            .expect("operation request");
        let request: serde_json::Value =
            serde_json::from_str(&request_json).expect("operation request JSON");
        request["authorization"]["digest_blake3"]
            .as_str()
            .expect("authorization digest")
            .to_owned()
    }

    fn persisted_confirmation_digest(&self) -> String {
        let connection = Connection::open(&self.db_path).expect("open authorization query");
        let request_json: String = connection
            .query_row(
                "SELECT request_json FROM operation_runs WHERE operation_kind = 'retention_delete'",
                [],
                |row| row.get(0),
            )
            .expect("operation request");
        let request: serde_json::Value =
            serde_json::from_str(&request_json).expect("operation request JSON");
        request["confirmation_digest_blake3"]
            .as_str()
            .expect("confirmation digest")
            .to_owned()
    }

    fn object_states(&self) -> Vec<(String, Option<String>, String)> {
        let connection = Connection::open(&self.db_path).expect("open object query");
        let mut statement = connection
            .prepare(
                "SELECT object_key, object_version, state
                 FROM catalog_stored_objects
                 ORDER BY object_key",
            )
            .expect("prepare object query");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query objects")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect objects")
    }

    fn runtime_states(&self) -> (String, String, String) {
        Connection::open(&self.db_path)
            .expect("open runtime query")
            .query_row(
                "SELECT work_items.state, work_attempts.state, operation_runs.state
                 FROM work_items
                 JOIN work_attempts ON work_attempts.work_item_id = work_items.id
                 JOIN operation_runs ON operation_runs.id = work_items.operation_run_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("runtime states")
    }

    fn last_heartbeat_at_ms(&self) -> i64 {
        Connection::open(&self.db_path)
            .expect("open heartbeat query")
            .query_row(
                "SELECT last_heartbeat_at_ms FROM work_attempts",
                [],
                |row| row.get(0),
            )
            .expect("last heartbeat")
    }

    fn assert_retryable_release(&self, expected_diagnostic: &str, available_at_ms: i64) {
        let connection = Connection::open(&self.db_path).expect("open retry settlement query");
        let state: RetryReleaseState = connection
            .query_row(
                "SELECT work_items.state, work_items.lease_owner,
                        work_items.lease_expires_at_ms, work_items.available_at_ms,
                        work_attempts.state, work_attempts.retry_class,
                        work_attempts.retry_after_ms,
                        COUNT(resource_allocations.id),
                        COALESCE(SUM(CASE WHEN resource_allocations.released_at_ms IS NOT NULL
                                          THEN 1 ELSE 0 END), 0)
                 FROM work_items
                 JOIN work_attempts ON work_attempts.work_item_id = work_items.id
                 LEFT JOIN resource_allocations
                   ON resource_allocations.work_attempt_id = work_attempts.id
                 GROUP BY work_items.id, work_attempts.id",
                [],
                |row| {
                    Ok(RetryReleaseState {
                        work_state: row.get(0)?,
                        lease_owner: row.get(1)?,
                        lease_expires_at_ms: row.get(2)?,
                        available_at_ms: row.get(3)?,
                        attempt_state: row.get(4)?,
                        retry_class: row.get(5)?,
                        retry_after_ms: row.get(6)?,
                        allocation_count: row.get(7)?,
                        released_allocations: row.get(8)?,
                    })
                },
            )
            .expect("retry settlement");
        assert_eq!(state.work_state, "queued");
        assert_eq!(state.lease_owner, None);
        assert_eq!(state.lease_expires_at_ms, None);
        assert_eq!(state.available_at_ms, available_at_ms);
        assert_eq!(state.attempt_state, "failed");
        assert_eq!(state.retry_class.as_deref(), Some("retryable"));
        assert_eq!(state.retry_after_ms, Some(available_at_ms));
        assert_eq!(state.released_allocations, state.allocation_count);

        let result_json: String = connection
            .query_row(
                "SELECT result_json FROM operation_runs WHERE operation_kind = 'retention_delete'",
                [],
                |row| row.get(0),
            )
            .expect("retention result evidence");
        let result: serde_json::Value =
            serde_json::from_str(&result_json).expect("retention result JSON");
        assert_eq!(result["retry_required_count"], 1);
        assert_eq!(result["blocked_count"], 1);
        assert_eq!(
            result["observations"][0]["diagnostic"].as_str(),
            Some(expected_diagnostic)
        );
    }
}

fn seed_projection(
    db_path: &std::path::Path,
    revision_id: &PlanRevisionId,
    target_id: &DatabaseTargetId,
    recovery_point_id: &RecoveryPointId,
    destination_fingerprint: &str,
) {
    let connection = Connection::open(db_path).expect("open projection seed");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    let occurrence_id = PlanOccurrenceId::new();
    let backup_run_id = Uuid::new_v4().to_string();
    let part_id = Uuid::new_v4().to_string();
    let replica_id = Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO plan_occurrences(
                 id, plan_revision_id, origin, requested_at_ms,
                 request_idempotency_key, state, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'manual', 100, ?3, 'succeeded', 100, 100)",
            params![
                occurrence_id.as_str(),
                revision_id.as_str(),
                format!("manual:{}", occurrence_id.as_str())
            ],
        )
        .expect("occurrence");
    connection
        .execute(
            "INSERT INTO backup_runs(
                 id, plan_occurrence_id, plan_revision_id, database_target_id,
                 state, consistency_grade, engine, source_fingerprint,
                 started_at_ms, completed_at_ms, recovery_point_id,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 'succeeded', 'transaction_consistent', 'postgresql', 'source:orders',
                 100, 100, ?5, 100, 100
             )",
            params![
                backup_run_id,
                occurrence_id.as_str(),
                revision_id.as_str(),
                target_id.as_str(),
                recovery_point_id.as_str()
            ],
        )
        .expect("backup run");
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, origin_occurrence_id, origin_backup_run_id,
                 source_target_identity, source_display_name,
                 engine, state, consistency_grade, committed_at_ms,
                 manifest_schema_version, set_manifest_checksum_blake3,
                 signing_key_id, signature_status, required_part_count,
                 present_part_count, engine_metadata_json,
                 first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (
                 ?1, 'fixture-instance', ?2, ?3,
                 'source:orders', 'orders',
                 'postgresql', 'complete', 'transaction_consistent', 100,
                 2, ?4, 'fixture-key', 'valid', 1, 1, '{}', 100, 100
             )",
            params![
                recovery_point_id.as_str(),
                occurrence_id.as_str(),
                backup_run_id,
                "a".repeat(64)
            ],
        )
        .expect("recovery point");
    connection
        .execute(
            "INSERT INTO catalog_artifact_parts(
                 id, recovery_point_id, part_role, logical_name, required,
                 dump_format, compression_format, encryption_format,
                 metadata_json, created_at_ms
             ) VALUES (?1, ?2, 'database_dump', 'orders', 1,
                       'custom', 'zstd', 'age', '{}', 100)",
            params![part_id, recovery_point_id.as_str()],
        )
        .expect("artifact part");
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint,
                 destination_kind, requirement, state,
                 immutability_capability, reader_status, committed_at_ms,
                 last_observed_at_ms, metadata_json
             ) VALUES (?1, ?2, ?3, 'local', 'required', 'committed',
                       'none', 'eligible', 100, 100, '{}')",
            params![
                replica_id,
                recovery_point_id.as_str(),
                destination_fingerprint
            ],
        )
        .expect("artifact replica");
    for (role, key, version, artifact_part_id, checksum) in [
        (
            "set_manifest",
            "set/manifest.json",
            "manifest-version",
            None,
            "b".repeat(64),
        ),
        (
            "payload",
            "payload/object.bin",
            "payload-version",
            Some(part_id.as_str()),
            "c".repeat(64),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO catalog_stored_objects(
                     id, recovery_point_id, artifact_replica_id, artifact_part_id,
                     object_role, object_key, object_version, checksum_blake3,
                     size_bytes, state, observed_at_ms, metadata_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 5, 'present', 100, '{}')",
                params![
                    Uuid::new_v4().to_string(),
                    recovery_point_id.as_str(),
                    replica_id,
                    artifact_part_id,
                    role,
                    key,
                    version,
                    checksum
                ],
            )
            .expect("stored object");
    }
    connection
        .execute(
            "INSERT INTO catalog_snapshot_projection(
                 recovery_point_id, source_target_identity, source_display_name,
                 engine, committed_at_ms, artifact_completeness,
                 replica_durability, protection_compliance, restore_eligibility,
                 search_text, projection_version, projected_at_ms
             ) VALUES (?1, 'source:orders', 'orders', 'postgresql', 100,
                       'complete', 'healthy', 'compliant', 'eligible',
                       'orders source:orders', 1, 100)",
            [recovery_point_id.as_str()],
        )
        .expect("snapshot projection");
    seed_newer_generation(&connection, revision_id, target_id, destination_fingerprint);
}

fn seed_newer_generation(
    connection: &Connection,
    revision_id: &PlanRevisionId,
    target_id: &DatabaseTargetId,
    destination_fingerprint: &str,
) {
    let recovery_point_id = RecoveryPointId::new();
    let occurrence_id = PlanOccurrenceId::new();
    let backup_run_id = Uuid::new_v4().to_string();
    let replica_id = Uuid::new_v4().to_string();
    connection
        .execute(
            "INSERT INTO plan_occurrences(
                 id, plan_revision_id, origin, requested_at_ms,
                 request_idempotency_key, state, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'manual', 150, ?3, 'succeeded', 150, 150)",
            params![
                occurrence_id.as_str(),
                revision_id.as_str(),
                format!("manual:{}", occurrence_id.as_str())
            ],
        )
        .expect("newer occurrence");
    connection
        .execute(
            "INSERT INTO backup_runs(
                 id, plan_occurrence_id, plan_revision_id, database_target_id,
                 state, consistency_grade, engine, source_fingerprint,
                 started_at_ms, completed_at_ms, recovery_point_id,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4,
                 'succeeded', 'transaction_consistent', 'postgresql', 'source:orders',
                 150, 150, ?5, 150, 150
             )",
            params![
                backup_run_id,
                occurrence_id.as_str(),
                revision_id.as_str(),
                target_id.as_str(),
                recovery_point_id.as_str()
            ],
        )
        .expect("newer backup run");
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, origin_occurrence_id, origin_backup_run_id,
                 source_target_identity, source_display_name,
                 engine, state, consistency_grade, committed_at_ms,
                 manifest_schema_version, set_manifest_checksum_blake3,
                 signing_key_id, signature_status, required_part_count,
                 present_part_count, engine_metadata_json,
                 first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (
                 ?1, 'fixture-instance', ?2, ?3,
                 'source:orders', 'orders',
                 'postgresql', 'complete', 'transaction_consistent', 150,
                 2, ?4, 'fixture-key', 'valid', 1, 1, '{}', 150, 150
             )",
            params![
                recovery_point_id.as_str(),
                occurrence_id.as_str(),
                backup_run_id,
                "d".repeat(64)
            ],
        )
        .expect("newer recovery point");
    connection
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint,
                 destination_kind, requirement, state,
                 immutability_capability, reader_status, committed_at_ms,
                 last_observed_at_ms, metadata_json
             ) VALUES (?1, ?2, ?3, 'local', 'required', 'committed',
                       'none', 'eligible', 150, 150, '{}')",
            params![
                replica_id,
                recovery_point_id.as_str(),
                destination_fingerprint
            ],
        )
        .expect("newer artifact replica");
    connection
        .execute(
            "INSERT INTO catalog_snapshot_projection(
                 recovery_point_id, source_target_identity, source_display_name,
                 engine, committed_at_ms, artifact_completeness,
                 replica_durability, protection_compliance, restore_eligibility,
                 search_text, projection_version, projected_at_ms
             ) VALUES (?1, 'source:orders', 'orders', 'postgresql', 150,
                       'complete', 'healthy', 'compliant', 'eligible',
                       'orders source:orders', 1, 150)",
            [recovery_point_id.as_str()],
        )
        .expect("newer snapshot projection");
}
