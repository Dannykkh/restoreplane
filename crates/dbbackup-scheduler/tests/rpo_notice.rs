// T13~T15 — RPO notice 계산 + 문구.
use dbbackup_catalog::CatalogWriterHandle;
use dbbackup_scheduler::cron::ScheduleSpec;
use dbbackup_scheduler::rpo::{build_notice, compute_rpo_notice, format_notice};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

const DAILY: ScheduleSpec = ScheduleSpec::Daily { hour: 3, minute: 0 };

#[test]
fn t13_within_target_when_recent() {
    let now = OffsetDateTime::now_utc();
    let last = (now - Duration::hours(12)).format(&Rfc3339).unwrap();
    let n = build_notice("j", &DAILY, Some(&last), now);
    assert!(n.within_target, "12h 경과는 목표(1일*1.5) 이내여야 함");
    let line = format_notice(&n);
    assert!(line.contains("이내"));
}

#[test]
fn t14_exceeded_when_stale() {
    let now = OffsetDateTime::now_utc();
    // 40h > 36h(1일의 1.5배) → 초과.
    let last = (now - Duration::hours(40)).format(&Rfc3339).unwrap();
    let n = build_notice("j", &DAILY, Some(&last), now);
    assert!(!n.within_target, "40h 경과는 목표를 초과해야 함");
    let line = format_notice(&n);
    assert!(line.contains("[경고]"));
    assert!(line.contains("초과"));
}

#[test]
fn t15_no_history_warns() {
    let now = OffsetDateTime::now_utc();
    let n = build_notice("j", &DAILY, None, now);
    assert!(!n.within_target);
    let line = format_notice(&n);
    assert!(line.contains("보장 불가"));
}

#[test]
fn weekly_target_is_seven_days() {
    let now = OffsetDateTime::now_utc();
    let last = (now - Duration::days(8)).format(&Rfc3339).unwrap();
    // 주간 목표(7일*1.5=10.5일) 이내이므로 8일 경과는 within_target.
    let n = build_notice(
        "j",
        &ScheduleSpec::Weekly { hour: 3, minute: 0, day: dbbackup_scheduler::cron::Weekday::Mon },
        Some(&last),
        now,
    );
    assert!(n.within_target);
    assert_eq!(n.target_interval, Duration::days(7));
}

#[test]
fn compute_reads_catalog_backups() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("catalog.db");
    let writer = CatalogWriterHandle::spawn(db.clone()).unwrap();

    let recent = (OffsetDateTime::now_utc() - Duration::hours(6))
        .format(&Rfc3339)
        .unwrap();
    writer
        .run(move |conn| {
            conn.execute(
                "INSERT INTO backups (id, job, source, engine, kind, started_at, status, schema_version)
                 VALUES ('b1', 'rpo-job', 'src', 'postgresql', 'full', ?1, 'success', 1)",
                dbbackup_catalog::rusqlite::params![recent],
            )?;
            Ok(())
        })
        .unwrap();

    let n = compute_rpo_notice(&db, "rpo-job", &DAILY).unwrap();
    assert!(n.last_success_at.is_some());
    assert!(n.within_target);

    // 이력 없는 job.
    let none = compute_rpo_notice(&db, "missing-job", &DAILY).unwrap();
    assert!(none.last_success_at.is_none());
    assert!(!none.within_target);
}
