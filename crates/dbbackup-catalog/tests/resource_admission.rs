use dbbackup_catalog::{
    AdmissionRequest, ClaimRequest, OperationWorkSpec, OperationalStore, ResourcePoolSpec,
    ResourceRequirement, SpoolRequirement, SpoolVolumeSpec,
};
use dbbackup_domain::{OperationRunId, WorkItemId};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn one_insufficient_resource_creates_no_allocation_or_spool_reservation() {
    let temp_dir = TempDir::new().expect("temp directory");
    let db_path = temp_dir.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let global_pool_id = Uuid::new_v4().to_string();
    let source_pool_id = Uuid::new_v4().to_string();
    let spool_volume_id = Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            global_pool_id.clone(),
            "global_job",
            "service",
            2,
            100,
        ))
        .expect("register global pool");
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            source_pool_id.clone(),
            "source",
            "db.internal:5432",
            1,
            100,
        ))
        .expect("register source pool");
    repository
        .register_spool_volume(SpoolVolumeSpec::ready(
            spool_volume_id.clone(),
            "D:",
            "D:/spool",
            10_000,
            8_000,
            1_000,
            9_000,
            100,
        ))
        .expect("register spool volume");

    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            WorkItemId::new(),
            "tool_rescan",
            "service",
            "capacity-blocker",
            "{}",
            1_000,
        ))
        .expect("enqueue blocker");
    let blocker = repository
        .claim_next(ClaimRequest::new("worker-a", "boot-a", 1_000, 1_000))
        .expect("claim blocker")
        .expect("blocker work");
    let blocker_admission = repository
        .admit_claim(AdmissionRequest::new(
            blocker,
            vec![ResourceRequirement::new(source_pool_id.clone(), 1)],
            None,
            1_001,
        ))
        .expect("admit blocker");
    assert!(blocker_admission.admitted());

    let candidate_work_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            candidate_work_id.clone(),
            "tool_rescan",
            "service",
            "candidate",
            "{}",
            1_100,
        ))
        .expect("enqueue candidate");
    let candidate = repository
        .claim_next(ClaimRequest::new("worker-b", "boot-b", 1_100, 1_000))
        .expect("claim candidate")
        .expect("candidate work");
    let candidate_attempt_id = candidate.attempt_id().clone();
    let outcome = repository
        .admit_claim(AdmissionRequest::new(
            candidate,
            vec![
                ResourceRequirement::new(source_pool_id, 1),
                ResourceRequirement::new(global_pool_id, 1),
            ],
            Some(SpoolRequirement::new(
                spool_volume_id,
                2_000,
                "D:/spool/candidate",
            )),
            1_101,
        ))
        .expect("capacity shortage is a waiting outcome");
    assert!(!outcome.admitted());
    assert_eq!(
        outcome.wait_reason(),
        Some("resource_capacity:source:db.internal:5432")
    );

    let direct =
        dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open admission probe");
    let rows: (i64, i64, Option<String>) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM resource_allocations
                  WHERE work_attempt_id = ?1),
                 (SELECT COUNT(*) FROM spool_reservations
                  WHERE work_attempt_id = ?1),
                 (SELECT wait_reason FROM work_items WHERE id = ?2)",
            dbbackup_catalog::rusqlite::params![
                candidate_attempt_id.as_str(),
                candidate_work_id.as_str()
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query atomic admission result");
    assert_eq!(
        rows,
        (
            0,
            0,
            Some("resource_capacity:source:db.internal:5432".to_owned())
        )
    );
}

