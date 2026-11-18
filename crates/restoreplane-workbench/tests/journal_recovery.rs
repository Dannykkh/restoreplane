//! Local fault model. These tests do not prove remote COMMIT behavior.
use async_trait::async_trait;
use restoreplane_domain::workbench::*;
use restoreplane_domain::{ConnectionProfileId, DatabaseTargetId, WorkspaceId};
use restoreplane_workbench::driver::{
    DriverFactory, DriverLogin, NativeDriverFactory, QueryLimits, RowSink,
};
use restoreplane_workbench::events::{HintAction, OperationHint, VersionTracker};
use restoreplane_workbench::journal::*;
use restoreplane_workbench::store::{ClientStore, UserRootPolicy};
use restoreplane_workbench::{IntegrityLevel, InvocationContext, NativePrincipal, WorkbenchError};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::{
    atomic::{AtomicI64, AtomicUsize, Ordering},
    Arc, Barrier,
};
use uuid::Uuid;
use zeroize::Zeroizing;

struct TestRoot;
impl UserRootPolicy for TestRoot {
    fn ensure_private_root(&self, path: &Path) -> Result<(), WorkbenchError> {
        std::fs::create_dir_all(path).map_err(|_| WorkbenchError::Io)
    }
}
struct Clock(AtomicI64);
impl JournalClock for Clock {
    fn now_ms(&self) -> Result<i64, WorkbenchError> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}
struct Fixture {
    directory: tempfile::TempDir,
    store: Arc<ClientStore>,
    journal: Arc<OperationJournal>,
    clock: Arc<Clock>,
    context: InvocationContext,
    receipt: BindingReceipt,
    session: SessionRef,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let principal = NativePrincipal::new("a".repeat(64), 7, IntegrityLevel::Standard).unwrap();
        let context = InvocationContext::new(principal.clone(), Uuid::new_v4());
        let store = Arc::new(ClientStore::open(directory.path(), &principal, &TestRoot).unwrap());
        let clock = Arc::new(Clock(AtomicI64::new(1_700_000_000_000)));
        let journal = Arc::new(
            OperationJournal::with_clock(store.clone(), JournalLimits::default(), clock.clone())
                .unwrap(),
        );
        let receipt = BindingReceipt {
            scope: Scope {
                workspace_id: WorkspaceId::new(),
                connection_profile_id: ConnectionProfileId::new(),
                database_target_id: DatabaseTargetId::new(),
            },
            descriptor: ConnectDescriptor {
                engine: Engine::Sqlite,
                route: Route::File {
                    canonical_path: "C:\\test.sqlite".into(),
                },
                database: DatabaseIdentity {
                    engine_object_id: "main".into(),
                    name: "main".into(),
                    source_fingerprint: "native-proof-fixture".into(),
                },
                tls: Tls {
                    mode: "disable".into(),
                    server_name: None,
                    root_trust: None,
                },
                transport_options: Default::default(),
                expected_server_identity: None,
            },
            descriptor_revision: "b".repeat(64),
            authority_epoch: Uuid::new_v4().to_string(),
            authorized_at_ms: 1_700_000_000_000u64.into(),
            read_lease_expires_at_ms: None,
            observed_conflicts: vec![],
        };
        Self {
            directory,
            store,
            journal,
            clock,
            context,
            receipt,
            session: SessionRef {
                session_id: Uuid::new_v4(),
                connection_generation: 1u64.into(),
            },
        }
    }
    fn spec(&self, read: bool) -> AdmissionSpec {
        AdmissionSpec {
            kind: OperationKind::Query,
            session: Some(self.session),
            transaction_id: None,
            statements: vec![StatementSpec {
                digest: "c".repeat(64),
                effect_class: if read {
                    EffectClass::Read
                } else {
                    EffectClass::Write
                },
                certified_read_only: read,
                transaction_id: None,
            }],
            batch: None,
        }
    }
    fn accepted(&self, read: bool) -> Admission {
        self.admit(
            "query.run",
            json!({"session":self.session,"reviewId":Uuid::new_v4()}),
            self.spec(read),
        )
    }
    fn admit(&self, command: &str, payload: Value, spec: AdmissionSpec) -> Admission {
        let token = self
            .journal
            .prepare(
                &self.context,
                &self.receipt,
                spec.session,
                command,
                &payload,
            )
            .unwrap();
        self.journal
            .admit(
                &self.context,
                &token.attempt_token,
                command,
                &payload,
                &self.receipt,
                None,
                spec,
            )
            .unwrap()
    }
    fn dispatch(&self, admission: &Admission) -> u64 {
        self.journal
            .mark_dispatch(
                &self.context,
                &self.receipt.scope,
                admission.operation_id,
                admission.version.value(),
                1,
                &self.receipt,
            )
            .unwrap()
            .version()
    }
    fn move_to(&self, id: Uuid, version: u64, next: Transition) -> Operation {
        self.journal
            .transition(&self.context, &self.receipt.scope, id, version, next)
            .unwrap()
    }
}

#[test]
fn standalone_generated_noop_has_its_own_pending_effect_and_queries_cannot_skip_sent() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec(false);
    spec.kind = OperationKind::VerifiedApply;
    let admission = fixture.admit("edit.apply", json!({"reviewId":Uuid::new_v4()}), spec);
    let dispatched = fixture.dispatch(&admission);
    let running = fixture.move_to(admission.operation_id, dispatched, Transition::Running);
    let update = StatementUpdate {
        ordinal: 0,
        expected_version: 1,
        outcome: StatementStatus::Succeeded,
        effect_evidence: EffectEvidence::PendingTransaction,
        rows_matched: Some(1),
        rows_changed: Some(0),
        error_code: None,
    };
    let mut invalid = update.clone();
    invalid.rows_changed = Some(1);
    assert_eq!(
        fixture.journal.record_statement(
            &fixture.context,
            &fixture.receipt.scope,
            running.operation_id,
            running.version.value(),
            invalid
        ),
        Err(WorkbenchError::InvalidInput)
    );
    let noop = fixture
        .journal
        .record_statement(
            &fixture.context,
            &fixture.receipt.scope,
            running.operation_id,
            running.version.value(),
            update.clone(),
        )
        .unwrap();
    assert_eq!(noop.statements[0].outcome, StatementStatus::Succeeded);
    assert_eq!(
        noop.statements[0].transaction_id, None,
        "standalone scope is not a fabricated manual transaction FK"
    );
    let commit = fixture.move_to(
        noop.operation_id,
        noop.version.value(),
        Transition::CommitRequested,
    );
    let committed = fixture.move_to(
        commit.operation_id,
        commit.version.value(),
        Transition::CommitConfirmed,
    );
    assert_eq!(
        committed.statements[0].effect_evidence,
        EffectEvidence::Committed
    );
    let query = fixture.accepted(false);
    let dispatched = fixture.dispatch(&query);
    let running = fixture.move_to(query.operation_id, dispatched, Transition::Running);
    assert_eq!(
        fixture.journal.record_statement(
            &fixture.context,
            &fixture.receipt.scope,
            running.operation_id,
            running.version.value(),
            update
        ),
        Err(WorkbenchError::InvalidInput)
    );
}

