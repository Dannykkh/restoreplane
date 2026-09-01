use dbbackup_domain::{
    transition_backup_run, transition_plan_occurrence, transition_recovery_point,
    transition_work_attempt, transition_work_item, BackupRunCommand, BackupRunState,
    DestinationRequirement, OccurrenceCommand, OccurrenceOrigin, PlanOccurrenceState,
    ProtectionCompliance, RecoveryPointCommand, RecoveryPointState, RestoreEligibility,
    SnapshotArtifactCompleteness, SnapshotReplicaDurability, WorkAttemptCommand, WorkAttemptState,
    WorkItemCommand, WorkItemKind, WorkItemState,
};

const DDL: &str = include_str!("../../../docs/plan/connected-backup-platform-v2/schema-v2.sql");

#[test]
fn serialized_domain_enums_match_their_ddl_check_constraints() {
    assert_ddl_values(
        "plan_occurrences",
        "origin",
        OccurrenceOrigin::ALL.map(OccurrenceOrigin::as_str),
    );
    assert_ddl_values(
        "plan_occurrences",
        "state",
        PlanOccurrenceState::ALL.map(PlanOccurrenceState::as_str),
    );
    assert_ddl_values(
        "plan_revision_destinations",
        "requirement",
        DestinationRequirement::ALL.map(DestinationRequirement::as_str),
    );
    assert_ddl_values(
        "backup_runs",
        "state",
        BackupRunState::ALL.map(BackupRunState::as_str),
    );
    assert_ddl_values(
        "work_items",
        "kind",
        WorkItemKind::ALL.map(WorkItemKind::as_str),
    );
    assert_ddl_values(
        "work_items",
        "state",
        WorkItemState::ALL.map(WorkItemState::as_str),
    );
    assert_ddl_values(
        "work_attempts",
        "state",
        WorkAttemptState::ALL.map(WorkAttemptState::as_str),
    );
    assert_ddl_values(
        "catalog_recovery_points",
        "state",
        RecoveryPointState::ALL.map(RecoveryPointState::as_str),
    );
    assert_ddl_values(
        "catalog_snapshot_projection",
        "artifact_completeness",
        SnapshotArtifactCompleteness::ALL.map(SnapshotArtifactCompleteness::as_str),
    );
    assert_ddl_values(
        "catalog_snapshot_projection",
        "replica_durability",
        SnapshotReplicaDurability::ALL.map(SnapshotReplicaDurability::as_str),
    );
    assert_ddl_values(
        "catalog_snapshot_projection",
        "protection_compliance",
        ProtectionCompliance::ALL.map(ProtectionCompliance::as_str),
    );
    assert_ddl_values(
        "catalog_snapshot_projection",
        "restore_eligibility",
        RestoreEligibility::ALL.map(RestoreEligibility::as_str),
    );

    assert_eq!(
        serde_json::to_string(&WorkItemState::CancelRequested).expect("serialize enum"),
        "\"cancel_requested\""
    );
}

#[test]
fn work_item_and_attempt_transitions_reject_terminal_replay() {
    let leased = transition_work_item(WorkItemState::Queued, WorkItemCommand::Claim)
        .expect("queued item can be claimed");
    let starting =
        transition_work_item(leased, WorkItemCommand::Start).expect("leased item can start");
    let running = transition_work_item(starting, WorkItemCommand::MarkRunning)
        .expect("starting item can run");
    let committing = transition_work_item(running, WorkItemCommand::BeginCommit)
        .expect("running item can commit");
    assert_eq!(
        transition_work_item(running, WorkItemCommand::Release)
            .expect("a reaped running attempt can be retried"),
        WorkItemState::Queued
    );
    assert_eq!(
        transition_work_item(committing, WorkItemCommand::Release)
            .expect("a failed commit attempt can be retried"),
        WorkItemState::Queued
    );
    let succeeded = transition_work_item(committing, WorkItemCommand::Succeed)
        .expect("committing item can succeed");
    assert_eq!(succeeded, WorkItemState::Succeeded);
    let terminal_replay = transition_work_item(succeeded, WorkItemCommand::Succeed)
        .expect_err("terminal work item is immutable");
    assert_eq!(terminal_replay.code(), "DOMAIN_STATE_TRANSITION_INVALID");

    let attempt_start =
        transition_work_attempt(WorkAttemptState::Claimed, WorkAttemptCommand::Start)
            .expect("claimed attempt can start");
    let attempt_running = transition_work_attempt(attempt_start, WorkAttemptCommand::MarkRunning)
        .expect("attempt can run");
    let abandoned = transition_work_attempt(attempt_running, WorkAttemptCommand::Abandon)
        .expect("stale running attempt can be abandoned");
    assert_eq!(abandoned, WorkAttemptState::Abandoned);
    assert!(
        transition_work_attempt(abandoned, WorkAttemptCommand::Start).is_err(),
        "abandoned attempt must not restart"
    );
}

