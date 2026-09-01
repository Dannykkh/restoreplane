use chrono::{
    DateTime, Datelike, Duration as ChronoDuration, LocalResult, NaiveDate, TimeZone, Utc,
};
use chrono_tz::Tz;
use thiserror::Error;

use crate::{ScheduleSpec, Weekday};

pub const MAX_EVALUATION_WINDOW_MS: i64 = 366 * 24 * 60 * 60 * 1_000;
const MAX_DST_GAP_MINUTES: i64 = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeMode {
    Regular,
    Recovery,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatchUpPolicy {
    Skip,
    BoundedLatestOne,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DueOrigin {
    Scheduled,
    CatchUp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DueResolution {
    Exact,
    DstFoldEarliest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissedReason {
    DstGap,
    CatchUpPolicySkip,
    SupersededByLatestCatchUp,
}

impl MissedReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DstGap => "dst_gap",
            Self::CatchUpPolicySkip => "catchup_policy_skip",
            Self::SupersededByLatestCatchUp => "superseded_by_latest_catchup",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DueOccurrence {
    scheduled_for_ms: i64,
    origin: DueOrigin,
    resolution: DueResolution,
}

impl DueOccurrence {
    pub const fn scheduled_for_ms(&self) -> i64 {
        self.scheduled_for_ms
    }

    pub const fn origin(&self) -> DueOrigin {
        self.origin
    }

    pub const fn resolution(&self) -> DueResolution {
        self.resolution
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MissedOccurrence {
    scheduled_for_ms: i64,
    reason: MissedReason,
}

impl MissedOccurrence {
    pub const fn scheduled_for_ms(&self) -> i64 {
        self.scheduled_for_ms
    }

    pub const fn reason(&self) -> MissedReason {
        self.reason
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DueEvaluation {
    due: Vec<DueOccurrence>,
    missed: Vec<MissedOccurrence>,
}

impl DueEvaluation {
    pub fn due(&self) -> &[DueOccurrence] {
        &self.due
    }

    pub fn missed(&self) -> &[MissedOccurrence] {
        &self.missed
    }

    /// Converts every due occurrence in a bounded historical chunk into durable
    /// missed evidence. The service uses this while walking an old cursor toward
    /// the final catch-up window so no schedule time is silently discarded and
    /// only the latest overall catch-up can create Backup work.
    pub fn mark_all_due_missed(&mut self, reason: MissedReason) {
        self.missed
            .extend(self.due.drain(..).map(|occurrence| MissedOccurrence {
                scheduled_for_ms: occurrence.scheduled_for_ms,
                reason,
            }));
        self.missed
            .sort_by_key(|occurrence| occurrence.scheduled_for_ms);
        self.missed
            .dedup_by_key(|occurrence| occurrence.scheduled_for_ms);
    }
}

#[derive(Debug, Error)]
pub enum ScheduleEvaluationError {
    #[error("SCHEDULE_TIMEZONE_INVALID")]
    TimezoneInvalid,
    #[error("SCHEDULE_WINDOW_INVALID")]
    WindowInvalid,
    #[error("SCHEDULE_DATE_RANGE_INVALID")]
    DateRangeInvalid,
}

impl ScheduleEvaluationError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::TimezoneInvalid => "SCHEDULE_TIMEZONE_INVALID",
            Self::WindowInvalid => "SCHEDULE_WINDOW_INVALID",
            Self::DateRangeInvalid => "SCHEDULE_DATE_RANGE_INVALID",
        }
    }
}

pub fn evaluate_due_window(
    schedule: &ScheduleSpec,
    timezone: &str,
    after_exclusive_ms: i64,
    now_inclusive_ms: i64,
    wake_mode: WakeMode,
    catchup_policy: CatchUpPolicy,
) -> Result<DueEvaluation, ScheduleEvaluationError> {
    let window_ms = now_inclusive_ms
        .checked_sub(after_exclusive_ms)
        .ok_or(ScheduleEvaluationError::WindowInvalid)?;
    if after_exclusive_ms < 0
        || now_inclusive_ms <= after_exclusive_ms
        || window_ms > MAX_EVALUATION_WINDOW_MS
    {
        return Err(ScheduleEvaluationError::WindowInvalid);
    }
    let timezone: Tz = timezone
        .parse()
        .map_err(|_| ScheduleEvaluationError::TimezoneInvalid)?;
    let after = DateTime::<Utc>::from_timestamp_millis(after_exclusive_ms)
        .ok_or(ScheduleEvaluationError::DateRangeInvalid)?;
    let now = DateTime::<Utc>::from_timestamp_millis(now_inclusive_ms)
        .ok_or(ScheduleEvaluationError::DateRangeInvalid)?;
    let mut date = after
        .with_timezone(&timezone)
        .date_naive()
        .pred_opt()
        .ok_or(ScheduleEvaluationError::DateRangeInvalid)?;
    let end = now
        .with_timezone(&timezone)
        .date_naive()
        .succ_opt()
        .ok_or(ScheduleEvaluationError::DateRangeInvalid)?;

    let mut candidates = Vec::new();
    let mut missed = Vec::new();
    while date <= end {
        if schedule_matches_date(schedule, date) {
            for (hour, minute) in schedule_times(schedule) {
                let local = date
                    .and_hms_opt(u32::from(hour), u32::from(minute), 0)
                    .ok_or(ScheduleEvaluationError::DateRangeInvalid)?;
                match timezone.from_local_datetime(&local) {
                    LocalResult::Single(due) => push_candidate(
                        &mut candidates,
                        due.timestamp_millis(),
                        DueResolution::Exact,
                        after_exclusive_ms,
                        now_inclusive_ms,
                    ),
                    LocalResult::Ambiguous(first, second) => {
                        let due_ms = first.timestamp_millis().min(second.timestamp_millis());
                        push_candidate(
                            &mut candidates,
                            due_ms,
                            DueResolution::DstFoldEarliest,
                            after_exclusive_ms,
                            now_inclusive_ms,
                        );
                    }
                    LocalResult::None => {
                        let transition_ms = first_valid_after_gap(timezone, local)?;
                        if transition_ms > after_exclusive_ms && transition_ms <= now_inclusive_ms {
                            missed.push(MissedOccurrence {
                                scheduled_for_ms: transition_ms,
                                reason: MissedReason::DstGap,
                            });
                        }
                    }
                }
            }
        }
        date = date
            .succ_opt()
            .ok_or(ScheduleEvaluationError::DateRangeInvalid)?;
    }
    candidates.sort_by_key(|candidate| candidate.scheduled_for_ms);
    candidates.dedup_by_key(|candidate| candidate.scheduled_for_ms);
    missed.sort_by_key(|candidate| candidate.scheduled_for_ms);
    missed.dedup_by_key(|candidate| candidate.scheduled_for_ms);

    let due = match wake_mode {
        WakeMode::Regular => candidates
            .into_iter()
            .map(|candidate| DueOccurrence {
                scheduled_for_ms: candidate.scheduled_for_ms,
                origin: DueOrigin::Scheduled,
                resolution: candidate.resolution,
            })
            .collect(),
        WakeMode::Recovery => match catchup_policy {
            CatchUpPolicy::Skip => {
                missed.extend(candidates.into_iter().map(|candidate| MissedOccurrence {
                    scheduled_for_ms: candidate.scheduled_for_ms,
                    reason: MissedReason::CatchUpPolicySkip,
                }));
                Vec::new()
            }
            CatchUpPolicy::BoundedLatestOne => {
                let latest = candidates.pop();
                missed.extend(candidates.into_iter().map(|candidate| MissedOccurrence {
                    scheduled_for_ms: candidate.scheduled_for_ms,
                    reason: MissedReason::SupersededByLatestCatchUp,
                }));
                latest
                    .into_iter()
                    .map(|candidate| DueOccurrence {
                        scheduled_for_ms: candidate.scheduled_for_ms,
                        origin: DueOrigin::CatchUp,
                        resolution: candidate.resolution,
                    })
                    .collect()
            }
        },
    };
    missed.sort_by_key(|candidate| candidate.scheduled_for_ms);
    Ok(DueEvaluation { due, missed })
}

#[derive(Clone, Copy)]
struct Candidate {
    scheduled_for_ms: i64,
    resolution: DueResolution,
}

fn push_candidate(
    candidates: &mut Vec<Candidate>,
    scheduled_for_ms: i64,
    resolution: DueResolution,
    after_exclusive_ms: i64,
    now_inclusive_ms: i64,
) {
    if scheduled_for_ms > after_exclusive_ms && scheduled_for_ms <= now_inclusive_ms {
        candidates.push(Candidate {
            scheduled_for_ms,
            resolution,
        });
    }
}

fn first_valid_after_gap(
    timezone: Tz,
    local: chrono::NaiveDateTime,
) -> Result<i64, ScheduleEvaluationError> {
    for minutes in 1..=MAX_DST_GAP_MINUTES {
        let probe = local
            .checked_add_signed(ChronoDuration::minutes(minutes))
            .ok_or(ScheduleEvaluationError::DateRangeInvalid)?;
        if let Some(valid) = timezone.from_local_datetime(&probe).earliest() {
            return Ok(valid.timestamp_millis());
        }
    }
    Err(ScheduleEvaluationError::DateRangeInvalid)
}

fn schedule_times(schedule: &ScheduleSpec) -> Vec<(u8, u8)> {
    match schedule {
        ScheduleSpec::Hourly { minute } => (0..24).map(|hour| (hour, *minute)).collect(),
        ScheduleSpec::Daily { hour, minute } | ScheduleSpec::Weekly { hour, minute, .. } => {
            vec![(*hour, *minute)]
        }
    }
}

fn schedule_matches_date(schedule: &ScheduleSpec, date: NaiveDate) -> bool {
    match schedule {
        ScheduleSpec::Hourly { .. } => true,
        ScheduleSpec::Daily { .. } => true,
        ScheduleSpec::Weekly { day, .. } => {
            date.weekday()
                == match day {
                    Weekday::Sun => chrono::Weekday::Sun,
                    Weekday::Mon => chrono::Weekday::Mon,
                    Weekday::Tue => chrono::Weekday::Tue,
                    Weekday::Wed => chrono::Weekday::Wed,
                    Weekday::Thu => chrono::Weekday::Thu,
                    Weekday::Fri => chrono::Weekday::Fri,
                    Weekday::Sat => chrono::Weekday::Sat,
                }
        }
    }
}