#[test]
fn known_statement_failure_requires_settled_evidence_and_preserves_prior_commits() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec(false);
    spec.statements.push(StatementSpec {
        digest: "d".repeat(64),
        effect_class: EffectClass::Write,
        certified_read_only: false,
        transaction_id: None,
    });
    spec.statements.push(StatementSpec {
        digest: "e".repeat(64),
        effect_class: EffectClass::Write,
        certified_read_only: false,
        transaction_id: None,
    });
    let admission = fixture.admit("query.run", json!({"reviewId":Uuid::new_v4()}), spec);
    let dispatched = fixture.dispatch(&admission);
    let mut operation = fixture.move_to(admission.operation_id, dispatched, Transition::Running);
    let update = |operation: &Operation, ordinal: u32, version: u64, outcome, evidence| {
        fixture
            .journal
            .record_statement(
                &fixture.context,
                &fixture.receipt.scope,
                operation.operation_id,
                operation.version.value(),
                StatementUpdate {
                    ordinal,
                    expected_version: version,
                    outcome,
                    effect_evidence: evidence,
                    rows_matched: None,
                    rows_changed: None,
                    error_code: None,
                },
            )
            .unwrap()
    };
    operation = update(
        &operation,
        0,
        1,
        StatementStatus::Sent,
        EffectEvidence::None,
    );
    operation = update(
        &operation,
        0,
        2,
        StatementStatus::Succeeded,
        EffectEvidence::Committed,
    );
    operation = update(
        &operation,
        1,
        1,
        StatementStatus::Sent,
        EffectEvidence::None,
    );
    assert_eq!(
        fixture.journal.transition(
            &fixture.context,
            &fixture.receipt.scope,
            operation.operation_id,
            operation.version.value(),
            Transition::StatementGroupFailed
        ),
        Err(WorkbenchError::OutcomeUnknown)
    );
    operation = update(
        &operation,
        1,
        2,
        StatementStatus::Failed,
        EffectEvidence::RolledBack,
    );
    operation = fixture.move_to(
        operation.operation_id,
        operation.version.value(),
        Transition::StatementGroupFailed,
    );
    assert_eq!(operation.state, OperationState::Partial);
    assert_eq!(
        operation.statements[0].effect_evidence,
        EffectEvidence::Committed
    );
    assert_eq!(
        operation.statements[1].effect_evidence,
        EffectEvidence::RolledBack
    );
    assert_eq!(operation.statements[2].outcome, StatementStatus::NotSent);
    assert_eq!(operation.result_completeness, ResultCompleteness::Partial);
    let admission = fixture.accepted(false);
    let version = fixture.dispatch(&admission);
    let mut operation = fixture.move_to(admission.operation_id, version, Transition::Running);
    operation = update(
        &operation,
        0,
        1,
        StatementStatus::Sent,
        EffectEvidence::None,
    );
    operation = update(
        &operation,
        0,
        2,
        StatementStatus::Failed,
        EffectEvidence::RolledBack,
    );
    operation = fixture.move_to(
        operation.operation_id,
        operation.version.value(),
        Transition::StatementGroupFailed,
    );
    assert_eq!(operation.state, OperationState::Failed);
    assert_eq!(
        operation.result_completeness,
        ResultCompleteness::Interrupted
    );
    let mut unknown_spec = fixture.spec(false);
    unknown_spec.statements[0].effect_class = EffectClass::Unknown;
    let admission = fixture.admit(
        "query.run",
        json!({"reviewId":Uuid::new_v4()}),
        unknown_spec,
    );
    let dispatched = fixture.dispatch(&admission);
    let mut operation = fixture.move_to(admission.operation_id, dispatched, Transition::Running);
    operation = update(
        &operation,
        0,
        1,
        StatementStatus::Sent,
        EffectEvidence::None,
    );
    operation = update(
        &operation,
        0,
        2,
        StatementStatus::Failed,
        EffectEvidence::RolledBack,
    );
    assert_eq!(
        fixture.journal.transition(
            &fixture.context,
            &fixture.receipt.scope,
            operation.operation_id,
            operation.version.value(),
            Transition::StatementGroupFailed
        ),
        Err(WorkbenchError::OutcomeUnknown),
        "known database rollback cannot settle external/unknown effects"
    );
}

#[test]
fn native_tokens_bind_exact_digest_owner_window_revision_generation_and_expiry() {
    let fixture = Fixture::new();
    let payload = json!({"cell":{"kind":"decimal","value":"9007199254740993.0000","scale":4},"reviewId":Uuid::new_v4()});
    let token = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "query.run",
            &payload,
        )
        .unwrap();
    let alias = Uuid::parse_str(&token.attempt_token)
        .unwrap()
        .simple()
        .to_string();
    assert_eq!(
        fixture
            .journal
            .admit(
                &fixture.context,
                &alias,
                "query.run",
                &payload,
                &fixture.receipt,
                None,
                fixture.spec(false)
            )
            .unwrap_err(),
        WorkbenchError::AttemptExpired
    );
    let admit = |context: &InvocationContext,
                 receipt: &BindingReceipt,
                 spec: AdmissionSpec,
                 body: &Value| {
        fixture.journal.admit(
            context,
            &token.attempt_token,
            "query.run",
            body,
            receipt,
            None,
            spec,
        )
    };
    let first = admit(
        &fixture.context,
        &fixture.receipt,
        fixture.spec(false),
        &payload,
    )
    .unwrap();
    let duplicate = admit(
        &fixture.context,
        &fixture.receipt,
        fixture.spec(false),
        &payload,
    )
    .unwrap();
    assert!(duplicate.deduplicated);
    assert_eq!(first.operation_id, duplicate.operation_id);
    let changed = json!({"cell":{"kind":"decimal","value":"9007199254740993.0001","scale":4}});
    assert_eq!(
        admit(
            &fixture.context,
            &fixture.receipt,
            fixture.spec(false),
            &changed
        )
        .unwrap_err(),
        WorkbenchError::IdempotencyConflict
    );
    let window = InvocationContext::new(fixture.context.principal().clone(), Uuid::new_v4());
    assert_eq!(
        admit(&window, &fixture.receipt, fixture.spec(false), &payload).unwrap_err(),
        WorkbenchError::Forbidden
    );
    let foreign = InvocationContext::new(
        NativePrincipal::new("d".repeat(64), 7, IntegrityLevel::Standard).unwrap(),
        fixture.context.window_instance_id(),
    );
    assert_eq!(
        admit(&foreign, &fixture.receipt, fixture.spec(false), &payload).unwrap_err(),
        WorkbenchError::Forbidden
    );
    let mut revised = fixture.receipt.clone();
    revised.descriptor_revision = "e".repeat(64);
    assert_eq!(
        admit(&fixture.context, &revised, fixture.spec(false), &payload).unwrap_err(),
        WorkbenchError::BindingStale
    );
    let mut generation = fixture.spec(false);
    generation.session.as_mut().unwrap().connection_generation = 2u64.into();
    assert_eq!(
        admit(&fixture.context, &fixture.receipt, generation, &payload).unwrap_err(),
        WorkbenchError::BindingStale
    );
    fixture.clock.0.fetch_add(86_400_000, Ordering::SeqCst);
    assert_eq!(
        admit(
            &fixture.context,
            &fixture.receipt,
            fixture.spec(false),
            &payload
        )
        .unwrap_err(),
        WorkbenchError::AttemptExpired
    );
    assert_eq!(
        fixture
            .journal
            .get(&fixture.context, &fixture.receipt.scope, first.operation_id)
            .unwrap()
            .state,
        OperationState::Accepted
    );
    assert_eq!(
        payload_digest("query.run", &json!({"b":2,"a":1})).unwrap(),
        payload_digest("query.run", &json!({"a":1,"b":2})).unwrap()
    );
}