#[test]
fn successful_admission_is_idempotent_and_reserves_every_requested_unit() {
    let temp_dir = TempDir::new().expect("temp directory");
    let db_path = temp_dir.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let global_pool_id = Uuid::new_v4().to_string();
    let cpu_pool_id = Uuid::new_v4().to_string();
    let spool_volume_id = Uuid::new_v4().to_string();
    for (id, resource_type, resource_key, capacity) in [
        (global_pool_id.clone(), "global_job", "service", 4),
        (cpu_pool_id.clone(), "cpu_encode", "local", 2),
    ] {
        repository
            .register_resource_pool(ResourcePoolSpec::new(
                id,
                resource_type,
                resource_key,
                capacity,
                200,
            ))
            .expect("register resource pool");
    }
    repository
        .register_spool_volume(SpoolVolumeSpec::ready(
            spool_volume_id.clone(),
            "D:",
            "D:/spool",
            20_000,
            16_000,
            1_000,
            18_000,
            200,
        ))
        .expect("register spool volume");
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "admitted",
            "{}",
            2_000,
        ))
        .expect("enqueue admitted work");
    let claim = repository
        .claim_next(ClaimRequest::new("worker-c", "boot-c", 2_000, 1_000))
        .expect("claim admitted work")
        .expect("admitted candidate");
    let attempt_id = claim.attempt_id().clone();
    let make_request = || {
        AdmissionRequest::new(
            claim.clone(),
            vec![
                ResourceRequirement::new(cpu_pool_id.clone(), 1),
                ResourceRequirement::new(global_pool_id.clone(), 1),
            ],
            Some(SpoolRequirement::new(
                spool_volume_id.clone(),
                4_000,
                "D:/spool/admitted",
            )),
            2_001,
        )
    };

    let first = repository
        .admit_claim(make_request())
        .expect("first admission");
    let replay = repository
        .admit_claim(make_request())
        .expect("idempotent admission replay");
    assert!(first.admitted());
    assert!(replay.admitted());
    assert_eq!(first.allocation_count(), 2);
    assert!(first.spool_reserved());
    assert_eq!(first, replay);

    let direct = dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open success probe");
    let rows: (i64, i64, Option<String>) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM resource_allocations
                  WHERE work_attempt_id = ?1),
                 (SELECT COUNT(*) FROM spool_reservations
                  WHERE work_attempt_id = ?1),
                 (SELECT wait_reason FROM work_items WHERE id = ?2)",
            dbbackup_catalog::rusqlite::params![attempt_id.as_str(), work_item_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query successful admission");
    assert_eq!(rows, (2, 1, None));
}

