// T1 — cron 서브셋 파싱.
use dbbackup_scheduler::cron::{parse_cron_subset, CronError, ScheduleSpec, Weekday};

#[test]
fn daily_pattern() {
    assert_eq!(
        parse_cron_subset("0 3 * * *").unwrap(),
        ScheduleSpec::Daily { hour: 3, minute: 0 }
    );
    assert_eq!(
        parse_cron_subset("30 23 * * *").unwrap(),
        ScheduleSpec::Daily { hour: 23, minute: 30 }
    );
}

#[test]
fn weekly_pattern() {
    assert_eq!(
        parse_cron_subset("30 3 * * 1").unwrap(),
        ScheduleSpec::Weekly { hour: 3, minute: 30, day: Weekday::Mon }
    );
    // 0과 7은 모두 일요일.
    assert_eq!(
        parse_cron_subset("0 5 * * 0").unwrap(),
        ScheduleSpec::Weekly { hour: 5, minute: 0, day: Weekday::Sun }
    );
    assert_eq!(
        parse_cron_subset("0 5 * * 7").unwrap(),
        ScheduleSpec::Weekly { hour: 5, minute: 0, day: Weekday::Sun }
    );
}

#[test]
fn rejects_unsupported_expressions() {
    // 스텝.
    assert!(matches!(parse_cron_subset("*/15 * * * *"), Err(CronError::Unsupported(_))));
    // 일 필드 고정.
    assert!(matches!(parse_cron_subset("0 3 1 * *"), Err(CronError::Unsupported(_))));
    // 리스트.
    assert!(matches!(parse_cron_subset("0 3 * * 1,2"), Err(CronError::Unsupported(_))));
    // 범위.
    assert!(matches!(parse_cron_subset("0 3 * * 1-5"), Err(CronError::Unsupported(_))));
    // 필드 개수 오류.
    assert!(matches!(parse_cron_subset("0 3 * *"), Err(CronError::WrongFieldCount(_))));
    // 범위 초과.
    assert!(matches!(parse_cron_subset("70 3 * * *"), Err(CronError::Unsupported(_))));
    assert!(matches!(parse_cron_subset("0 25 * * *"), Err(CronError::Unsupported(_))));
    // 알 수 없는 요일.
    assert!(matches!(parse_cron_subset("0 3 * * 9"), Err(CronError::Unsupported(_))));
}