#[test]
fn guarded_marker_has_one_winner_and_precedes_a_simulated_send() {
    let fixture = Fixture::new();
    let payload = json!({"session":fixture.session,"reviewId":Uuid::new_v4()});
    let token = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "query.run",
            &payload,
        )
        .unwrap();
    let admission_barrier = Arc::new(Barrier::new(8));
    let admissions = (0..8)
        .map(|_| {
            let journal = fixture.journal.clone();
            let context = fixture.context.clone();
            let receipt = fixture.receipt.clone();
            let payload = payload.clone();
            let token = token.attempt_token.clone();
            let spec = fixture.spec(false);
            let barrier = admission_barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                journal
                    .admit(
                        &context,
                        &token,
                        "query.run",
                        &payload,
                        &receipt,
                        None,
                        spec,
                    )
                    .unwrap()
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    let accepted = admissions[0].clone();
    assert!(admissions
        .iter()
        .all(|value| value.operation_id == accepted.operation_id));
    assert_eq!(
        admissions
            .iter()
            .filter(|value| !value.deduplicated)
            .count(),
        1
    );
    let barrier = Arc::new(Barrier::new(8));
    let sends = Arc::new(AtomicUsize::new(0));
    let threads = (0..8)
        .map(|_| {
            let journal = fixture.journal.clone();
            let context = fixture.context.clone();
            let receipt = fixture.receipt.clone();
            let barrier = barrier.clone();
            let sends = sends.clone();
            let id = accepted.operation_id;
            std::thread::spawn(move || {
                barrier.wait();
                match journal.mark_dispatch(&context, &receipt.scope, id, 1, 11, &receipt) {
                    Ok(permit) => {
                        assert_eq!(
                            journal.get(&context, &receipt.scope, id).unwrap().state,
                            OperationState::Dispatching
                        );
                        assert_eq!(permit.version(), 2);
                        sends.fetch_add(1, Ordering::SeqCst);
                        true
                    }
                    Err(WorkbenchError::IdempotencyConflict) => false,
                    Err(error) => panic!("{error}"),
                }
            })
        })
        .collect::<Vec<_>>();
    let winners = threads
        .into_iter()
        .map(|thread| usize::from(thread.join().unwrap()))
        .sum::<usize>();
    assert_eq!(winners, 1);
    assert_eq!(sends.load(Ordering::SeqCst), 1);
    assert_eq!(
        fixture
            .move_to(accepted.operation_id, 2, Transition::Running)
            .version
            .value(),
        3
    );
    assert_eq!(
        fixture
            .journal
            .transition(
                &fixture.context,
                &fixture.receipt.scope,
                accepted.operation_id,
                2,
                Transition::Running
            )
            .unwrap_err(),
        WorkbenchError::IdempotencyConflict
    );
    eprintln!("M3 boundary_violations=0; local_dispatch_cas_winners={winners}; simulated_sends=1; remote_proof=false");
}

#[test]
fn restart_preserves_uncertainty_and_acknowledgment_is_not_evidence() {
    let fixture = Fixture::new();
    let accepted = fixture.accepted(false);
    let write = fixture.accepted(false);
    let read = fixture.accepted(true);
    let write_version = fixture.dispatch(&write);
    let running = fixture.move_to(write.operation_id, write_version, Transition::Running);
    fixture.move_to(
        write.operation_id,
        running.version.value(),
        Transition::CommitRequested,
    );
    let read_version = fixture.dispatch(&read);
    fixture.move_to(read.operation_id, read_version, Transition::Running);
    let root = fixture.directory.path().to_owned();
    let scope = fixture.receipt.scope.clone();
    let context = fixture.context.clone();
    let clock = fixture.clock.clone();
    drop(fixture.journal);
    drop(fixture.store);
    let store = Arc::new(ClientStore::open(&root, context.principal(), &TestRoot).unwrap());
    let journal =
        OperationJournal::with_clock(store.clone(), JournalLimits::default(), clock.clone())
            .unwrap();
    assert_eq!(journal.recovery_report().cancelled, 1);
    assert_eq!(journal.recovery_report().interrupted, 1);
    assert_eq!(journal.recovery_report().outcome_unknown, 1);
    assert_eq!(
        journal
            .get(&context, &scope, accepted.operation_id)
            .unwrap()
            .state,
        OperationState::Cancelled
    );
    assert_eq!(
        journal
            .get(&context, &scope, read.operation_id)
            .unwrap()
            .state,
        OperationState::Interrupted
    );
    let unknown = journal.get(&context, &scope, write.operation_id).unwrap();
    assert_eq!(unknown.state, OperationState::OutcomeUnknown);
    let acknowledged = journal
        .acknowledge_unknown(
            &context,
            &scope,
            write.operation_id,
            unknown.version.value(),
        )
        .unwrap();
    assert_eq!(acknowledged.state, OperationState::OutcomeUnknown);
    assert!(acknowledged.acknowledged_at_ms.is_some());
    clock.0.fetch_add(31 * 86_400_000, Ordering::SeqCst);
    assert_eq!(journal.tombstone_eligible(&context).unwrap(), 2);
    let unknown_tombstone: Option<i64> = store
        .read(|c| {
            c.query_row(
                "SELECT tombstoned_at_ms FROM operations WHERE operation_id=?1",
                [write.operation_id.to_string()],
                |r| r.get(0),
            )
            .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert!(unknown_tombstone.is_none());
    assert_eq!(
        journal
            .get(&context, &scope, write.operation_id)
            .unwrap()
            .state,
        OperationState::OutcomeUnknown
    );
    eprintln!("M3 boundary_violations=0; recovered_write_unknown=1; recovered_read_interrupted=1; remote_commit_loss_proof=false");
}

#[test]
fn reopened_unknown_survives_explicit_epoch_rotation_and_old_attempts_stay_expired() {
    let fixture = Fixture::new();
    let payload = json!({"reviewId":Uuid::new_v4()});
    let token = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "query.run",
            &payload,
        )
        .unwrap();
    let admitted = fixture
        .journal
        .admit(
            &fixture.context,
            &token.attempt_token,
            "query.run",
            &payload,
            &fixture.receipt,
            None,
            fixture.spec(false),
        )
        .unwrap();
    let dispatched = fixture.dispatch(&admitted);
    let running = fixture.move_to(admitted.operation_id, dispatched, Transition::Running);
    fixture.move_to(
        admitted.operation_id,
        running.version.value(),
        Transition::CommitRequested,
    );
    let root = fixture.directory.path().to_owned();
    let context = fixture.context.clone();
    let receipt = fixture.receipt.clone();
    let spec = fixture.spec(false);
    let clock = fixture.clock.clone();
    drop(fixture.journal);
    drop(fixture.store);
    let store = Arc::new(ClientStore::open(&root, context.principal(), &TestRoot).unwrap());
    let journal =
        OperationJournal::with_clock(store.clone(), JournalLimits::default(), clock).unwrap();
    let recovered = journal
        .get(&context, &receipt.scope, admitted.operation_id)
        .unwrap();
    assert_eq!(recovered.state, OperationState::OutcomeUnknown);
    let historical: (String, String, String) = store
        .read(|connection| {
            connection
                .query_row(
                    "SELECT journal_epoch,idempotency_key,payload_digest FROM operations WHERE operation_id=?1",
                    [admitted.operation_id.to_string()],
                    |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                )
                .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    let rotated = journal.rotate_epoch_after_reconciliation(&context).unwrap();
    assert_ne!(historical.0, rotated.to_string());
    assert_eq!(historical.1, token.attempt_token);
    assert_eq!(historical.2, token.payload_digest);
    assert_eq!(
        journal
            .admit(
                &context,
                &token.attempt_token,
                "query.run",
                &payload,
                &receipt,
                None,
                spec.clone()
            )
            .err(),
        Some(WorkbenchError::AttemptExpired)
    );
    let unchanged = journal
        .get(&context, &receipt.scope, admitted.operation_id)
        .unwrap();
    assert_eq!(unchanged.state, OperationState::OutcomeUnknown);
    assert_eq!(
        unchanged.transaction_outcome,
        TransactionOutcome::OutcomeUnknown
    );
    assert_eq!(unchanged.version, recovered.version);
    let fresh = journal
        .prepare(&context, &receipt, spec.session, "query.run", &payload)
        .unwrap();
    assert_eq!(
        journal
            .admit(
                &context,
                &fresh.attempt_token,
                "query.run",
                &payload,
                &receipt,
                None,
                spec.clone()
            )
            .err(),
        Some(WorkbenchError::OutcomeUnknown)
    );
    let acknowledged = journal
        .acknowledge_unknown(
            &context,
            &receipt.scope,
            admitted.operation_id,
            unchanged.version.value(),
        )
        .unwrap();
    assert_eq!(acknowledged.state, OperationState::OutcomeUnknown);
    assert_eq!(
        journal
            .admit(
                &context,
                &token.attempt_token,
                "query.run",
                &payload,
                &receipt,
                None,
                spec
            )
            .err(),
        Some(WorkbenchError::AttemptExpired)
    );
    let stored: (String, String, String) = store
        .read(|connection| {
            connection
                .query_row(
                    "SELECT journal_epoch,idempotency_key,payload_digest FROM operations WHERE operation_id=?1",
                    [admitted.operation_id.to_string()],
                    |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                )
                .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert_eq!(stored, historical);
    eprintln!("M3 boundary_violations=0; reopened_unknown_preserved=true; explicit_epoch_rotation_not_reconciliation_proof=true; old_token_dispatches=0; os_standard_integrity_e2e=false");
}

#[test]
fn hints_ignore_duplicate_and_old_versions_and_pull_on_gap_or_reconnect() {
    let fixture = Fixture::new();
    let operation = fixture.accepted(true);
    let mut tracker = VersionTracker::new(32).unwrap();
    let hint = |version: u64| OperationHint {
        operation_id: operation.operation_id,
        version: version.into(),
    };
    assert_eq!(tracker.observe(&hint(1)).unwrap(), HintAction::ApplyNext);
    tracker.applied_snapshot(operation.operation_id, 1).unwrap();
    assert_eq!(tracker.observe(&hint(1)).unwrap(), HintAction::IgnoreOld);
    assert_eq!(tracker.observe(&hint(5)).unwrap(), HintAction::PullRequired);
    let snapshot = fixture
        .journal
        .get(
            &fixture.context,
            &fixture.receipt.scope,
            operation.operation_id,
        )
        .unwrap();
    tracker
        .applied_snapshot(snapshot.operation_id, snapshot.version.value())
        .unwrap();
    tracker.reconnect();
    assert_eq!(tracker.observe(&hint(5)).unwrap(), HintAction::PullRequired);
}

#[test]
fn local_commit_failure_after_dispatch_projects_unknown_and_disables_new_writes() {
    let fixture = Fixture::new();
    let operation = fixture.accepted(false);
    let version = fixture.dispatch(&operation);
    let running = fixture.move_to(operation.operation_id, version, Transition::Running);
    let commit = fixture.move_to(
        operation.operation_id,
        running.version.value(),
        Transition::CommitRequested,
    );
    fixture
        .store
        .write(|c| {
            let commits = AtomicUsize::new(0);
            // Permit the anchor header revision that seals hook installation;
            // veto the following actual operation-finalization transaction.
            c.commit_hook(Some(move || commits.fetch_add(1, Ordering::SeqCst) >= 1))
                .map_err(|_| WorkbenchError::StoreUnavailable)?;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        fixture
            .journal
            .transition(
                &fixture.context,
                &fixture.receipt.scope,
                operation.operation_id,
                commit.version.value(),
                Transition::CommitConfirmed
            )
            .unwrap_err(),
        WorkbenchError::StoreUnavailable
    );
    assert!(!fixture.journal.write_enabled());
    let observed = fixture
        .journal
        .get(
            &fixture.context,
            &fixture.receipt.scope,
            operation.operation_id,
        )
        .unwrap();
    assert_eq!(observed.state, OperationState::OutcomeUnknown);
    assert_eq!(
        observed.transaction_outcome,
        TransactionOutcome::OutcomeUnknown
    );
    let unknown_list = fixture
        .journal
        .list(
            &fixture.context,
            &fixture.receipt.scope,
            Some(OperationState::OutcomeUnknown),
            None,
            100,
        )
        .unwrap();
    assert!(unknown_list
        .iter()
        .any(|item| item.operation_id == operation.operation_id
            && item.state == OperationState::OutcomeUnknown));
    let raw: String = fixture
        .store
        .read(|c| {
            c.query_row(
                "SELECT state FROM operations WHERE operation_id=?1",
                [operation.operation_id.to_string()],
                |r| r.get(0),
            )
            .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    assert_eq!(raw, "commit_requested");
    eprintln!("M3 boundary_violations=0; local_finalize_veto=1; durable_commit_marker_preserved=true; remote_proof=false");
}

#[test]
fn actual_sqlite_full_and_corruption_are_fail_closed() {
    let fixture = Fixture::new();
    fixture.accepted(false);
    let full=fixture.store.write(|c|{
  let pages:i64=c.pragma_query_value(None,"page_count",|r|r.get(0)).unwrap();c.pragma_update(None,"max_page_count",pages).unwrap();
  let result=c.execute("INSERT INTO saved_queries(owner_id,workspace_id,target_id,binding_id,query_id,encryption_format,encrypted_content,version,created_at_ms,updated_at_ms) SELECT owner_id,workspace_id,target_id,binding_id,?1,'current-user-dpapi-v1',zeroblob(262144),1,0,0 FROM target_bindings LIMIT 1",[Uuid::new_v4().to_string()]);
  assert_eq!(result.unwrap_err().sqlite_error_code(),Some(rusqlite::ErrorCode::DiskFull));Ok(())
 });
    assert!(full.is_ok());
    let mut saw_failure = false;
    for index in 0..500 {
        let payload = json!({"reviewId":Uuid::new_v4(),"ordinal":index});
        let token = fixture
            .journal
            .prepare(
                &fixture.context,
                &fixture.receipt,
                Some(fixture.session),
                "query.run",
                &payload,
            )
            .unwrap();
        match fixture.journal.admit(
            &fixture.context,
            &token.attempt_token,
            "query.run",
            &payload,
            &fixture.receipt,
            None,
            fixture.spec(false),
        ) {
            Ok(_) => {}
            Err(WorkbenchError::StoreUnavailable) => {
                saw_failure = true;
                break;
            }
            Err(error) => panic!("unexpected {error}"),
        }
    }
    assert!(saw_failure);
    assert!(!fixture.journal.write_enabled());
    let root = fixture.directory.path().to_owned();
    let principal = fixture.context.principal().clone();
    drop(fixture.journal);
    drop(fixture.store);
    let path = root.join("client-state.sqlite3");
    std::fs::write(&path, b"intentionally corrupt journal fixture").unwrap();
    assert!(ClientStore::open(&root, &principal, &TestRoot).is_err());
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"intentionally corrupt journal fixture"
    );
    eprintln!(
        "M3 boundary_violations=0; sqlite_full_observed=true; corrupted_file_recreated=false"
    );
}

#[test]
fn metadata_journal_and_wal_never_contain_request_canaries() {
    let fixture = Fixture::new();
    let payload = json!({"sql":"SQL_CANARY_719a select never persist","parameters":["PARAM_CANARY_e781"],"row":"ROW_CANARY_ba98","secret":"SECRET_CANARY_32cf"}); // gitleaks:allow -- synthetic journal privacy canaries, never authentication credentials
    fixture.admit("query.run", payload, fixture.spec(false));
    for entry in std::fs::read_dir(fixture.directory.path()).unwrap() {
        let path = entry.unwrap().path();
        // Windows byte-range locks cover the process-lock/SHM metadata. SQL
        // payload storage is the database and WAL, both scanned while active.
        if matches!(
            path.file_name().and_then(|n| n.to_str()),
            Some("client-state.sqlite3" | "client-state.sqlite3-wal")
        ) {
            let bytes = std::fs::read(path).unwrap();
            for canary in [
                b"SQL_CANARY_719a".as_slice(),
                b"PARAM_CANARY_e781",
                b"ROW_CANARY_ba98",
                b"SECRET_CANARY_32cf",
            ] {
                assert!(!bytes.windows(canary.len()).any(|window| window == canary));
            }
        }
    }
    let protected = restoreplane_secrets::protect_current_user(
        b"SECRET_CANARY_32cf",
        "invalid purpose with spaces",
    );
    assert!(protected.is_err());
    eprintln!("M3 boundary_violations=0; persisted_raw_canaries=0; plaintext_fallback=0");
}

#[test]
fn manual_commit_reuses_operation_and_settles_linked_statement_evidence_atomically() {
    let fixture = Fixture::new();
    let mut transaction = fixture.spec(false);
    transaction.kind = OperationKind::ManualTransaction;
    transaction.statements.clear();
    let transaction = fixture.admit(
        "transaction.begin",
        json!({"session":fixture.session,"isolation":"serializable","readOnly":false}),
        transaction,
    );
    let tx_version = fixture.dispatch(&transaction);
    let active = fixture.move_to(transaction.operation_id, tx_version, Transition::Running);
    assert_eq!(active.transaction_outcome, TransactionOutcome::Open);
    let mut spec = fixture.spec(false);
    spec.transaction_id = Some(transaction.operation_id);
    spec.statements[0].transaction_id = Some(transaction.operation_id);
    let query = fixture.admit(
        "query.run",
        json!({"session":fixture.session,"reviewId":Uuid::new_v4()}),
        spec,
    );
    let version = fixture.dispatch(&query);
    let running = fixture.move_to(query.operation_id, version, Transition::Running);
    let sent = fixture
        .journal
        .record_statement(
            &fixture.context,
            &fixture.receipt.scope,
            query.operation_id,
            running.version.value(),
            StatementUpdate {
                ordinal: 0,
                expected_version: 1,
                outcome: StatementStatus::Sent,
                effect_evidence: EffectEvidence::None,
                rows_matched: None,
                rows_changed: None,
                error_code: None,
            },
        )
        .unwrap();
    let succeeded = fixture
        .journal
        .record_statement(
            &fixture.context,
            &fixture.receipt.scope,
            query.operation_id,
            sent.version.value(),
            StatementUpdate {
                ordinal: 0,
                expected_version: 2,
                outcome: StatementStatus::Succeeded,
                effect_evidence: EffectEvidence::PendingTransaction,
                rows_matched: Some(1),
                rows_changed: Some(0),
                error_code: None,
            },
        )
        .unwrap();
    let complete = fixture.move_to(
        query.operation_id,
        succeeded.version.value(),
        Transition::StatementGroupCompleted(ResultCompleteness::Complete),
    );
    assert_eq!(complete.state, OperationState::Completed);
    assert_eq!(complete.transaction_outcome, TransactionOutcome::Open);
    assert_eq!(
        complete.statements[0].effect_evidence,
        EffectEvidence::PendingTransaction
    );
    let payload = json!({"session":fixture.session,"transactionId":transaction.operation_id});
    let token = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "transaction.commit",
            &payload,
        )
        .unwrap();
    let commit = fixture
        .journal
        .request_commit(
            &fixture.context,
            &token.attempt_token,
            &payload,
            &fixture.receipt,
            active.version.value(),
        )
        .unwrap();
    assert!(commit.permit.is_some());
    assert_eq!(commit.admission.operation_id, transaction.operation_id);
    let duplicate = fixture
        .journal
        .request_commit(
            &fixture.context,
            &token.attempt_token,
            &payload,
            &fixture.receipt,
            active.version.value(),
        )
        .unwrap();
    assert!(duplicate.admission.deduplicated);
    assert!(duplicate.permit.is_none());
    fixture.move_to(
        transaction.operation_id,
        commit.admission.version.value(),
        Transition::CommitConfirmed,
    );
    let final_query = fixture
        .journal
        .get(&fixture.context, &fixture.receipt.scope, query.operation_id)
        .unwrap();
    assert_eq!(final_query.state, OperationState::Completed);
    assert_eq!(
        final_query.transaction_outcome,
        TransactionOutcome::Committed
    );
    assert_eq!(final_query.transaction_id, Some(transaction.operation_id));
    assert_eq!(
        final_query.statements[0].transaction_id,
        Some(transaction.operation_id)
    );
    assert_eq!(
        final_query.statements[0].effect_evidence,
        EffectEvidence::Committed
    );
    assert_eq!(final_query.statements[0].rows_matched, Some(1u64.into()));
    assert_eq!(final_query.statements[0].rows_changed, Some(0u64.into()));
    eprintln!("M3 boundary_violations=0; commit_operation_reused=true; commit_permits=1; zero_changed_not_zero_matched=true");
}

#[test]
fn import_children_bind_parent_ranges_and_unknown_blocks_the_next_batch() {
    let fixture = Fixture::new();
    let mut parent_spec = fixture.spec(false);
    parent_spec.kind = OperationKind::ImportJob;
    parent_spec.statements.clear();
    let parent = fixture.admit(
        "import.start",
        json!({"session":fixture.session,"previewId":Uuid::new_v4()}),
        parent_spec,
    );
    let version = fixture.dispatch(&parent);
    let parent_running = fixture.move_to(parent.operation_id, version, Transition::Running);
    let child_spec = |index: u32| {
        let mut spec = fixture.spec(false);
        spec.kind = OperationKind::ImportBatch;
        spec.batch = Some(ImportBatch {
            parent_operation_id: parent.operation_id,
            batch_index: index,
            source_digest: "d".repeat(64),
            batch_digest: "e".repeat(64),
            byte_start: u64::from(index) * 100,
            byte_end: u64::from(index + 1) * 100,
            row_start: u64::from(index) * 10,
            row_end: u64::from(index + 1) * 10,
        });
        spec
    };
    let first=fixture.admit("import.continue",json!({"session":fixture.session,"parentOperationId":parent.operation_id,"reviewId":Uuid::new_v4()}),child_spec(0));
    let v = fixture.dispatch(&first);
    let running = fixture.move_to(first.operation_id, v, Transition::Running);
    fixture.move_to(
        first.operation_id,
        running.version.value(),
        Transition::CommitConfirmed,
    );
    let second=fixture.admit("import.continue",json!({"session":fixture.session,"parentOperationId":parent.operation_id,"reviewId":Uuid::new_v4()}),child_spec(1));
    let v = fixture.dispatch(&second);
    fixture.move_to(second.operation_id, v, Transition::EffectsUnknown);
    let payload = json!({"session":fixture.session,"parentOperationId":parent.operation_id,"reviewId":Uuid::new_v4()});
    let token = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "import.continue",
            &payload,
        )
        .unwrap();
    assert_eq!(
        fixture
            .journal
            .admit(
                &fixture.context,
                &token.attempt_token,
                "import.continue",
                &payload,
                &fixture.receipt,
                None,
                child_spec(2)
            )
            .unwrap_err(),
        WorkbenchError::OutcomeUnknown
    );
    assert_eq!(
        fixture
            .journal
            .transition(
                &fixture.context,
                &fixture.receipt.scope,
                parent.operation_id,
                parent_running.version.value(),
                Transition::JobCompleted
            )
            .unwrap_err(),
        WorkbenchError::OutcomeUnknown
    );
    let parent_final = fixture.move_to(
        parent.operation_id,
        parent_running.version.value(),
        Transition::Partial,
    );
    assert_eq!(parent_final.state, OperationState::OutcomeUnknown);
    let child = fixture
        .journal
        .get(&fixture.context, &fixture.receipt.scope, first.operation_id)
        .unwrap();
    assert_eq!(child.parent_operation_id, Some(parent.operation_id));
    assert_eq!(child.batch_index, Some(0));
    eprintln!("M3 boundary_violations=0; unknown_batch_halts_next=true; committed_prior_child_preserved=true");
}

#[test]
fn epoch_rotation_and_quota_do_not_reenable_old_attempts() {
    let fixture = Fixture::new();
    let payload = json!({"reviewId":Uuid::new_v4()});
    let old = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "query.run",
            &payload,
        )
        .unwrap();
    let original = fixture.store.journal_epoch().to_owned();
    let rotated = fixture
        .journal
        .rotate_epoch_after_reconciliation(&fixture.context)
        .unwrap();
    assert_ne!(original, rotated.to_string());
    assert_eq!(
        fixture
            .journal
            .admit(
                &fixture.context,
                &old.attempt_token,
                "query.run",
                &payload,
                &fixture.receipt,
                None,
                fixture.spec(false)
            )
            .unwrap_err(),
        WorkbenchError::AttemptExpired
    );
    let fresh = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "query.run",
            &payload,
        )
        .unwrap();
    fixture
        .journal
        .admit(
            &fixture.context,
            &fresh.attempt_token,
            "query.run",
            &payload,
            &fixture.receipt,
            None,
            fixture.spec(false),
        )
        .unwrap();
    let limited = OperationJournal::with_clock(
        fixture.store.clone(),
        JournalLimits {
            operations: 1,
            ..JournalLimits::default()
        },
        fixture.clock.clone(),
    )
    .unwrap();
    let token = limited
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "query.run",
            &payload,
        )
        .unwrap();
    assert_eq!(
        limited
            .admit(
                &fixture.context,
                &token.attempt_token,
                "query.run",
                &payload,
                &fixture.receipt,
                None,
                fixture.spec(false)
            )
            .unwrap_err(),
        WorkbenchError::Limit
    );
    assert!(limited.write_enabled());
}

#[test]
fn mixed_statement_transaction_links_preserve_confirmed_commit_and_recover_pending_unknown() {
    let fixture = Fixture::new();
    let mut transaction = fixture.spec(false);
    transaction.kind = OperationKind::ManualTransaction;
    transaction.statements.clear();
    let transaction = fixture.admit(
        "transaction.begin",
        json!({"session":fixture.session,"isolation":"serializable","readOnly":false}),
        transaction,
    );
    let version = fixture.dispatch(&transaction);
    fixture.move_to(transaction.operation_id, version, Transition::Running);
    let mut spec = fixture.spec(false);
    spec.statements[0].transaction_id = Some(transaction.operation_id);
    spec.statements.push(StatementSpec {
        digest: "f".repeat(64),
        effect_class: EffectClass::Ddl,
        certified_read_only: false,
        transaction_id: None,
    });
    let operation = fixture.admit(
        "query.run",
        json!({"session":fixture.session,"reviewId":Uuid::new_v4()}),
        spec,
    );
    let version = fixture.dispatch(&operation);
    let mut current = fixture.move_to(operation.operation_id, version, Transition::Running);
    for (ordinal, evidence) in [
        (0, EffectEvidence::PendingTransaction),
        (1, EffectEvidence::Committed),
    ] {
        current = fixture
            .journal
            .record_statement(
                &fixture.context,
                &fixture.receipt.scope,
                operation.operation_id,
                current.version.value(),
                StatementUpdate {
                    ordinal,
                    expected_version: 1,
                    outcome: StatementStatus::Sent,
                    effect_evidence: EffectEvidence::None,
                    rows_matched: None,
                    rows_changed: None,
                    error_code: None,
                },
            )
            .unwrap();
        current = fixture
            .journal
            .record_statement(
                &fixture.context,
                &fixture.receipt.scope,
                operation.operation_id,
                current.version.value(),
                StatementUpdate {
                    ordinal,
                    expected_version: 2,
                    outcome: StatementStatus::Succeeded,
                    effect_evidence: evidence,
                    rows_matched: None,
                    rows_changed: Some(1),
                    error_code: None,
                },
            )
            .unwrap();
    }
    let complete = fixture.move_to(
        operation.operation_id,
        current.version.value(),
        Transition::StatementGroupCompleted(ResultCompleteness::Complete),
    );
    assert_eq!(complete.effect_summary, EffectSummary::Partial);
    assert!(complete.transaction_id.is_none());
    let root = fixture.directory.path().to_owned();
    let context = fixture.context.clone();
    let scope = fixture.receipt.scope.clone();
    let clock = fixture.clock.clone();
    drop(fixture.journal);
    drop(fixture.store);
    let store = Arc::new(ClientStore::open(&root, context.principal(), &TestRoot).unwrap());
    let journal = OperationJournal::with_clock(store, JournalLimits::default(), clock).unwrap();
    let recovered = journal
        .get(&context, &scope, operation.operation_id)
        .unwrap();
    assert_eq!(recovered.state, OperationState::OutcomeUnknown);
    assert_eq!(recovered.effect_summary, EffectSummary::Unknown);
    assert_eq!(
        recovered.statements[0].effect_evidence,
        EffectEvidence::OutcomeUnknown
    );
    assert_eq!(
        recovered.statements[1].effect_evidence,
        EffectEvidence::Committed
    );
    eprintln!("M3 boundary_violations=0; mixed_pending_unknown=true; independent_committed_evidence_preserved=true");
}

#[test]
fn admitted_tokens_evict_pending_memory_and_durable_retries_survive_1024_runs() {
    let fixture = Fixture::new();
    let mut first = None;
    for index in 0..1025 {
        let payload = json!({"session":fixture.session,"sequence":index});
        let token = fixture
            .journal
            .prepare(
                &fixture.context,
                &fixture.receipt,
                Some(fixture.session),
                "query.run",
                &payload,
            )
            .unwrap();
        let admission = fixture
            .journal
            .admit(
                &fixture.context,
                &token.attempt_token,
                "query.run",
                &payload,
                &fixture.receipt,
                None,
                fixture.spec(true),
            )
            .unwrap();
        fixture.move_to(
            admission.operation_id,
            admission.version.value(),
            Transition::CancelledBeforeSend,
        );
        if first.is_none() {
            first = Some((token, payload, admission.operation_id));
        }
    }
    let (old, payload, id) = first.unwrap();
    let duplicate = fixture
        .journal
        .admit(
            &fixture.context,
            &old.attempt_token,
            "query.run",
            &payload,
            &fixture.receipt,
            None,
            fixture.spec(true),
        )
        .unwrap();
    assert!(duplicate.deduplicated);
    assert_eq!(duplicate.operation_id, id);
    assert_eq!(duplicate.version.value(), 2);
    for index in 0..1024 {
        fixture
            .journal
            .prepare(
                &fixture.context,
                &fixture.receipt,
                Some(fixture.session),
                "query.run",
                &json!({"pending":index}),
            )
            .unwrap();
    }
    assert_eq!(
        fixture
            .journal
            .prepare(
                &fixture.context,
                &fixture.receipt,
                Some(fixture.session),
                "query.run",
                &json!({"pending":"overflow"})
            )
            .unwrap_err(),
        WorkbenchError::Limit
    );
    let duplicate = fixture
        .journal
        .admit(
            &fixture.context,
            &old.attempt_token,
            "query.run",
            &payload,
            &fixture.receipt,
            None,
            fixture.spec(true),
        )
        .unwrap();
    assert_eq!(duplicate.operation_id, id);
    assert_eq!(
        fixture
            .store
            .read(|c| c
                .query_row("SELECT COUNT(*) FROM operations", [], |r| r
                    .get::<_, i64>(0))
                .map_err(|_| WorkbenchError::StoreUnavailable))
            .unwrap(),
        1025
    );
    eprintln!("M3 boundary_violations=0; sequential_admissions=1025; native_pending_cap=1024; historical_retry_same_operation=true");
}

#[test]
fn evicted_commit_token_cannot_be_retargeted_or_forged_for_terminal_status() {
    let fixture = Fixture::new();
    let finish = || {
        let mut spec = fixture.spec(false);
        spec.kind = OperationKind::ManualTransaction;
        spec.statements.clear();
        let transaction = fixture.admit(
            "transaction.begin",
            json!({"session":fixture.session,"isolation":"serializable","readOnly":false}),
            spec,
        );
        let version = fixture.dispatch(&transaction);
        let active = fixture.move_to(transaction.operation_id, version, Transition::Running);
        let payload = json!({"session":fixture.session,"transactionId":transaction.operation_id});
        let token = fixture
            .journal
            .prepare(
                &fixture.context,
                &fixture.receipt,
                Some(fixture.session),
                "transaction.commit",
                &payload,
            )
            .unwrap();
        assert!(token.attempt_token.len() <= 512);
        let commit = fixture
            .journal
            .request_commit(
                &fixture.context,
                &token.attempt_token,
                &payload,
                &fixture.receipt,
                active.version.value(),
            )
            .unwrap();
        let complete = fixture.move_to(
            transaction.operation_id,
            commit.admission.version.value(),
            Transition::CommitConfirmed,
        );
        (token, payload, complete)
    };
    let (a, payload_a, op_a) = finish();
    let (_b, payload_b, op_b) = finish();
    assert_eq!(
        fixture
            .journal
            .request_commit(
                &fixture.context,
                &a.attempt_token,
                &payload_b,
                &fixture.receipt,
                op_b.version.value()
            )
            .err(),
        Some(WorkbenchError::IdempotencyConflict)
    );
    let mut forged = a.attempt_token.clone();
    let final_byte = forged.pop().unwrap();
    forged.push(if final_byte == '0' { '1' } else { '0' });
    assert_eq!(
        fixture
            .journal
            .request_commit(
                &fixture.context,
                &forged,
                &payload_a,
                &fixture.receipt,
                op_a.version.value()
            )
            .err(),
        Some(WorkbenchError::IdempotencyConflict)
    );
    let same = fixture
        .journal
        .request_commit(
            &fixture.context,
            &a.attempt_token,
            &payload_a,
            &fixture.receipt,
            op_a.version.value(),
        )
        .unwrap();
    assert!(same.admission.deduplicated);
    assert!(same.permit.is_none());
    assert_eq!(same.admission.operation_id, op_a.operation_id);
    fixture
        .journal
        .rotate_epoch_after_reconciliation(&fixture.context)
        .unwrap();
    assert_eq!(
        fixture
            .journal
            .request_commit(
                &fixture.context,
                &a.attempt_token,
                &payload_a,
                &fixture.receipt,
                op_a.version.value()
            )
            .err(),
        Some(WorkbenchError::AttemptExpired)
    );
    eprintln!("M3 boundary_violations=0; retargeted_commit_rejected=true; forged_terminal_token_rejected=true; evicted_valid_commit_status_only=true");
}

#[test]
fn unacknowledged_unknown_quarantines_new_mutations_and_requires_a_fresh_intent() {
    let fixture = Fixture::new();
    let uncertain = fixture.accepted(false);
    let version = fixture.dispatch(&uncertain);
    let unknown = fixture.move_to(uncertain.operation_id, version, Transition::EffectsUnknown);
    let mut spec = fixture.spec(false);
    spec.kind = OperationKind::VerifiedApply;
    let payload =
        json!({"session":fixture.session,"draftId":Uuid::new_v4(),"reviewId":Uuid::new_v4()});
    let blocked = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(fixture.session),
            "edit.apply",
            &payload,
        )
        .unwrap();
    assert_eq!(
        fixture
            .journal
            .admit(
                &fixture.context,
                &blocked.attempt_token,
                "edit.apply",
                &payload,
                &fixture.receipt,
                None,
                spec.clone()
            )
            .unwrap_err(),
        WorkbenchError::OutcomeUnknown
    );
    let read = fixture.accepted(true);
    assert!(fixture.dispatch(&read) > 1);
    let acknowledged = fixture
        .journal
        .acknowledge_unknown(
            &fixture.context,
            &fixture.receipt.scope,
            uncertain.operation_id,
            unknown.version.value(),
        )
        .unwrap();
    assert_eq!(acknowledged.state, OperationState::OutcomeUnknown);
    assert_eq!(
        fixture
            .journal
            .admit(
                &fixture.context,
                &blocked.attempt_token,
                "edit.apply",
                &payload,
                &fixture.receipt,
                None,
                spec.clone()
            )
            .unwrap_err(),
        WorkbenchError::AttemptExpired
    );
    let next_session = SessionRef {
        session_id: Uuid::new_v4(),
        connection_generation: 2u64.into(),
    };
    spec.session = Some(next_session);
    let fresh_payload =
        json!({"session":next_session,"draftId":Uuid::new_v4(),"reviewId":Uuid::new_v4()});
    let fresh = fixture
        .journal
        .prepare(
            &fixture.context,
            &fixture.receipt,
            Some(next_session),
            "edit.apply",
            &fresh_payload,
        )
        .unwrap();
    let next = fixture
        .journal
        .admit(
            &fixture.context,
            &fresh.attempt_token,
            "edit.apply",
            &fresh_payload,
            &fixture.receipt,
            None,
            spec,
        )
        .unwrap();
    assert_ne!(next.operation_id, uncertain.operation_id);
    assert_eq!(
        fixture
            .journal
            .get(
                &fixture.context,
                &fixture.receipt.scope,
                uncertain.operation_id
            )
            .unwrap()
            .state,
        OperationState::OutcomeUnknown
    );
    eprintln!("M3 boundary_violations=0; unacknowledged_unknown_blocks_mutation=true; readonly_inspection_allowed=true; acknowledged_history_preserved=true; fresh_intent_required=true");
}

#[derive(Default)]
struct LiveSink {
    rows: Vec<Vec<TypedCell>>,
}
#[async_trait]
impl RowSink for LiveSink {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, row: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        self.rows.push(row);
        Ok(())
    }
}
fn owned_pg_fixture() -> (ConnectDescriptor, DriverLogin) {
    let path =
        std::env::var_os("WORKBENCH_LIVE_FIXTURES").expect("explicit owned fixture required");
    let bytes = Zeroizing::new(std::fs::read(path).expect("owned fixture unavailable"));
    let config: Value = serde_json::from_slice(&bytes).expect("owned fixture format");
    let pg = &config["postgresql"];
    let ca = pg["ca_file"].as_str().expect("owned fixture CA");
    let ca_bytes = std::fs::read(ca).expect("owned fixture CA unavailable");
    let host = pg["host"].as_str().expect("owned fixture host");
    assert!(
        matches!(host, "localhost" | "127.0.0.1" | "::1"),
        "disposable loopback fixture only"
    );
    let descriptor = ConnectDescriptor {
        engine: Engine::Postgresql,
        route: Route::Tcp {
            host: host.into(),
            port: u16::try_from(pg["port"].as_u64().unwrap()).unwrap(),
        },
        database: DatabaseIdentity {
            engine_object_id: "owned-process-fault-fixture".into(),
            name: pg["database"].as_str().unwrap().into(),
            source_fingerprint: "owned-process-fault-fixture".into(),
        },
        tls: Tls {
            mode: "verify-full".into(),
            server_name: pg
                .get("server_name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            root_trust: Some(RootTrust::Custom {
                canonical_path: ca.into(),
                content_digest: blake3::hash(&ca_bytes).to_hex().to_string(),
            }),
        },
        transport_options: Default::default(),
        expected_server_identity: None,
    };
    let login = DriverLogin {
        username: pg["username"].as_str().unwrap().into(),
        secret: Zeroizing::new(pg["password"].as_str().unwrap().into()),
    };
    (descriptor, login)
}
fn fault_table() -> String {
    let table = std::env::var("WORKBENCH_FAULT_TABLE").expect("owned table required");
    assert!(
        table.starts_with("wb_commit_fault_")
            && table.len() == 48
            && table
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    );
    table
}

