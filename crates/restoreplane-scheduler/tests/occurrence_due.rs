use restoreplane_scheduler::occurrence::{
    evaluate_due_window, CatchUpPolicy, DueOrigin, DueResolution, MissedReason, WakeMode,
};
use restoreplane_scheduler::ScheduleSpec;

#[test]
fn dst_fold_runs_once_at_the_earliest_utc_instant() {
    let evaluation = evaluate_due_window(
        &ScheduleSpec::Daily {
            hour: 1,
            minute: 30,
        },
        "America/New_York",
        1_730_606_400_000,
        1_730_617_200_000,
        WakeMode::Regular,
        CatchUpPolicy::BoundedLatestOne,
    )
    .expect("evaluate fallback window");

    assert_eq!(evaluation.due().len(), 1);
    assert_eq!(evaluation.due()[0].scheduled_for_ms(), 1_730_611_800_000);
    assert_eq!(evaluation.due()[0].origin(), DueOrigin::Scheduled);
    assert_eq!(
        evaluation.due()[0].resolution(),
        DueResolution::DstFoldEarliest
    );
    assert!(evaluation.missed().is_empty());
}

#[test]
fn dst_gap_is_recorded_as_missed_at_the_transition_instant() {
    let evaluation = evaluate_due_window(
        &ScheduleSpec::Daily {
            hour: 2,
            minute: 30,
        },
        "America/New_York",
        1_710_050_400_000,
        1_710_057_600_000,
        WakeMode::Regular,
        CatchUpPolicy::BoundedLatestOne,
    )
    .expect("evaluate spring-forward window");

    assert!(evaluation.due().is_empty());
    assert_eq!(evaluation.missed().len(), 1);
    assert_eq!(evaluation.missed()[0].scheduled_for_ms(), 1_710_054_000_000);
    assert_eq!(evaluation.missed()[0].reason(), MissedReason::DstGap);
}

#[test]
fn recovery_wake_keeps_only_latest_due_and_records_older_misses() {
    let evaluation = evaluate_due_window(
        &ScheduleSpec::Daily { hour: 3, minute: 0 },
        "UTC",
        1_735_689_600_000,
        1_735_963_200_000,
        WakeMode::Recovery,
        CatchUpPolicy::BoundedLatestOne,
    )
    .expect("evaluate catch-up");

    assert_eq!(evaluation.due().len(), 1);
    assert_eq!(evaluation.due()[0].scheduled_for_ms(), 1_735_959_600_000);
    assert_eq!(evaluation.due()[0].origin(), DueOrigin::CatchUp);
    assert_eq!(evaluation.missed().len(), 3);
    assert!(evaluation
        .missed()
        .iter()
        .all(|missed| missed.reason() == MissedReason::SupersededByLatestCatchUp));

    let skipped = evaluate_due_window(
        &ScheduleSpec::Daily { hour: 3, minute: 0 },
        "UTC",
        1_735_689_600_000,
        1_735_963_200_000,
        WakeMode::Recovery,
        CatchUpPolicy::Skip,
    )
    .expect("evaluate skipped catch-up");
    assert!(skipped.due().is_empty());
    assert_eq!(skipped.missed().len(), 4);
    assert!(skipped
        .missed()
        .iter()
        .all(|missed| missed.reason() == MissedReason::CatchUpPolicySkip));
}

#[test]
fn invalid_timezone_and_unbounded_window_fail_closed() {
    let invalid_zone = evaluate_due_window(
        &ScheduleSpec::Daily { hour: 3, minute: 0 },
        "Not/AZone",
        1_735_689_600_000,
        1_735_693_200_000,
        WakeMode::Regular,
        CatchUpPolicy::Skip,
    )
    .expect_err("unknown timezone");
    assert_eq!(invalid_zone.code(), "SCHEDULE_TIMEZONE_INVALID");

    let unbounded = evaluate_due_window(
        &ScheduleSpec::Daily { hour: 3, minute: 0 },
        "UTC",
        0,
        40_000_000_000,
        WakeMode::Recovery,
        CatchUpPolicy::BoundedLatestOne,
    )
    .expect_err("window is bounded");
    assert_eq!(unbounded.code(), "SCHEDULE_WINDOW_INVALID");
}

#[test]
fn hourly_schedule_enumerates_each_due_hour_in_the_bounded_window() {
    let evaluation = evaluate_due_window(
        &ScheduleSpec::Hourly { minute: 15 },
        "UTC",
        1_735_689_600_000,
        1_735_700_500_000,
        WakeMode::Regular,
        CatchUpPolicy::BoundedLatestOne,
    )
    .expect("bounded hourly window");

    let scheduled: Vec<i64> = evaluation
        .due()
        .iter()
        .map(|occurrence| occurrence.scheduled_for_ms())
        .collect();
    assert_eq!(
        scheduled,
        [1_735_690_500_000, 1_735_694_100_000, 1_735_697_700_000,]
    );
}
