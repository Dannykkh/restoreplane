use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::DomainError;

macro_rules! count_variants {
    ($($variant:ident),* $(,)?) => {
        <[()]>::len(&[$(count_variants!(@one $variant)),*])
    };
    (@one $variant:ident) => { () };
}

macro_rules! string_enum {
    (
        $name:ident {
            $($variant:ident => $value:literal),+ $(,)?
        }
    ) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: [Self; count_variants!($($variant),+)] = [
                $(Self::$variant),+
            ];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $value),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = DomainError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(DomainError::InvalidEnum {
                        enum_name: stringify!($name),
                    }),
                }
            }
        }
    };
}

string_enum!(OccurrenceOrigin {
    Scheduled => "scheduled",
    CatchUp => "catch_up",
    Manual => "manual",
});

string_enum!(PlanOccurrenceState {
    Pending => "pending",
    Enqueued => "enqueued",
    Running => "running",
    Succeeded => "succeeded",
    PartiallySucceeded => "partially_succeeded",
    Failed => "failed",
    Missed => "missed",
    Cancelled => "cancelled",
});

string_enum!(DestinationRequirement {
    Required => "required",
    BestEffort => "best_effort",
});

string_enum!(BackupRunState {
    Created => "created",
    Queued => "queued",
    Running => "running",
    Committing => "committing",
    Succeeded => "succeeded",
    PartiallySucceeded => "partially_succeeded",
    Failed => "failed",
    Cancelled => "cancelled",
});

string_enum!(WorkItemKind {
    Backup => "backup",
    Restore => "restore",
    Verification => "verification",
    CatalogRebuild => "catalog_rebuild",
    Operation => "operation",
});

string_enum!(WorkItemState {
    Queued => "queued",
    Delayed => "delayed",
    Leased => "leased",
    Starting => "starting",
    Running => "running",
    Committing => "committing",
    CancelRequested => "cancel_requested",
    Succeeded => "succeeded",
    PartiallySucceeded => "partially_succeeded",
    FailedTerminal => "failed_terminal",
    Cancelled => "cancelled",
});

string_enum!(WorkAttemptState {
    Claimed => "claimed",
    Starting => "starting",
    Running => "running",
    Committing => "committing",
    Succeeded => "succeeded",
    Failed => "failed",
    Cancelled => "cancelled",
    Abandoned => "abandoned",
});

string_enum!(RecoveryPointState {
    Complete => "complete",
    Incomplete => "incomplete",
    Orphaned => "orphaned",
    Quarantined => "quarantined",
});

string_enum!(SnapshotArtifactCompleteness {
    Complete => "complete",
    Incomplete => "incomplete",
});

string_enum!(SnapshotReplicaDurability {
    Healthy => "healthy",
    Degraded => "degraded",
    Unavailable => "unavailable",
});

string_enum!(ProtectionCompliance {
    Compliant => "compliant",
    NonCompliant => "non_compliant",
    Unknown => "unknown",
});