#[test]
#[ignore = "child process helper; run through the owned PG process-exit parent test"]
fn live_pg_commit_fault_child() {
    assert_eq!(std::env::var("WORKBENCH_FAULT_CHILD").as_deref(), Ok("1"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async{
  let table=fault_table();let row=Zeroizing::new(std::env::var("WORKBENCH_FAULT_ROW").unwrap());
  let root=std::path::PathBuf::from(std::env::var_os("WORKBENCH_FAULT_JOURNAL").unwrap());
  // Logical principal fixtures exercise the journal; this is not OS standard-
  // integrity or Tauri authorization E2E. The database connection/TLS are real.
  let principal=NativePrincipal::new("a".repeat(64),7,IntegrityLevel::Standard).unwrap();let context=InvocationContext::new(principal.clone(),Uuid::new_v4());
  let store=Arc::new(ClientStore::open(&root,&principal,&TestRoot).unwrap());let journal=OperationJournal::open(store).unwrap();
  let (descriptor,login)=owned_pg_fixture();let mut connection=NativeDriverFactory::default().connect(&descriptor,login,Mode::Full).await.expect("owned TLS connect failed; redacted");assert!(connection.facts().verified_tls);
  let now=SystemClock.now_ms().unwrap() as u64;let mut receipt=BindingReceipt{scope:Scope{workspace_id:WorkspaceId::new(),connection_profile_id:ConnectionProfileId::new(),database_target_id:DatabaseTargetId::new()},descriptor,descriptor_revision:"b".repeat(64),authority_epoch:Uuid::new_v4().to_string(),authorized_at_ms:now.into(),read_lease_expires_at_ms:None,observed_conflicts:vec![]};
  let session=SessionRef{session_id:Uuid::new_v4(),connection_generation:1u64.into()};
  let insert=format!("INSERT INTO \"{table}\"(id,value) VALUES($1::text,$2::bigint) /* SQL_CANARY_REMOTE_712c */");
  let begin=json!({"session":session,"isolation":"read_committed","readOnly":false});let token=journal.prepare(&context,&receipt,Some(session),"transaction.begin",&begin).unwrap();
  let admission=journal.admit(&context,&token.attempt_token,"transaction.begin",&begin,&receipt,None,AdmissionSpec{kind:OperationKind::ManualTransaction,session:Some(session),transaction_id:None,statements:vec![StatementSpec{digest:blake3::hash(b"BEGIN").to_hex().to_string(),effect_class:EffectClass::Transaction,certified_read_only:false,transaction_id:None},StatementSpec{digest:blake3::hash(insert.as_bytes()).to_hex().to_string(),effect_class:EffectClass::Write,certified_read_only:false,transaction_id:None}],batch:None}).unwrap();
  let marker=journal.mark_dispatch(&context,&receipt.scope,admission.operation_id,1,1,&receipt).unwrap();let mut operation=journal.transition(&context,&receipt.scope,admission.operation_id,marker.version(),Transition::Running).unwrap();
  operation=journal.record_statement(&context,&receipt.scope,admission.operation_id,operation.version.value(),StatementUpdate{ordinal:0,expected_version:1,outcome:StatementStatus::Sent,effect_evidence:EffectEvidence::None,rows_matched:None,rows_changed:None,error_code:None}).unwrap();
  connection.execute("BEGIN",QueryLimits::default(),&mut LiveSink::default()).await.expect("owned BEGIN failed; redacted");
  operation=journal.record_statement(&context,&receipt.scope,admission.operation_id,operation.version.value(),StatementUpdate{ordinal:0,expected_version:2,outcome:StatementStatus::Succeeded,effect_evidence:EffectEvidence::PendingTransaction,rows_matched:None,rows_changed:None,error_code:None}).unwrap();
  operation=journal.record_statement(&context,&receipt.scope,admission.operation_id,operation.version.value(),StatementUpdate{ordinal:1,expected_version:1,outcome:StatementStatus::Sent,effect_evidence:EffectEvidence::None,rows_matched:None,rows_changed:None,error_code:None}).unwrap();
  let parameters=[TypedCell::Text{value:row.to_string()},TypedCell::I64{value:"41".into()}];
  let inserted=connection.execute_bound(&insert,&parameters,QueryLimits::default(),&mut LiveSink::default()).await.expect("owned bound INSERT failed; redacted");assert_eq!(inserted.rows_affected,Some(1));
  operation=journal.record_statement(&context,&receipt.scope,admission.operation_id,operation.version.value(),StatementUpdate{ordinal:1,expected_version:2,outcome:StatementStatus::Succeeded,effect_evidence:EffectEvidence::PendingTransaction,rows_matched:Some(1),rows_changed:Some(1),error_code:None}).unwrap();
  receipt.authorized_at_ms=(SystemClock.now_ms().unwrap() as u64).into();let commit=json!({"session":session,"transactionId":admission.operation_id});let token=journal.prepare(&context,&receipt,Some(session),"transaction.commit",&commit).unwrap();
  let commit=journal.request_commit(&context,&token.attempt_token,&commit,&receipt,operation.version.value()).unwrap();assert!(commit.permit.is_some());
  connection.execute("COMMIT",QueryLimits::default(),&mut LiveSink::default()).await.expect("owned COMMIT acknowledgment failed; redacted");
  // Intentional process termination after a positive server COMMIT response and
  // before any local committed finalize. No Rust destructors run on this path.
  std::process::exit(86);
 });
}

#[tokio::test]
#[ignore = "requires explicit disposable WORKBENCH_LIVE_FIXTURES; creates and drops one owned PG table"]
async fn live_pg_committed_before_local_finalize_process_exit() {
    let root = tempfile::tempdir().unwrap();
    let table = format!("wb_commit_fault_{}", Uuid::new_v4().simple());
    let row = Zeroizing::new(format!("PARAM_CANARY_REMOTE_{}", Uuid::new_v4().simple()));
    let (descriptor, login) = owned_pg_fixture();
    let mut setup = NativeDriverFactory::default()
        .connect(&descriptor, login, Mode::Full)
        .await
        .expect("owned fixture setup TLS failed; redacted");
    assert!(setup.facts().verified_tls);
    setup
        .execute(
            &format!("CREATE TABLE \"{table}\"(id TEXT NOT NULL,value BIGINT NOT NULL)"),
            QueryLimits::default(),
            &mut LiveSink::default(),
        )
        .await
        .expect("owned fixture table setup failed; redacted");
    setup.close().await.unwrap();
    drop(setup);
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "live_pg_commit_fault_child",
            "--ignored",
            "--nocapture",
        ])
        .env("WORKBENCH_FAULT_CHILD", "1")
        .env("WORKBENCH_FAULT_TABLE", &table)
        .env("WORKBENCH_FAULT_ROW", row.as_str())
        .env("WORKBENCH_FAULT_JOURNAL", root.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().expect("native fault child launch failed");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            break child.wait().unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let principal = NativePrincipal::new("a".repeat(64), 7, IntegrityLevel::Standard).unwrap();
    let context = InvocationContext::new(principal.clone(), Uuid::new_v4());
    let store = Arc::new(ClientStore::open(root.path(), &principal, &TestRoot).unwrap());
    let raw: Option<(String, String, i64)> = store
        .read(|c| {
            c.query_row(
                "SELECT operation_id,state,dispatch_count FROM operations LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(|_| WorkbenchError::StoreUnavailable)
        })
        .unwrap();
    let journal = OperationJournal::open(store.clone()).unwrap();
    let recovered = raw.as_ref().map(|(id, _, _)| {
        let id = Uuid::parse_str(id).unwrap();
        let scope = journal.scope_for_operation(&context, id).unwrap();
        journal.get(&context, &scope, id).unwrap()
    });
    // A distinct fresh TLS connection observes the server independently of the
    // terminated process. Recovery has no driver dispatch/replay capability.
    let (descriptor, login) = owned_pg_fixture();
    let mut verify = NativeDriverFactory::default()
        .connect(&descriptor, login, Mode::ReadOnly)
        .await
        .expect("fresh owned TLS observer failed; redacted");
    let mut sink = LiveSink::default();
    verify
        .execute_bound(
            &format!("SELECT COUNT(*)::bigint FROM \"{table}\" WHERE id=$1::text"),
            &[TypedCell::Text {
                value: row.to_string(),
            }],
            QueryLimits::default(),
            &mut sink,
        )
        .await
        .expect("fresh owned count failed; redacted");
    verify.close().await.unwrap();
    drop(verify);
    let (descriptor, login) = owned_pg_fixture();
    let mut cleanup = NativeDriverFactory::default()
        .connect(&descriptor, login, Mode::Full)
        .await
        .expect("owned cleanup TLS failed; redacted");
    cleanup
        .execute(
            &format!("DROP TABLE \"{table}\""),
            QueryLimits::default(),
            &mut LiveSink::default(),
        )
        .await
        .expect("owned table cleanup failed; redacted");
    cleanup.close().await.unwrap();
    assert_eq!(
        status.code(),
        Some(86),
        "child must exit at the post-COMMIT/pre-finalize boundary"
    );
    let (_, raw_state, dispatches) = raw.unwrap();
    assert_eq!(raw_state, "commit_requested");
    assert_eq!(dispatches, 1);
    let operation = recovered.unwrap();
    assert_eq!(operation.state, OperationState::OutcomeUnknown);
    assert_eq!(
        operation.transaction_outcome,
        TransactionOutcome::OutcomeUnknown
    );
    assert_eq!(sink.rows, vec![vec![TypedCell::I64 { value: "1".into() }]]);
    for name in ["client-state.sqlite3", "client-state.sqlite3-wal"] {
        let path = root.path().join(name);
        if path.exists() {
            let bytes = std::fs::read(path).unwrap();
            for canary in [b"SQL_CANARY_REMOTE_712c".as_slice(), row.as_bytes()] {
                assert!(!bytes.windows(canary.len()).any(|part| part == canary));
            }
        }
    }
    eprintln!("M3 boundary_violations=0; real_pg_verified_tls=true; child_exit=86; positive_remote_commit_then_process_exit=true; local_reopen=outcome_unknown; observed_remote_rows=1; recovery_auto_replays=0; packet_loss_simulated=false; os_standard_integrity_e2e=false");
}
