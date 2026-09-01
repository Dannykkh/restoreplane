// dbbackup-scheduler — S10: Task Scheduler(schtasks) 등록 + Overlap Guard 어댑터 + 알림 + 리댁션.
//
// 설계 원칙:
// - overlap guard 로직은 신규 구현하지 않는다. S08(dbbackup-catalog)의 try_acquire_run/RunGuard를
//   재사용하고, 이 크레이트는 exit code 정책 + 스킵 가시화만 얹는다(overlap.rs).
// - windows-service를 사용하지 않는다. catch-up은 Task Scheduler의 StartWhenAvailable=true에 위임.
// - 모든 로그/알림은 기록 직전 시크릿 리댁션을 거친다(redact.rs 2중 방어).

pub mod cron;
pub mod entrypoint;
pub mod notify;
pub mod occurrence;
pub mod overlap;
pub mod redact;
pub mod register;
pub mod rpo;
pub mod service_wake;
pub mod xml;

pub use cron::{parse_cron_subset, CronError, ScheduleSpec, Weekday};
pub use entrypoint::{run_scheduled_job, JobResult, JobRunner, SchedulerContext};
pub use notify::{
    CompositeSink, EmailPlaceholderSink, LogFileSink, NotificationSink, RunEvent, RunOutcome,
    WebhookSink, WindowsEventLogSink,
};
pub use overlap::{
    acquire_or_skip, AcquireOutcome, EXIT_FAILED, EXIT_SCHEDULER_ERROR, EXIT_SKIPPED_OVERLAP,
    EXIT_SUCCESS,
};
pub use redact::{scrub, RedactingWriter};
pub use register::{
    query_task_xml, register_idempotent, register_task, remove_task, task_name, RegisterOutcome,
    SchedulerError,
};
pub use rpo::{build_notice, compute_rpo_notice, format_notice, print_and_log, RpoNotice};
pub use xml::{escape_xml, render_task_xml, LogonType, TaskXmlParams};

/// tracing_subscriber 초기화(FR8 방어선 2: RedactingWriter로 파일 writer를 감싼다).
pub fn init_scheduler_tracing(log_path: &std::path::Path) -> anyhow::Result<()> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let redacting = crate::redact::RedactingWriter::new(file);
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(redacting))
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .map_err(|e| anyhow::anyhow!("tracing 초기화 실패: {e}"))?;
    Ok(())
}