string_enum!(RestoreEligibility {
    Eligible => "eligible",
    Conditional => "conditional",
    Blocked => "blocked",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkItemCommand {
    Enqueue,
    Delay,
    Claim,
    Start,
    MarkRunning,
    BeginCommit,
    RequestCancel,
    Succeed,
    PartialSucceed,
    FailTerminal,
    Cancel,
    Release,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkAttemptCommand {
    Start,
    MarkRunning,
    BeginCommit,
    Succeed,
    Fail,
    Cancel,
    Abandon,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OccurrenceCommand {
    Enqueue,
    Start,
    Succeed,
    PartialSucceed,
    Fail,
    Miss,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupRunCommand {
    Enqueue,
    Start,
    BeginCommit,
    Succeed,
    PartialSucceed,
    Fail,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryPointCommand {
    MarkComplete,
    MarkIncomplete,
    MarkOrphaned,
    Quarantine,
}

pub fn transition_work_item(
    current: WorkItemState,
    command: WorkItemCommand,
) -> Result<WorkItemState, DomainError> {
    use WorkItemCommand as Command;
    use WorkItemState as State;

    let next = match (current, command) {
        (State::Queued, Command::Delay) => State::Delayed,
        (State::Delayed, Command::Enqueue) => State::Queued,
        (State::Queued | State::Delayed, Command::Claim) => State::Leased,
        (State::Leased, Command::Start) => State::Starting,
        (State::Starting, Command::MarkRunning) => State::Running,
        (State::Running, Command::BeginCommit) => State::Committing,
        (
            State::Leased | State::Starting | State::Running | State::Committing,
            Command::RequestCancel,
        ) => State::CancelRequested,
        (State::Committing, Command::Succeed) => State::Succeeded,
        (State::Committing, Command::PartialSucceed) => State::PartiallySucceeded,
        (
            State::Queued
            | State::Delayed
            | State::Leased
            | State::Starting
            | State::Running
            | State::Committing
            | State::CancelRequested,
            Command::FailTerminal,
        ) => State::FailedTerminal,
        (State::Queued | State::Delayed | State::CancelRequested, Command::Cancel) => {
            State::Cancelled
        }
        (
            State::Leased | State::Starting | State::Running | State::Committing,
            Command::Release,
        ) => State::Queued,
        _ => {
            return Err(invalid_transition(
                "work_item",
                current.as_str(),
                work_item_command_str(command),
            ));
        }
    };
    Ok(next)
}

pub fn transition_work_attempt(
    current: WorkAttemptState,
    command: WorkAttemptCommand,
) -> Result<WorkAttemptState, DomainError> {
    use WorkAttemptCommand as Command;
    use WorkAttemptState as State;

    let next = match (current, command) {
        (State::Claimed, Command::Start) => State::Starting,
        (State::Starting, Command::MarkRunning) => State::Running,
        (State::Running, Command::BeginCommit) => State::Committing,
        (State::Committing, Command::Succeed) => State::Succeeded,
        (State::Claimed | State::Starting | State::Running | State::Committing, Command::Fail) => {
            State::Failed
        }
        (
            State::Claimed | State::Starting | State::Running | State::Committing,
            Command::Cancel,
        ) => State::Cancelled,
        (
            State::Claimed | State::Starting | State::Running | State::Committing,
            Command::Abandon,
        ) => State::Abandoned,
        _ => {
            return Err(invalid_transition(
                "work_attempt",
                current.as_str(),
                work_attempt_command_str(command),
            ));
        }
    };
    Ok(next)
}

pub fn transition_plan_occurrence(
    current: PlanOccurrenceState,
    command: OccurrenceCommand,
) -> Result<PlanOccurrenceState, DomainError> {
    use OccurrenceCommand as Command;
    use PlanOccurrenceState as State;

    let next = match (current, command) {
        (State::Pending, Command::Enqueue) => State::Enqueued,
        (State::Enqueued, Command::Start) => State::Running,
        (State::Running, Command::Succeed) => State::Succeeded,
        (State::Running, Command::PartialSucceed) => State::PartiallySucceeded,
        (State::Running, Command::Fail) => State::Failed,
        (State::Pending, Command::Miss) => State::Missed,
        (State::Pending | State::Enqueued | State::Running, Command::Cancel) => State::Cancelled,
        _ => {
            return Err(invalid_transition(
                "plan_occurrence",
                current.as_str(),
                occurrence_command_str(command),
            ));
        }
    };
    Ok(next)
}

pub fn transition_backup_run(
    current: BackupRunState,
    command: BackupRunCommand,
) -> Result<BackupRunState, DomainError> {
    use BackupRunCommand as Command;
    use BackupRunState as State;

    let next = match (current, command) {
        (State::Created, Command::Enqueue) => State::Queued,
        (State::Queued, Command::Start) => State::Running,
        (State::Running, Command::BeginCommit) => State::Committing,
        (State::Committing, Command::Succeed) => State::Succeeded,
        (State::Committing, Command::PartialSucceed) => State::PartiallySucceeded,
        (State::Created | State::Queued | State::Running | State::Committing, Command::Fail) => {
            State::Failed
        }
        (State::Created | State::Queued | State::Running | State::Committing, Command::Cancel) => {
            State::Cancelled
        }
        _ => {
            return Err(invalid_transition(
                "backup_run",
                current.as_str(),
                backup_run_command_str(command),
            ));
        }
    };
    Ok(next)
}

pub fn transition_recovery_point(
    current: RecoveryPointState,
    command: RecoveryPointCommand,
) -> Result<RecoveryPointState, DomainError> {
    use RecoveryPointCommand as Command;
    use RecoveryPointState as State;

    let next = match (current, command) {
        (State::Incomplete, Command::MarkComplete) => State::Complete,
        (State::Orphaned, Command::MarkIncomplete) => State::Incomplete,
        (State::Incomplete, Command::MarkOrphaned) => State::Orphaned,
        (State::Complete | State::Incomplete | State::Orphaned, Command::Quarantine) => {
            State::Quarantined
        }
        _ => {
            return Err(invalid_transition(
                "recovery_point",
                current.as_str(),
                recovery_point_command_str(command),
            ));
        }
    };
    Ok(next)
}

fn invalid_transition(
    machine: &'static str,
    current: &'static str,
    command: &'static str,
) -> DomainError {
    DomainError::InvalidTransition {
        machine,
        current,
        command,
    }
}

const fn work_item_command_str(command: WorkItemCommand) -> &'static str {
    match command {
        WorkItemCommand::Enqueue => "enqueue",
        WorkItemCommand::Delay => "delay",
        WorkItemCommand::Claim => "claim",
        WorkItemCommand::Start => "start",
        WorkItemCommand::MarkRunning => "mark_running",
        WorkItemCommand::BeginCommit => "begin_commit",
        WorkItemCommand::RequestCancel => "request_cancel",
        WorkItemCommand::Succeed => "succeed",
        WorkItemCommand::PartialSucceed => "partial_succeed",
        WorkItemCommand::FailTerminal => "fail_terminal",
        WorkItemCommand::Cancel => "cancel",
        WorkItemCommand::Release => "release",
    }
}

const fn work_attempt_command_str(command: WorkAttemptCommand) -> &'static str {
    match command {
        WorkAttemptCommand::Start => "start",
        WorkAttemptCommand::MarkRunning => "mark_running",
        WorkAttemptCommand::BeginCommit => "begin_commit",
        WorkAttemptCommand::Succeed => "succeed",
        WorkAttemptCommand::Fail => "fail",
        WorkAttemptCommand::Cancel => "cancel",
        WorkAttemptCommand::Abandon => "abandon",
    }
}

const fn occurrence_command_str(command: OccurrenceCommand) -> &'static str {
    match command {
        OccurrenceCommand::Enqueue => "enqueue",
        OccurrenceCommand::Start => "start",
        OccurrenceCommand::Succeed => "succeed",
        OccurrenceCommand::PartialSucceed => "partial_succeed",
        OccurrenceCommand::Fail => "fail",
        OccurrenceCommand::Miss => "miss",
        OccurrenceCommand::Cancel => "cancel",
    }
}

const fn backup_run_command_str(command: BackupRunCommand) -> &'static str {
    match command {
        BackupRunCommand::Enqueue => "enqueue",
        BackupRunCommand::Start => "start",
        BackupRunCommand::BeginCommit => "begin_commit",
        BackupRunCommand::Succeed => "succeed",
        BackupRunCommand::PartialSucceed => "partial_succeed",
        BackupRunCommand::Fail => "fail",
        BackupRunCommand::Cancel => "cancel",
    }
}

const fn recovery_point_command_str(command: RecoveryPointCommand) -> &'static str {
    match command {
        RecoveryPointCommand::MarkComplete => "mark_complete",
        RecoveryPointCommand::MarkIncomplete => "mark_incomplete",
        RecoveryPointCommand::MarkOrphaned => "mark_orphaned",
        RecoveryPointCommand::Quarantine => "quarantine",
    }
}
