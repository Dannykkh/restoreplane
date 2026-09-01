// src/entrypoint.rs — 스케줄러 진입점(FR6/FR7/FR9). overlap guard → 실행 → RunGuard 마감 → RPO → 알림.
//
// 편차(문서화): 원본 S10은 restoreplane_pipeline::run_job/JobResult를 직접 호출하도록 명세하지만,
// 그 API는 아직 S05에 존재하지 않는다. 재구현/재발명을 피하기 위해 JobRunner 트레이트로 실행 계층을
// 추상화하고, S05가 준비되면 restoreplane_pipeline::run_job을 감싸는 어댑터를 주입하도록 계약만 고정한다.
use crate::cron::ScheduleSpec;
use crate::notify::{CompositeSink, RunEvent, RunOutcome};
use crate::overlap::{
    acquire_or_skip, AcquireOutcome, EXIT_FAILED, EXIT_SCHEDULER_ERROR, EXIT_SKIPPED_OVERLAP,
    EXIT_SUCCESS,
};
use crate::redact::scrub;
use crate::rpo::{compute_rpo_notice, print_and_log};
use async_trait::async_trait;
use restoreplane_catalog::writer::CatalogWriterHandle;
use std::path::PathBuf;
use time::OffsetDateTime;

/// 파이프라인 실행 결과. S05가 이 shape의 결과를 돌려주는 러너를 제공한다.
#[derive(Debug, Clone)]
pub enum JobResult {
    Success {
        backup_id: String,
    },
    PartialSuccess {
        backup_id: String,
        failed_destinations: Vec<String>,
    },
}

impl JobResult {
    pub fn backup_id(&self) -> &str {
        match self {
            JobResult::Success { backup_id } => backup_id,
            JobResult::PartialSuccess { backup_id, .. } => backup_id,
        }
    }
}

/// 실제 덤프/암호화/업로드를 수행하는 실행 계층 추상화(S05 소관). 이 섹션은 호출만 한다.
#[async_trait]
pub trait JobRunner: Send + Sync {
    async fn run(&self, job: &str) -> anyhow::Result<JobResult>;
}

pub struct SchedulerContext {
    pub catalog_writer: CatalogWriterHandle,
    pub catalog_db_path: PathBuf,
    pub notify: CompositeSink,
}

/// restoreplane-cli의 `run` 커맨드 핸들러가 호출하는 유일한 진입점. 반환값은 프로세스 exit code.
pub async fn run_scheduled_job(
    job: &str,
    schedule: &ScheduleSpec,
    ctx: &SchedulerContext,
    runner: &dyn JobRunner,
) -> i32 {
    let acquired = match acquire_or_skip(&ctx.catalog_writer, job) {
        Ok(o) => o,
        Err(e) => {
            tracing::error!(job, error = %scrub(&e.to_string()), "overlap guard 획득 자체가 실패함");
            return EXIT_SCHEDULER_ERROR;
        }
    };

    let guard = match acquired {
        AcquireOutcome::Acquired(g) => g,
        AcquireOutcome::Skipped => {
            let event = RunEvent {
                job: job.to_string(),
                backup_id: None,
                outcome: RunOutcome::SkippedOverlap,
                started_at: OffsetDateTime::now_utc(),
                finished_at: None,
                message: "이미 실행 중인 인스턴스가 있어 이번 트리거를 스킵함(overlap guard)"
                    .to_string(),
                rpo_summary: None,
            };
            ctx.notify.notify_all(&event).await;
            return EXIT_SKIPPED_OVERLAP; // 의도적으로 0 — RestartOnFailure 폭주 방지(FR6)
        }
    };

    let started_at = OffsetDateTime::now_utc();
    let result = runner.run(job).await;
    let finished_at = OffsetDateTime::now_utc();

    let (status, outcome, msg, backup_id) = match &result {
        Ok(JobResult::Success { backup_id }) => (
            "success",
            RunOutcome::Success,
            format!("백업 성공(backup_id={backup_id})"),
            Some(backup_id.clone()),
        ),
        Ok(JobResult::PartialSuccess {
            backup_id,
            failed_destinations,
        }) => (
            "partial",
            RunOutcome::PartialSuccess,
            format!("일부 목적지 실패(backup_id={backup_id}, 실패={failed_destinations:?})"),
            Some(backup_id.clone()),
        ),
        Err(e) => (
            "failed",
            RunOutcome::Failed,
            scrub(&format!("백업 실패: {e}")),
            None,
        ),
    };

    if let Err(e) = guard.finish(&ctx.catalog_writer, status) {
        tracing::error!(job, error = %scrub(&e.to_string()), "RunGuard.finish() 실패(runs row가 running으로 남을 수 있음, 다음 실행 시 stale 회수됨)");
    }

    let rpo = compute_rpo_notice(&ctx.catalog_db_path, job, schedule).ok();
    if let Some(n) = &rpo {
        print_and_log(n);
    }

    let event = RunEvent {
        job: job.to_string(),
        backup_id,
        outcome,
        started_at,
        finished_at: Some(finished_at),
        message: msg,
        rpo_summary: rpo.map(|n| format!("last_success_elapsed_ok={}", n.within_target)),
    };
    ctx.notify.notify_all(&event).await;

    match outcome {
        RunOutcome::Success | RunOutcome::PartialSuccess => EXIT_SUCCESS,
        RunOutcome::Failed => EXIT_FAILED,
        RunOutcome::SkippedOverlap => unreachable!("이 분기는 위에서 이미 처리됨"),
    }
}
