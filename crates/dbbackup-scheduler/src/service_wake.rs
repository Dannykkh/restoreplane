use dbbackup_catalog::{OccurrenceMaterializationSpec, RepositoryError, RuntimeRepository};
use dbbackup_domain::{OccurrenceOrigin, PlanRevisionId};

use crate::occurrence::{DueEvaluation, DueOrigin, MissedReason};

#[derive(Clone)]
pub struct ServiceWake {
    repository: RuntimeRepository,
    plan_revision_id: PlanRevisionId,
}

impl ServiceWake {
    pub fn new(repository: RuntimeRepository, plan_revision_id: PlanRevisionId) -> Self {
        Self {
            repository,
            plan_revision_id,
        }
    }

    pub fn materialize(
        &self,
        evaluation: &DueEvaluation,
        requested_at_ms: i64,
    ) -> Result<WakeMaterializationReport, RepositoryError> {
        if requested_at_ms < 0 {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        let mut report = WakeMaterializationReport::default();
        for due in evaluation.due() {
            let origin = match due.origin() {
                DueOrigin::Scheduled => OccurrenceOrigin::Scheduled,
                DueOrigin::CatchUp => OccurrenceOrigin::CatchUp,
            };
            let outcome =
                self.repository
                    .materialize_occurrence(OccurrenceMaterializationSpec::due(
                        self.plan_revision_id.clone(),
                        origin,
                        due.scheduled_for_ms(),
                        requested_at_ms,
                    ))?;
            report.created_occurrences += usize::from(outcome.occurrence_created());
            report.created_backup_runs += outcome.backup_runs_created();
            report.created_work_items += outcome.work_items_created();
        }
        for missed in evaluation.missed() {
            let origin = match missed.reason() {
                MissedReason::DstGap => OccurrenceOrigin::Scheduled,
                MissedReason::CatchUpPolicySkip | MissedReason::SupersededByLatestCatchUp => {
                    OccurrenceOrigin::CatchUp
                }
            };
            let outcome =
                self.repository
                    .materialize_occurrence(OccurrenceMaterializationSpec::missed(
                        self.plan_revision_id.clone(),
                        origin,
                        missed.scheduled_for_ms(),
                        requested_at_ms,
                        missed.reason().as_str(),
                    ))?;
            let created = usize::from(outcome.occurrence_created());
            report.created_occurrences += created;
            report.missed_occurrences += created;
        }
        Ok(report)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WakeMaterializationReport {
    created_occurrences: usize,
    created_backup_runs: usize,
    created_work_items: usize,
    missed_occurrences: usize,
}

impl WakeMaterializationReport {
    pub const fn created_occurrences(self) -> usize {
        self.created_occurrences
    }

    pub const fn created_backup_runs(self) -> usize {
        self.created_backup_runs
    }

    pub const fn created_work_items(self) -> usize {
        self.created_work_items
    }

    pub const fn missed_occurrences(self) -> usize {
        self.missed_occurrences
    }
}
