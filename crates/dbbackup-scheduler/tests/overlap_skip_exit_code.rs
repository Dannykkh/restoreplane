// T6~T8 — overlap 스킵 경로의 exit code 정책 + 알림 라우팅. S08 try_acquire_run 재사용을 검증한다.
use dbbackup_catalog::{try_acquire_run, CatalogWriterHandle};
use dbbackup_scheduler::entrypoint::{run_scheduled_job, JobResult, JobRunner, SchedulerContext};
use dbbackup_scheduler::notify::{CompositeSink, NotificationSink, RunEvent, RunOutcome};
use dbbackup_scheduler::overlap::{EXIT_SKIPPED_OVERLAP, EXIT_SUCCESS};
use dbbackup_scheduler::ScheduleSpec;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const DAILY: ScheduleSpec = ScheduleSpec::Daily { hour: 3, minute: 0 };

struct MockRunner {
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl JobRunner for MockRunner {
    async fn run(&self, _job: &str) -> anyhow::Result<JobResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(JobResult::Success {
            backup_id: "bk-1".to_string(),
        })
    }
}

/// run 종료 outcome을 기록하는 sink(알림 라우팅 검증용).
struct RecordingSink(Arc<Mutex<Vec<RunOutcome>>>);
#[async_trait::async_trait]
impl NotificationSink for RecordingSink {
    async fn notify(&self, e: &RunEvent) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(e.outcome);
        Ok(())
    }
    fn name(&self) -> &'static str {
        "recording"
    }
}

fn ctx(db: std::path::PathBuf, writer: CatalogWriterHandle, rec: Arc<Mutex<Vec<RunOutcome>>>) -> SchedulerContext {
    SchedulerContext {
        catalog_writer: writer,
        catalog_db_path: db,
        notify: CompositeSink::from_sinks(vec![Box::new(RecordingSink(rec))]),
    }
}

#[test]
fn exit_code_constants_are_aligned() {
    // 스킵은 의도적으로 성공과 동일 exit code(RestartOnFailure 폭주 방지, FR6).
    assert_eq!(EXIT_SKIPPED_OVERLAP, EXIT_SUCCESS);
}

#[tokio::test]
async fn t6_normal_run_invokes_pipeline_once_and_exits_success() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog.db");
    let writer = CatalogWriterHandle::spawn(db.clone()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let rec = Arc::new(Mutex::new(Vec::new()));
    let c = ctx(db, writer, rec.clone());

    let code = run_scheduled_job("job-a", &DAILY, &c, &MockRunner { calls: calls.clone() }).await;
    assert_eq!(code, EXIT_SUCCESS);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "파이프라인은 정확히 1회 호출");
    assert_eq!(rec.lock().unwrap().as_slice(), &[RunOutcome::Success]);
}

#[tokio::test]
async fn t7_t8_overlap_skips_without_running_pipeline_and_exits_zero() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog.db");
    let writer = CatalogWriterHandle::spawn(db.clone()).unwrap();

    // 이미 실행 중인 인스턴스 시뮬레이션: RunGuard를 마감하지 않고 살려둔다(running row 유지).
    let _held = try_acquire_run(&writer, "job-b").expect("첫 acquire 성공");

    let calls = Arc::new(AtomicUsize::new(0));
    let rec = Arc::new(Mutex::new(Vec::new()));
    let c = ctx(db, writer, rec.clone());

    let code = run_scheduled_job("job-b", &DAILY, &c, &MockRunner { calls: calls.clone() }).await;

    // 스킵은 exit 0 (RestartOnFailure 유발 금지).
    assert_eq!(code, EXIT_SKIPPED_OVERLAP);
    // 파이프라인은 절대 실행되지 않음.
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // 스킵도 알림 채널에 SkippedOverlap outcome으로 가시화됨(실패로 오인되지 않음).
    assert_eq!(rec.lock().unwrap().as_slice(), &[RunOutcome::SkippedOverlap]);
}
