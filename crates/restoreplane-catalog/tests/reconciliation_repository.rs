use restoreplane_catalog::{
    AdmissionRequest, ClaimRequest, OperationWorkSpec, OperationalStore, ProcessStartSpec,
    ResourcePoolSpec, ResourceRequirement,
};
use restoreplane_domain::{OperationRunId, WorkItemId};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn expired_starting_claim_without_a_process_is_fenced_released_and_requeued() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let pool_id = Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            pool_id.clone(),
            "global_job",
            "service",
            1,
            900,
        ))
        .expect("register resource pool");
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "reconcile-missing",
            "{}",
            1_000,
        ))
        .expect("enqueue operation");
    let stale = repository
        .claim_next(ClaimRequest::new("worker-old", "boot-old", 1_000, 20))
        .expect("claim old work")
        .expect("due old work");
    assert!(repository
        .admit_claim(AdmissionRequest::new(
            stale.clone(),
            vec![ResourceRequirement::new(pool_id, 1)],
            None,
            1_001,
        ))
        .expect("admit old work")
        .admitted());
    repository
        .start_claim(&stale, 1_002)
        .expect("persist starting before simulated service crash");

    let candidates = repository
        .expired_claims(1_021, 100)
        .expect("scan expired claims");
    assert_eq!(candidates.len(), 1);
    let candidate = &candidates[0];
    assert_eq!(candidate.claim(), &stale);
    assert!(candidate.process().is_none());
    assert_eq!(candidate.work_state(), "starting");
    assert_eq!(candidate.attempt_state(), "starting");

    repository
        .reconcile_expired_without_process(candidate, 1_021, 1_022)
        .expect("fence and requeue missing process");
    let stale_write = repository
        .heartbeat_claim(&stale, 1_021, 1_041)
        .expect_err("old worker is fenced immediately");
    assert_eq!(stale_write.code(), "WORK_ITEM_FENCE_STALE");

    let current = repository
        .claim_next(ClaimRequest::new("worker-new", "boot-new", 1_022, 100))
        .expect("claim reconciled work")
        .expect("reconciled work is due");
    assert_eq!(current.attempt_no(), 2);
    assert_eq!(current.fencing_token(), 3);

    let direct =
        restoreplane_catalog::rusqlite::Connection::open(db_path).expect("open reconcile probe");
    let first: (String, Option<String>, Option<String>, Option<i64>) = direct
        .query_row(
            "SELECT
                 work_attempts.state,
                 work_attempts.retry_class,
                 work_attempts.error_code,
                 resource_allocations.released_at_ms
             FROM work_attempts
             JOIN resource_allocations
               ON resource_allocations.work_attempt_id = work_attempts.id
             WHERE work_attempts.id = ?1",
            [stale.attempt_id().as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query reconciled first attempt");
    assert_eq!(
        first,
        (
            "failed".to_owned(),
            Some("reconcile".to_owned()),
            Some("RECONCILE_PROCESS_MISSING".to_owned()),
            Some(1_021)
        )
    );
}

#[test]
fn expired_running_claim_with_a_confirmed_gone_process_is_killed_fenced_and_requeued() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let pool_id = Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            pool_id.clone(),
            "global_job",
            "service",
            1,
            1_900,
        ))
        .expect("register resource pool");
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id,
            "tool_rescan",
            "service",
            "reconcile-recorded-process",
            "{}",
            2_000,
        ))
        .expect("enqueue operation");
    let stale = repository
        .claim_next(ClaimRequest::new("worker-old", "boot-old", 2_000, 20))
        .expect("claim old work")
        .expect("due old work");
    assert!(repository
        .admit_claim(AdmissionRequest::new(
            stale.clone(),
            vec![ResourceRequirement::new(pool_id, 1)],
            None,
            2_001,
        ))
        .expect("admit old work")
        .admitted());
    repository.start_claim(&stale, 2_002).expect("start claim");
    let process_instance_id = Uuid::new_v4().to_string();
    let executable = std::env::current_exe()
        .expect("current executable")
        .to_string_lossy()
        .into_owned();
    repository
        .record_process_start(ProcessStartSpec::new(
            process_instance_id.clone(),
            stale.clone(),
            "native_tool",
            42_424,
            1_999,
            "boot-old",
            format!(r"Local\RestorePlane.Job.{}", Uuid::new_v4()),
            executable,
            None,
            2_003,
        ))
        .expect("record process identity");
    repository
        .mark_process_running(&stale, &process_instance_id, 2_004)
        .expect("mark process running");

    let candidate = repository
        .expired_claims(2_021, 1)
        .expect("scan expired claim")
        .pop()
        .expect("recorded process candidate");
    assert!(candidate.process().is_some());
    repository
        .reconcile_expired_process_gone(&candidate, 2_021, 2_022)
        .expect("atomically settle gone process and requeue");

    let stale_write = repository
        .heartbeat_claim(&stale, 2_021, 2_041)
        .expect_err("old process owner is fenced");
    assert_eq!(stale_write.code(), "WORK_ITEM_FENCE_STALE");
    let next = repository
        .claim_next(ClaimRequest::new("worker-new", "boot-new", 2_022, 100))
        .expect("claim retried work")
        .expect("retried work is due");
    assert_eq!(next.attempt_no(), 2);
    assert_eq!(next.fencing_token(), 3);

    let direct =
        restoreplane_catalog::rusqlite::Connection::open(db_path).expect("open reconcile probe");
    let settled: (
        String,
        Option<i64>,
        Option<String>,
        String,
        Option<i64>,
        Option<String>,
    ) = direct
        .query_row(
            "SELECT
                 process_instances.state,
                 process_instances.ended_at_ms,
                 process_instances.termination_reason,
                 work_attempts.state,
                 resource_allocations.released_at_ms,
                 work_attempts.error_code
             FROM process_instances
             JOIN work_attempts
               ON work_attempts.id = process_instances.work_attempt_id
             JOIN resource_allocations
               ON resource_allocations.work_attempt_id = work_attempts.id
             WHERE process_instances.id = ?1",
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
        .expect("query reconciled process");
    assert_eq!(
        settled,
        (
            "killed".to_owned(),
            Some(2_021),
            Some("reconciled_process_gone".to_owned()),
            "failed".to_owned(),
            Some(2_021),
            Some("RECONCILE_PROCESS_MISSING".to_owned()),
        )
    );
}