#[test]
fn runtime_capacity_observations_are_idempotent_and_reuse_canonical_rows() {
    let temp_dir = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp_dir.path().join("operational.db"))
        .expect("open operational store");
    let repository = store.runtime();
    let initial_pool_id = Uuid::new_v4().to_string();
    let replacement_pool_id = Uuid::new_v4().to_string();
    let initial_spool_id = Uuid::new_v4().to_string();
    let replacement_spool_id = Uuid::new_v4().to_string();

    let pool_id = repository
        .upsert_resource_pool(ResourcePoolSpec::new(
            initial_pool_id.clone(),
            "global_job",
            "backup-service",
            2,
            100,
        ))
        .expect("insert observed resource pool");
    let replayed_pool_id = repository
        .upsert_resource_pool(ResourcePoolSpec::new(
            replacement_pool_id,
            "global_job",
            "backup-service",
            4,
            200,
        ))
        .expect("refresh observed resource pool");
    assert_eq!(pool_id, initial_pool_id);
    assert_eq!(replayed_pool_id, initial_pool_id);

    let spool_id = repository
        .upsert_spool_volume(SpoolVolumeSpec::ready(
            initial_spool_id.clone(),
            "volume:D",
            "D:/spool",
            10_000,
            9_000,
            1_000,
            8_000,
            100,
        ))
        .expect("insert observed spool volume");
    let replayed_spool_id = repository
        .upsert_spool_volume(SpoolVolumeSpec::ready(
            replacement_spool_id,
            "volume:D",
            "D:/spool",
            20_000,
            18_000,
            2_000,
            16_000,
            200,
        ))
        .expect("refresh observed spool volume");
    assert_eq!(spool_id, initial_spool_id);
    assert_eq!(replayed_spool_id, initial_spool_id);

    let direct =
        dbbackup_catalog::rusqlite::Connection::open(temp_dir.path().join("operational.db"))
            .expect("open capacity observation probe");
    let pool: (i64, i64) = direct
        .query_row(
            "SELECT capacity, updated_at_ms FROM resource_pools WHERE id = ?1",
            [&initial_pool_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read refreshed resource pool");
    assert_eq!(pool, (4, 200));
    let spool: (i64, i64, i64, i64) = direct
        .query_row(
            "SELECT hard_limit_bytes, reserve_floor_bytes, last_free_bytes, updated_at_ms
             FROM spool_volumes WHERE id = ?1",
            [&initial_spool_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read refreshed spool volume");
    assert_eq!(spool, (20_000, 2_000, 16_000, 200));

    repository
        .upsert_spool_volume(SpoolVolumeSpec::ready(
            Uuid::new_v4().to_string(),
            "volume:D",
            "D:/spool",
            20_000,
            18_000,
            2_000,
            1_500,
            300,
        ))
        .expect("observe spool below its reserve floor");
    let state: String = direct
        .query_row(
            "SELECT state FROM spool_volumes WHERE id = ?1",
            [&initial_spool_id],
            |row| row.get(0),
        )
        .expect("read low-capacity spool state");
    assert_eq!(state, "full");
}

#[test]
fn active_spool_reservations_cannot_consume_the_observed_free_space_floor() {
    let temp_dir = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp_dir.path().join("operational.db"))
        .expect("open operational store");
    let repository = store.runtime();
    let spool_volume_id = Uuid::new_v4().to_string();
    repository
        .register_spool_volume(SpoolVolumeSpec::ready(
            spool_volume_id.clone(),
            "capacity-floor",
            "D:/spool",
            1_000,
            900,
            100,
            1_000,
            100,
        ))
        .expect("register spool volume");

    let first_work_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            first_work_id,
            "tool_rescan",
            "service",
            "floor-holder",
            "{}",
            1_000,
        ))
        .expect("enqueue floor holder");
    let first = repository
        .claim_next(ClaimRequest::new(
            "worker-floor-a",
            "boot-floor-a",
            1_000,
            1_000,
        ))
        .expect("claim floor holder")
        .expect("floor holder work");
    assert!(repository
        .admit_claim(AdmissionRequest::new(
            first,
            vec![],
            Some(SpoolRequirement::new(
                spool_volume_id.clone(),
                600,
                "D:/spool/first",
            )),
            1_001,
        ))
        .expect("admit floor holder")
        .admitted());

    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            WorkItemId::new(),
            "tool_rescan",
            "service",
            "floor-candidate",
            "{}",
            1_100,
        ))
        .expect("enqueue floor candidate");
    let candidate = repository
        .claim_next(ClaimRequest::new(
            "worker-floor-b",
            "boot-floor-b",
            1_100,
            1_000,
        ))
        .expect("claim floor candidate")
        .expect("floor candidate work");
    let outcome = repository
        .admit_claim(AdmissionRequest::new(
            candidate,
            vec![],
            Some(SpoolRequirement::new(
                spool_volume_id,
                400,
                "D:/spool/second",
            )),
            1_101,
        ))
        .expect("floor shortage is a waiting outcome");
    assert!(!outcome.admitted());
    assert_eq!(outcome.wait_reason(), Some("spool_capacity:capacity-floor"));
}

