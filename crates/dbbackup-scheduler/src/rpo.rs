// src/rpo.rs — RPO Notice(FR9). 매 run 종료 시 마지막 성공 백업 이후 경과 시간과 목표 대비 초과 여부를 출력.
use crate::cron::ScheduleSpec;
use dbbackup_catalog::open_reader_conn;
use std::path::Path;
use time::{Duration, OffsetDateTime};

pub struct RpoNotice {
    pub job: String,
    pub last_success_at: Option<OffsetDateTime>,
    pub elapsed_since_last_success: Option<Duration>,
    pub target_interval: Duration,
    pub within_target: bool,
}

fn target_interval_of(spec: &ScheduleSpec) -> Duration {
    match spec {
        ScheduleSpec::Daily { .. } => Duration::days(1),
        ScheduleSpec::Weekly { .. } => Duration::days(7),
    }
}

/// backups.started_at은 RFC3339이거나 SQLite datetime("YYYY-MM-DD HH:MM:SS", UTC 가정)일 수 있다.
fn parse_timestamp(s: &str) -> Option<OffsetDateTime> {
    if let Ok(t) = OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339) {
        return Some(t);
    }
    // SQLite strftime 포맷 폴백(UTC로 간주).
    let fmt = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day] [hour]:[minute]:[second]",
    )
    .ok()?;
    time::PrimitiveDateTime::parse(s, &fmt)
        .ok()
        .map(|p| p.assume_utc())
}

/// job의 마지막 성공 백업 시각을 카탈로그(읽기 전용)에서 조회해 RPO 경과를 계산한다.
/// 카탈로그는 캐시이므로(S08 H5 원칙) 이 notice는 "정보 제공"용이지 보안 판단에 쓰지 않는다.
pub fn compute_rpo_notice(
    catalog_db_path: &Path,
    job: &str,
    schedule: &ScheduleSpec,
) -> anyhow::Result<RpoNotice> {
    let conn = open_reader_conn(catalog_db_path)?;
    let last: Option<String> = conn
        .query_row(
            "SELECT started_at FROM backups WHERE job=?1 AND status='success' ORDER BY started_at DESC LIMIT 1",
            dbbackup_catalog::rusqlite::params![job],
            |row| row.get(0),
        )
        .ok();
    Ok(build_notice(
        job,
        schedule,
        last.as_deref(),
        OffsetDateTime::now_utc(),
    ))
}

/// 순수 계산부(테스트 가능). now를 주입받아 시각 의존성을 제거한다.
pub fn build_notice(
    job: &str,
    schedule: &ScheduleSpec,
    last_success_raw: Option<&str>,
    now: OffsetDateTime,
) -> RpoNotice {
    let target_interval = target_interval_of(schedule);
    let last_success_at = last_success_raw.and_then(parse_timestamp);
    let elapsed = last_success_at.map(|t| now - t);
    let within_target = match elapsed {
        // 목표의 1.5배까지 허용(경고 임계값).
        Some(e) => e <= target_interval + target_interval / 2,
        None => false,
    };
    RpoNotice {
        job: job.to_string(),
        last_success_at,
        elapsed_since_last_success: elapsed,
        target_interval,
        within_target,
    }
}

/// notice를 사람이 읽는 한 줄로 포맷(테스트 가능한 순수 함수).
pub fn format_notice(n: &RpoNotice) -> String {
    match (n.last_success_at, n.within_target) {
        (Some(t), true) => format!(
            "RPO notice: job '{}' — 최근 성공 백업 {} (경과 {}h{}m), 목표 RPO(~{}일) 이내.",
            n.job,
            t,
            n.elapsed_since_last_success.unwrap().whole_hours(),
            n.elapsed_since_last_success.unwrap().whole_minutes() % 60,
            n.target_interval.whole_days(),
        ),
        (Some(t), false) => format!(
            "RPO notice: [경고] job '{}' — 최근 성공 백업 {} 이후 경과 {}h — 목표 RPO(~{}일)를 초과함. \
             지금 랜섬웨어 피해가 발생하면 이 경과 시간만큼 데이터가 유실됩니다.",
            n.job,
            t,
            n.elapsed_since_last_success.unwrap().whole_hours(),
            n.target_interval.whole_days(),
        ),
        (None, _) => format!(
            "RPO notice: [경고] job '{}' — 성공한 백업 이력이 없습니다. 현재 RPO 보장 불가.",
            n.job
        ),
    }
}

pub fn print_and_log(n: &RpoNotice) {
    let line = format_notice(n);
    println!("{line}");
    if n.within_target {
        tracing::info!("{line}");
    } else {
        tracing::warn!("{line}");
    }
}