#[test]
fn occurrence_run_and_recovery_point_transitions_preserve_distinct_outcomes() {
    let enqueued =
        transition_plan_occurrence(PlanOccurrenceState::Pending, OccurrenceCommand::Enqueue)
            .expect("pending occurrence can enqueue");
    let running = transition_plan_occurrence(enqueued, OccurrenceCommand::Start)
        .expect("enqueued occurrence can run");
    let partial = transition_plan_occurrence(running, OccurrenceCommand::PartialSucceed)
        .expect("multi-target result can be partial");
    assert_eq!(partial, PlanOccurrenceState::PartiallySucceeded);

    let queued = transition_backup_run(BackupRunState::Created, BackupRunCommand::Enqueue)
        .expect("created backup run can enqueue");
    let running = transition_backup_run(queued, BackupRunCommand::Start)
        .expect("queued backup run can start");
    let committing = transition_backup_run(running, BackupRunCommand::BeginCommit)
        .expect("running backup run can commit");
    assert_eq!(
        transition_backup_run(committing, BackupRunCommand::PartialSucceed)
            .expect("required and best-effort outcomes stay distinct"),
        BackupRunState::PartiallySucceeded
    );

    assert_eq!(
        transition_recovery_point(
            RecoveryPointState::Incomplete,
            RecoveryPointCommand::MarkComplete,
        )
        .expect("validated incomplete recovery point can become complete"),
        RecoveryPointState::Complete
    );
    assert_eq!(
        transition_recovery_point(
            RecoveryPointState::Complete,
            RecoveryPointCommand::Quarantine,
        )
        .expect("new corruption evidence can quarantine a complete point"),
        RecoveryPointState::Quarantined
    );
    assert!(
        transition_recovery_point(
            RecoveryPointState::Quarantined,
            RecoveryPointCommand::MarkComplete,
        )
        .is_err(),
        "quarantine cannot be bypassed by a direct state write"
    );
}

fn assert_ddl_values<const N: usize>(table: &str, column: &str, expected: [&str; N]) {
    let table_start = DDL
        .find(&format!("CREATE TABLE {table} ("))
        .unwrap_or_else(|| panic!("table '{table}' missing from canonical DDL"));
    let table_tail = &DDL[table_start..];
    let table_end = table_tail
        .find(") STRICT;")
        .unwrap_or_else(|| panic!("table '{table}' has no STRICT terminator"));
    let table_ddl = &table_tail[..table_end];
    let check_start = table_ddl
        .find(&format!("{column} TEXT NOT NULL CHECK({column} IN ("))
        .unwrap_or_else(|| panic!("{table}.{column} CHECK missing"));
    let check_tail = &table_ddl[check_start..];
    let check_end = check_tail
        .find("))")
        .unwrap_or_else(|| panic!("{table}.{column} CHECK terminator missing"));
    let actual: Vec<&str> = check_tail[..check_end]
        .split('\'')
        .skip(1)
        .step_by(2)
        .collect();
    assert_eq!(actual, expected, "{table}.{column} enum drift");
}
