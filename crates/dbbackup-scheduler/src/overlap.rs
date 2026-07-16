// src/overlap.rs — Overlap Guard 어댑터(FR6). S08의 try_acquire_run/RunGuard를 그대로 재사용하고
// exit code 정책 + 스킵 가시화만 얹는다. 새 overlap 로직을 구현하지 않는다(중복 구현 금지).
use dbbackup_catalog::overlap::{try_acquire_run, OverlapError, RunGuard};
use dbbackup_catalog::writer::CatalogWriterHandle;

/// 스케줄러 진입점이 쓰는 exit code. Task Scheduler의 RestartOnFailure는 "exit != 0"을 실패로 보고
/// 재시도를 건다 — 정상 실행 중인 이전 인스턴스를 건드리지 않기 위해 스킵(overlap)은 의도적으로
/// 0(성공)으로 리턴한다(FR6). 가시성은 로그/알림이 담당한다.
pub const EXIT_SUCCESS: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_SKIPPED_OVERLAP: i32 = 0; // 의도적으로 EXIT_SUCCESS와 동일값
pub const EXIT_SCHEDULER_ERROR: i32 = 2;

pub enum AcquireOutcome {
    Acquired(RunGuard),
    /// 이미 실행 중 — catch-up이 아직 끝나지 않은 인스턴스와 겹침.
    Skipped,
}

/// try_acquire_run을 호출하고, "이미 실행 중"과 "그 외 오류"를 구분해 로그/의미를 명확히 한다.
pub fn acquire_or_skip(
    writer: &CatalogWriterHandle,
    job: &str,
) -> Result<AcquireOutcome, OverlapError> {
    match try_acquire_run(writer, job) {
        Ok(guard) => Ok(AcquireOutcome::Acquired(guard)),
        Err(OverlapError::AlreadyRunning(_)) => {
            tracing::warn!(
                job,
                "overlap guard: 이미 실행 중인 인스턴스가 있어 이번 트리거(catch-up 포함)를 스킵/coalesce함"
            );
            Ok(AcquireOutcome::Skipped)
        }
        // 뮤텍스/DB 오류 등은 진짜 오류로 전파(entrypoint가 EXIT_SCHEDULER_ERROR로 처리).
        Err(e) => Err(e),
    }
}
