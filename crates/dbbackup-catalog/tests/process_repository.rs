use dbbackup_catalog::{
    AdmissionRequest, ClaimCompletionSpec, ClaimRequest, OperationWorkSpec, OperationalStore,
    ProcessExitSpec, ProcessStartSpec, ResourcePoolSpec, ResourceRequirement,
};
use dbbackup_domain::{OperationRunId, WorkItemId};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn process_identity_and_exit_are_persisted_under_the_current_attempt_fence() {
    let fixture = Fixture::new();
    let repository = fixture.store.runtime();
    let claim = fixture.enqueue_and_claim("normal", 1_000, "worker-a", "boot-a");
    let process_id = Uuid::new_v4().to_string();
    let executable = std::env::current_exe()
        .expect("current executable")
        .to_string_lossy()
        .into_owned();

    let malformed = repository
        .complete_claim(ClaimCompletionSpec::succeeded(
            claim.clone(),
            1_001,
            Some("{".to_owned()),
        ))
        .expect_err("malformed terminal result must fail before mutation");
    assert_eq!(malformed.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
    let resource_pool_id = Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            resource_pool_id.clone(),
            "global_job",
            "service",
            1,
            900,
        ))
        .expect("register lifecycle resource");
    assert!(repository
        .admit_claim(AdmissionRequest::new(
            claim.clone(),
            vec![ResourceRequirement::new(resource_pool_id, 1)],
            None,
            1_002,
        ))
        .expect("admit lifecycle claim")
        .admitted());
    repository
        .start_claim(&claim, 1_005)
        .expect("persist starting claim before launch");
    repository
        .record_process_start(ProcessStartSpec::new(
            process_id.clone(),
            claim.clone(),
            "native_dump",
            4_242,
            900,
            "boot-a",
            "job-object/normal",
            executable,
            Some("a".repeat(64)),
            1_010,
        ))
        .expect("record process start");
    repository
        .mark_process_running(&claim, &process_id, 1_011)
        .expect("mark process running");
    let active_process = repository
        .complete_claim(ClaimCompletionSpec::failed(
            claim.clone(),
            1_012,
            "PROCESS_FAILED",
            None,
        ))
        .expect_err("an active process prevents terminal settlement");
    assert_eq!(active_process.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
    repository
        .record_process_exit(
            &claim,
            ProcessExitSpec::new(process_id.clone(), 1_100, 0, "exited"),
        )
        .expect("record process exit");
    repository
        .begin_claim_commit(&claim, 1_101)
        .expect("begin durable commit");
    repository
        .complete_claim(ClaimCompletionSpec::succeeded(
            claim.clone(),
            1_102,
            Some(r#"{"inventory":"refreshed"}"#.to_owned()),
        ))
        .expect("complete operation exactly once");
    let replay = repository
        .complete_claim(ClaimCompletionSpec::succeeded(claim, 1_103, None))
        .expect_err("terminal claim replay is stale");
    assert_eq!(replay.code(), "WORK_ITEM_FENCE_STALE");

    let row: (String, i64, i64, String, String, Option<i64>) = fixture.query_process(&process_id);
    assert_eq!(
        row,
        (
            "exited".to_owned(),
            4_242,
            900,
            "boot-a".to_owned(),
            "job-object/normal".to_owned(),
            Some(0)
        )
    );
    assert_eq!(
        fixture.query_lifecycle(),
        (
            "succeeded".to_owned(),
            None,
            "succeeded".to_owned(),
            "succeeded".to_owned(),
            Some(r#"{"inventory":"refreshed"}"#.to_owned()),
            Some("completed".to_owned())
        )
    );
}

#[test]
fn stale_attempt_cannot_mutate_a_recorded_process_after_reclaim() {
    let fixture = Fixture::new();
    let repository = fixture.store.runtime();
    let first = fixture.enqueue_and_claim("stale", 2_000, "worker-a", "boot-a");
    let process_id = Uuid::new_v4().to_string();
    repository
        .start_claim(&first, 2_005)
        .expect("persist first starting claim");
    repository
        .record_process_start(ProcessStartSpec::new(
            process_id.clone(),
            first.clone(),
            "native_dump",
            7_777,
            1_900,
            "boot-a",
            "job-object/stale",
            std::env::current_exe()
                .expect("current executable")
                .to_string_lossy()
                .into_owned(),
            None,
            2_010,
        ))
        .expect("record first process");
    repository
        .mark_process_running(&first, &process_id, 2_011)
        .expect("mark first process running");
    repository
        .record_process_exit(
            &first,
            ProcessExitSpec::new(process_id.clone(), 2_019, 23, "exited"),
        )
        .expect("record retryable process exit");
    repository
        .requeue_claim(&first, 2_020, 2_021)
        .expect("release first claim");
    let second = repository
        .claim_next(ClaimRequest::new("worker-b", "boot-b", 2_021, 1_000))
        .expect("claim second attempt")
        .expect("requeued work");
    assert_eq!(second.attempt_no(), 2);

    let error = repository
        .record_process_exit(
            &first,
            ProcessExitSpec::new(process_id.clone(), 2_030, 0, "exited"),
        )
        .expect_err("stale fence must not mutate process lifecycle");
    assert_eq!(error.code(), "WORK_ITEM_FENCE_STALE");
    assert_eq!(fixture.query_process(&process_id).0, "exited");
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
        }
    }

    fn enqueue_and_claim(
        &self,
        subject_id: &str,
        now_ms: i64,
        worker_id: &str,
        boot_id: &str,
    ) -> dbbackup_catalog::ClaimedWork {
        self.store
            .runtime()
            .enqueue_operation_work(OperationWorkSpec::new(
                OperationRunId::new(),
                WorkItemId::new(),
                "tool_rescan",
                "service",
                subject_id,
                "{}",
                now_ms,
            ))
            .expect("enqueue process work");
        self.store
            .runtime()
            .claim_next(ClaimRequest::new(worker_id, boot_id, now_ms, 1_000))
            .expect("claim process work")
            .expect("due process work")
    }

    fn query_process(
        &self,
        process_instance_id: &str,
    ) -> (String, i64, i64, String, String, Option<i64>) {
        dbbackup_catalog::rusqlite::Connection::open(&self.db_path)
            .expect("open process probe")
            .query_row(
                "SELECT
                     state, process_id, process_start_time_ms,
                     service_boot_id, job_object_key, exit_code
                 FROM process_instances
                 WHERE id = ?1",
                [process_instance_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("query process instance")
    }

    fn query_lifecycle(
        &self,
    ) -> (
        String,
        Option<String>,
        String,
        String,
        Option<String>,
        Option<String>,
    ) {
        dbbackup_catalog::rusqlite::Connection::open(&self.db_path)
            .expect("open lifecycle probe")
            .query_row(
                "SELECT
                     work_items.state,
                     work_items.lease_owner,
                     work_attempts.state,
                     operation_runs.state,
                     operation_runs.result_json,
                     (
                         SELECT release_reason
                         FROM resource_allocations
                         WHERE work_attempt_id = work_attempts.id
                         LIMIT 1
                     )
                 FROM work_items
                 JOIN work_attempts
                   ON work_attempts.work_item_id = work_items.id
                 JOIN operation_runs
                   ON operation_runs.id = work_items.operation_run_id
                 ORDER BY work_attempts.attempt_no DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("query lifecycle")
    }
}
