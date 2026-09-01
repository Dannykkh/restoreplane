// T4~T5, T16~T17 — schtasks 실 프로세스 통합(Windows 전용).
//
// 이 테스트는 실제로 schtasks로 throwaway 태스크를 등록/조회/삭제한다. 비상승(non-elevated) 셸이나
// 서비스 계정에 "배치 작업으로 로그온" 권한이 없는 환경에서는 등록이 access-denied로 실패할 수 있으므로,
// 그런 경우 실패가 아니라 eprintln 후 SKIP한다(CI/비상승 셸에서 스위트를 깨뜨리지 않기 위함).
// 기본 스위트에서 빠지도록 #[ignore]로 게이팅한다 — `cargo test -- --ignored`로 수동 실행.
#![cfg(windows)]

use restoreplane_scheduler::cron::ScheduleSpec;
use restoreplane_scheduler::register::{
    query_task_xml, register_idempotent, register_task, remove_task, RegisterOutcome,
};
use restoreplane_scheduler::xml::{LogonType, TaskXmlParams};
use time::OffsetDateTime;

fn selftest_params(job: &str) -> TaskXmlParams {
    // 현재 사용자 계정으로 등록 시도(S4U). 권한 부족 시 아래에서 감지해 SKIP한다.
    // ".\\user" 로컬 표기는 일부 환경에서 SID 매핑에 실패하므로 "DOMAIN\\USER"(USERDOMAIN)로 채운다.
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "SYSTEM".to_string());
    let account = match std::env::var("USERDOMAIN") {
        Ok(domain) if !domain.is_empty() => format!("{domain}\\{user}"),
        _ => user,
    };
    TaskXmlParams {
        job_name: job.to_string(),
        // 해가 없는 커맨드(실행되어도 무해). 실제 실행은 이 테스트에서 트리거하지 않는다.
        exe_path: r"C:\Windows\System32\cmd.exe".to_string(),
        config_path: r"C:\Windows\Temp\restoreplane.toml".to_string(),
        working_dir: r"C:\Windows\Temp".to_string(),
        service_account: account,
        logon_type: LogonType::S4U,
        schedule: ScheduleSpec::Daily { hour: 3, minute: 0 },
        next_start_boundary: OffsetDateTime::now_utc(),
        retry_attempts: 3,
        retry_backoff_min: 10,
    }
}

fn is_permission_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("access is denied")
        || m.contains("denied")
        || m.contains("권한")
        || m.contains("액세스가 거부")
        || m.contains("logon")
        || m.contains("로그온")
        // 계정 SID 매핑 실패(전용 서비스 계정 미프로비저닝 등) — 환경 문제이므로 SKIP 취급.
        || m.contains("no mapping")
        || m.contains("매핑")
        || m.contains("식별자")
}

#[tokio::test]
#[ignore = "schtasks 실 등록 필요(상승 권한/배치 로그온 권한). 수동: cargo test -- --ignored"]
async fn t4_t16_t17_register_query_delete_lifecycle() {
    let job = format!("restoreplane-selftest-{}", std::process::id());
    let dir = tempfile::tempdir().unwrap();

    // --- 등록(T4) ---
    match register_task(&selftest_params(&job), dir.path()).await {
        Ok(_) => {}
        Err(e) => {
            let msg = e.to_string();
            if is_permission_error(&msg) {
                eprintln!("SKIP: schtasks 등록에 권한이 필요함(비상승 셸 추정): {msg}");
                return;
            }
            panic!("예상치 못한 등록 실패: {msg}");
        }
    }

    // --- 재조회 정합성(T4) ---
    let xml = query_task_xml(&job)
        .await
        .expect("query 실패")
        .expect("등록된 태스크를 재조회하지 못함");
    assert!(
        xml.contains("StartWhenAvailable") && xml.to_lowercase().contains("true"),
        "재조회 XML에 StartWhenAvailable가 없음"
    );
    assert!(
        xml.contains("IgnoreNew"),
        "재조회 XML에 MultipleInstancesPolicy=IgnoreNew가 없음"
    );

    // --- 멱등 재등록(T16) ---
    let outcome = register_idempotent(&selftest_params(&job), dir.path())
        .await
        .expect("멱등 재등록 실패");
    assert_eq!(
        outcome,
        RegisterOutcome::Unchanged,
        "동일 파라미터 재등록은 Unchanged여야 함"
    );

    // --- 삭제(T5/T17) ---
    remove_task(&job).await.expect("삭제 실패");
    let after = query_task_xml(&job).await.expect("삭제 후 query 실패");
    assert!(after.is_none(), "삭제 후에는 태스크가 없어야 함");
}