#[test]
fn heartbeat_renews_resource_allocation_lease_and_blocks_competing_claim() {
    let temp_dir = TempDir::new().expect("temp directory");
    let db_path = temp_dir.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let resource_pool_id = Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            resource_pool_id.clone(),
            "global_job",
            "service",
            1,
            100,
        ))
        .expect("register resource pool");

    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            WorkItemId::new(),
            "tool_rescan",
            "service",
            "lease-holder",
            "{}",
            1_000,
        ))
        .expect("enqueue lease holder");
    let lease_holder = repository
        .claim_next(ClaimRequest::new("worker-a", "boot-a", 1_000, 100))
        .expect("claim lease holder")
        .expect("lease holder work");
    assert!(repository
        .admit_claim(AdmissionRequest::new(
            lease_holder.clone(),
            vec![ResourceRequirement::new(resource_pool_id.clone(), 1)],
            None,
            1_001,
        ))
        .expect("admit lease holder")
        .admitted());
    repository
        .heartbeat_claim(&lease_holder, 1_050, 2_000)
        .expect("renew lease holder");

    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            WorkItemId::new(),
            "tool_rescan",
            "service",
            "competitor",
            "{}",
            1_200,
        ))
        .expect("enqueue competitor");
    let competitor = repository
        .claim_next(ClaimRequest::new("worker-b", "boot-b", 1_200, 1_000))
        .expect("claim competitor")
        .expect("competitor work");
    let competitor_outcome = repository
        .admit_claim(AdmissionRequest::new(
            competitor,
            vec![ResourceRequirement::new(resource_pool_id, 1)],
            None,
            1_201,
        ))
        .expect("capacity contention is a waiting outcome");

    let direct = dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open lease probe");
    let allocation_expiry: i64 = direct
        .query_row(
            "SELECT expires_at_ms
             FROM resource_allocations
             WHERE work_attempt_id = ?1
               AND released_at_ms IS NULL",
            [lease_holder.attempt_id().as_str()],
            |row| row.get(0),
        )
        .expect("query active allocation lease");
    assert_eq!(
        (
            allocation_expiry,
            competitor_outcome.admitted(),
            competitor_outcome.wait_reason(),
        ),
        (2_000, false, Some("resource_capacity:global_job:service"))
    );
}

