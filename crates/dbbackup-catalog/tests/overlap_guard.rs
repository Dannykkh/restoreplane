// S08 시나리오 7~8 — overlap guard(동시 기동 차단) + stale run 회수.

use dbbackup_catalog::{open_reader_conn, try_acquire_run, CatalogWriterHandle, OverlapError};

#[test]
fn second_concurrent_run_for_same_job_is_blocked() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog.db");
    let writer = CatalogWriterHandle::spawn(db).unwrap();

    // 첫 실행권 획득(아직 finish 안 함 → running row 유지).
    let guard1 = try_acquire_run(&writer, "ov-concurrent").expect("첫 acquire는 성공");

    // 같은 job의 두 번째 acquire는 부분 UNIQUE 인덱스가 막는다(1차 뮤텍스가 재진입돼도 2차가 방어).
    let res2 = try_acquire_run(&writer, "ov-concurrent");
    assert!(
        matches!(res2, Err(OverlapError::AlreadyRunning(_))),
        "두 번째 동시 실행은 차단돼야 함: {res2:?}"
    );

    // 첫 실행을 마감하면 다시 획득 가능.
    guard1.finish(&writer, "success").unwrap();
    let guard3 = try_acquire_run(&writer, "ov-concurrent").expect("마감 후 재획득 성공");
    guard3.finish(&writer, "success").unwrap();
}

#[test]
fn stale_running_row_is_reaped_and_new_run_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog.db");
    let writer = CatalogWriterHandle::spawn(db.clone()).unwrap();

    // 크래시 시뮬레이션: MAX_RUN_SECONDS를 훨씬 넘긴 running row를 직접 삽입.
    writer
        .run(|conn| {
            conn.execute(
                "INSERT INTO runs (id, job, started_at, status)
                 VALUES ('stale-1', 'ov-stale', datetime('now','-10 hours'), 'running')",
                [],
            )?;
            Ok(())
        })
        .unwrap();

    // 새 acquire → 정체 row를 coalesced로 강등하고 새 실행이 성공해야 함.
    let guard = try_acquire_run(&writer, "ov-stale").expect("stale 회수 후 acquire 성공");

    let conn = open_reader_conn(&db).unwrap();
    let coalesced: i64 = conn
        .query_row(
            "SELECT count(*) FROM runs WHERE id='stale-1' AND status='coalesced'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(coalesced, 1, "정체 row는 coalesced로 회수돼야 함");

    let running: i64 = conn
        .query_row(
            "SELECT count(*) FROM runs WHERE job='ov-stale' AND status='running'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(running, 1, "새 실행만 running이어야 함");

    guard.finish(&writer, "success").unwrap();
}
