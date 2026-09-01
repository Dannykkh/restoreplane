// crates/restoreplane-cli/src/commands/schedule.rs
//
// S10 — Windows Task Scheduler(schtasks) 등록/갱신/삭제 배선.
// overlap guard/알림/리댁션은 restoreplane-scheduler 크레이트가 담당하고, 여기서는 config -> XML 파라미터
// 변환과 register/remove 호출만 한다.
//
// 편차(문서화): config 스키마에 서비스 계정 필드가 아직 없으므로(S11 프로비저닝 범위), 기본값으로
// 현재 사용자(".\\<USERNAME>")를 S4U로 채운다. S11에서 전용 서비스 계정 필드가 추가되면 그 값으로 교체.
use std::path::Path;

use restoreplane_scheduler::cron::parse_cron_subset;
use restoreplane_scheduler::register::{register_idempotent, remove_task, RegisterOutcome};
use restoreplane_scheduler::xml::{LogonType, TaskXmlParams};
use time::OffsetDateTime;

use crate::cli::ScheduleArgs;

pub fn run(config_path: &Path, args: ScheduleArgs) -> anyhow::Result<()> {
    if !args.remove {
        crate::commands::direct_execution_guard::enforce()?;
    }
    let config = restoreplane_config::Config::load(config_path)?;
    restoreplane_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    // 이 커맨드는 schtasks(비동기 프로세스 스폰)를 호출하므로 로컬 tokio 런타임을 띄운다.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    if args.remove {
        rt.block_on(remove_task(&args.job))?;
        println!("schedule: 태스크 제거 완료 (job='{}')", args.job);
        return Ok(());
    }

    // 대상 job 조회.
    let job = config
        .jobs
        .iter()
        .find(|j| j.name == args.job)
        .ok_or_else(|| anyhow::anyhow!("config에 job '{}'이 없습니다", args.job))?;

    let schedule = parse_cron_subset(&job.schedule)
        .map_err(|e| anyhow::anyhow!("job '{}'의 schedule 파싱 실패: {e}", job.name))?;

    let exe_path = std::env::current_exe()?.to_string_lossy().into_owned();
    let registered_config_path = std::path::absolute(config_path)?
        .to_string_lossy()
        .into_owned();
    let working_dir = std::env::current_dir()?.to_string_lossy().into_owned();
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "SYSTEM".to_string());
    // "DOMAIN\\USER"(USERDOMAIN) 형식이 SID 매핑에 안전하다(로컬 ".\\user" 표기는 일부 환경에서 실패).
    let service_account = match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    };

    // 재시도 정책은 [policy.retry]에서 가져온다(FR1). 초 단위 backoff를 분으로 환산(최소 1분).
    let retry = &config.policy.retry;
    let retry_backoff_min = (retry.backoff_secs / 60).max(1);

    let params = TaskXmlParams {
        job_name: job.name.clone(),
        exe_path,
        config_path: registered_config_path,
        working_dir,
        service_account,
        logon_type: LogonType::S4U, // FR2 기본값: 비밀번호 미저장.
        schedule,
        next_start_boundary: OffsetDateTime::now_utc(),
        retry_attempts: retry.attempts,
        retry_backoff_min,
    };

    // XML 파일은 config와 같은 디렉터리(없으면 현재 디렉터리)에 쓴다.
    let xml_dir = config_path.parent().filter(|p| !p.as_os_str().is_empty());
    let xml_dir = xml_dir.unwrap_or_else(|| Path::new("."));

    match rt.block_on(register_idempotent(&params, xml_dir))? {
        RegisterOutcome::Registered => {
            println!("schedule: 태스크 등록/갱신 완료 (job='{}')", job.name)
        }
        RegisterOutcome::Unchanged => {
            println!("schedule: 변경 없음 — 재등록 스킵 (job='{}')", job.name)
        }
    }
    Ok(())
}