#[test]
fn retry_release_is_atomic_and_immediately_reuses_resource_and_spool_capacity() {
    let temp_dir = TempDir::new().expect("temp directory");
    let db_path = temp_dir.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let resource_pool_id = Uuid::new_v4().to_string();
    let spool_volume_id = Uuid::new_v4().to_string();
    repository
        .register_resource_pool(ResourcePoolSpec::new(
            resource_pool_id.clone(),
            "global_job",
            "service",
            1,
            100,
        ))
        .expect("register resource pool");
    repository
        .register_spool_volume(SpoolVolumeSpec::ready(
            spool_volume_id.clone(),
            "D:",
            "D:/spool",
            1_000,
            900,
            100,
            1_000,
            100,
        ))
        .expect("register spool volume");

    let first_work_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            first_work_id,
            "tool_rescan",
            "service",
            "first",
            "{}",
            1_000,
        ))
        .expect("enqueue first work");
    let first_claim = repository
        .claim_next(ClaimRequest::new("worker-a", "boot-a", 1_000, 10_000))
        .expect("claim first work")
        .expect("first work");
    let first_attempt_id = first_claim.attempt_id().clone();
    assert!(repository
        .admit_claim(AdmissionRequest::new(
            first_claim.clone(),
            vec![ResourceRequirement::new(resource_pool_id.clone(), 1)],
            Some(SpoolRequirement::new(
                spool_volume_id.clone(),
                900,
                "D:/spool/first",
            )),
            1_001,
        ))
        .expect("admit first")
        .admitted());
    assert_eq!(
        repository.active_spool_paths(10).expect("active spools"),
        vec!["D:/spool/first".to_owned()]
    );

    repository
        .requeue_claim(&first_claim, 1_002, 2_000)
        .expect("atomically release and requeue first claim");

    let second_work_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            second_work_id,
            "tool_rescan",
            "service",
            "second",
            "{}",
            1_003,
        ))
        .expect("enqueue second work");
    let second_claim = repository
        .claim_next(ClaimRequest::new("worker-b", "boot-b", 1_003, 10_000))
        .expect("claim second work")
        .expect("second work");
    let second_attempt_id = second_claim.attempt_id().clone();
    assert!(repository
        .admit_claim(AdmissionRequest::new(
            second_claim,
            vec![ResourceRequirement::new(resource_pool_id, 1)],
            Some(SpoolRequirement::new(
                spool_volume_id,
                900,
                "D:/spool/second",
            )),
            1_004,
        ))
        .expect("admit second immediately after release")
        .admitted());
    assert_eq!(
        repository
            .active_spool_paths(10)
            .expect("released spools are not active"),
        vec!["D:/spool/second".to_owned()]
    );

    let stale = repository
        .requeue_claim(&first_claim, 1_005, 2_001)
        .expect_err("stale retry must not release another attempt");
    assert_eq!(stale.code(), "WORK_ITEM_FENCE_STALE");

    let direct = dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open release probe");
    let first_rows: (Option<i64>, Option<String>, Option<i64>, String) = direct
        .query_row(
            "SELECT
                 resource_allocations.released_at_ms,
                 resource_allocations.release_reason,
                 spool_reservations.released_at_ms,
                 spool_reservations.state
             FROM resource_allocations
             JOIN spool_reservations
               ON spool_reservations.work_attempt_id = resource_allocations.work_attempt_id
             WHERE resource_allocations.work_attempt_id = ?1",
            [first_attempt_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("query first release");
    assert_eq!(
        first_rows,
        (
            Some(1_002),
            Some("failed".to_owned()),
            Some(1_002),
            "released".to_owned()
        )
    );
    let active_second: (i64, i64) = direct
        .query_row(
            "SELECT
                 (SELECT COUNT(*) FROM resource_allocations
                  WHERE work_attempt_id = ?1 AND released_at_ms IS NULL),
                 (SELECT COUNT(*) FROM spool_reservations
                  WHERE work_attempt_id = ?1 AND released_at_ms IS NULL)",
            [second_attempt_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query second active reservations");
    assert_eq!(active_second, (1, 1));
}

#[test]
fn repeated_admission_deferrals_do_not_exhaust_the_execution_attempt_budget() {
    let temp_dir = TempDir::new().expect("temp directory");
    let db_path = temp_dir.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open operational store");
    let repository = store.runtime();
    let resource_pool_id = Uuid::new_v4().to_string();
    let work_item_id = WorkItemId::new();
    repository
        .enqueue_operation_work(OperationWorkSpec::new(
            OperationRunId::new(),
            work_item_id.clone(),
            "tool_rescan",
            "service",
            "deferred",
            "{}",
            1_000,
        ))
        .expect("enqueue deferred work");

    for attempt in 1_i64..=5 {
        let now_ms = 1_000 + attempt * 10;
        let claim = repository
            .claim_next(ClaimRequest::new(
                format!("worker-{attempt}"),
                format!("boot-{attempt}"),
                now_ms,
                100,
            ))
            .expect("claim deferred work")
            .expect("admission deferral must not exhaust execution attempts");
        let outcome = repository
            .admit_claim(AdmissionRequest::new(
                claim.clone(),
                vec![ResourceRequirement::new(resource_pool_id.clone(), 1)],
                None,
                now_ms + 1,
            ))
            .expect("missing pool is a waiting outcome");
        assert!(!outcome.admitted());
        repository
            .defer_claim(
                &claim,
                now_ms + 2,
                now_ms + 3,
                outcome.wait_reason().expect("wait reason"),
            )
            .expect("durably defer without consuming execution budget");
    }

    repository
        .register_resource_pool(ResourcePoolSpec::new(
            resource_pool_id.clone(),
            "global_job",
            "service",
            1,
            1_100,
        ))
        .expect("register resource after contention clears");
    let final_claim = repository
        .claim_next(ClaimRequest::new("worker-final", "boot-final", 1_100, 100))
        .expect("claim after deferrals")
        .expect("execution budget remains");
    assert_eq!(final_claim.attempt_no(), 6);
    assert!(repository
        .admit_claim(AdmissionRequest::new(
            final_claim,
            vec![ResourceRequirement::new(resource_pool_id, 1)],
            None,
            1_101,
        ))
        .expect("admit after pool registration")
        .admitted());

    let direct =
        dbbackup_catalog::rusqlite::Connection::open(db_path).expect("open deferral probe");
    let attempts: (i64, i64, i64) = direct
        .query_row(
            "SELECT
                 COUNT(*),
                 SUM(CASE WHEN state = 'abandoned' THEN 1 ELSE 0 END),
                 SUM(CASE WHEN state = 'claimed' THEN 1 ELSE 0 END)
             FROM work_attempts
             WHERE work_item_id = ?1",
            [work_item_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query deferral history");
    assert_eq!(attempts, (6, 5, 1));
}
