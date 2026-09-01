use std::str::FromStr;

use restoreplane_domain::PlanRevisionId;
use rusqlite::{params, TransactionBehavior};

use crate::OperationalStore;

use super::RepositoryError;

const MAX_SCHEDULED_PLANS_PER_TICK: i64 = 1_024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledPlanRecord {
    plan_revision_id: PlanRevisionId,
    schedule_expression: String,
    timezone: String,
    catchup_policy: String,
    overlap_policy: String,
    published_at_ms: i64,
    last_evaluated_at_ms: Option<i64>,
    last_materialized_for_ms: Option<i64>,
    last_error_code: Option<String>,
    has_active_backup_work: bool,
}

impl ScheduledPlanRecord {
    pub fn plan_revision_id(&self) -> &PlanRevisionId {
        &self.plan_revision_id
    }

    pub fn schedule_expression(&self) -> &str {
        &self.schedule_expression
    }

    pub fn timezone(&self) -> &str {
        &self.timezone
    }

    pub fn catchup_policy(&self) -> &str {
        &self.catchup_policy
    }

    pub fn overlap_policy(&self) -> &str {
        &self.overlap_policy
    }

    pub const fn published_at_ms(&self) -> i64 {
        self.published_at_ms
    }

    pub const fn last_evaluated_at_ms(&self) -> Option<i64> {
        self.last_evaluated_at_ms
    }

    pub const fn last_materialized_for_ms(&self) -> Option<i64> {
        self.last_materialized_for_ms
    }

    pub fn last_error_code(&self) -> Option<&str> {
        self.last_error_code.as_deref()
    }

    pub const fn has_active_backup_work(&self) -> bool {
        self.has_active_backup_work
    }
}

#[derive(Clone, Debug)]
pub struct SchedulerCursorAdvanceSpec {
    plan_revision_id: PlanRevisionId,
    evaluated_at_ms: i64,
    materialized_for_ms: Option<i64>,
    error_code: Option<String>,
}

impl SchedulerCursorAdvanceSpec {
    pub fn success(
        plan_revision_id: PlanRevisionId,
        evaluated_at_ms: i64,
        materialized_for_ms: Option<i64>,
    ) -> Self {
        Self {
            plan_revision_id,
            evaluated_at_ms,
            materialized_for_ms,
            error_code: None,
        }
    }

    pub fn failure(
        plan_revision_id: PlanRevisionId,
        evaluated_at_ms: i64,
        error_code: impl Into<String>,
    ) -> Self {
        Self {
            plan_revision_id,
            evaluated_at_ms,
            materialized_for_ms: None,
            error_code: Some(error_code.into()),
        }
    }

    fn validate(&self) -> Result<(), RepositoryError> {
        if self.evaluated_at_ms < 0
            || self
                .materialized_for_ms
                .is_some_and(|value| value < 0 || value > self.evaluated_at_ms)
            || self.error_code.as_deref().is_some_and(|code| {
                code.trim().is_empty() || code.len() > 256 || code.chars().any(char::is_whitespace)
            })
        {
            return Err(RepositoryError::RuntimeStateInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerCursorAdvance {
    advanced: bool,
}

impl SchedulerCursorAdvance {
    pub const fn advanced(self) -> bool {
        self.advanced
    }
}

#[derive(Clone)]
pub struct SchedulerRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn scheduler(&self) -> SchedulerRepository {
        SchedulerRepository {
            store: self.clone(),
        }
    }
}

impl SchedulerRepository {
    pub fn scheduled_plans(&self) -> Result<Vec<ScheduledPlanRecord>, RepositoryError> {
        let reader = self.store.open_reader()?;
        let mut statement = reader.connection().prepare(
            "SELECT
                 revision.id,
                 revision.schedule_expression,
                 revision.timezone,
                 revision.catchup_policy,
                 revision.overlap_policy,
                 revision.published_at_ms,
                 cursor.last_evaluated_at_ms,
                 cursor.last_materialized_for_ms,
                 cursor.last_error_code,
                 EXISTS(
                     SELECT 1
                     FROM backup_runs AS run
                     JOIN work_items AS work ON work.backup_run_id = run.id
                     WHERE run.plan_revision_id = revision.id
                       AND work.state IN (
                           'queued', 'delayed', 'leased', 'starting', 'running',
                           'committing', 'cancel_requested'
                       )
                 )
             FROM backup_plans AS plan
             JOIN backup_plan_revisions AS revision
               ON revision.id = plan.current_revision_id
             LEFT JOIN scheduler_cursors AS cursor
               ON cursor.plan_revision_id = revision.id
             WHERE plan.state = 'enabled'
               AND plan.deleted_at_ms IS NULL
               AND revision.published_at_ms IS NOT NULL
               AND revision.schedule_kind = 'cron'
               AND revision.schedule_expression IS NOT NULL
             ORDER BY COALESCE(cursor.last_evaluated_at_ms, revision.published_at_ms), revision.id
             LIMIT ?1",
        )?;
        let rows = statement
            .query_map([MAX_SCHEDULED_PLANS_PER_TICK], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, bool>(9)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| {
                Ok(ScheduledPlanRecord {
                    plan_revision_id: PlanRevisionId::from_str(&row.0)?,
                    schedule_expression: row.1,
                    timezone: row.2,
                    catchup_policy: row.3,
                    overlap_policy: row.4,
                    published_at_ms: row.5,
                    last_evaluated_at_ms: row.6,
                    last_materialized_for_ms: row.7,
                    last_error_code: row.8,
                    has_active_backup_work: row.9,
                })
            })
            .collect()
    }

    pub fn advance_cursor(
        &self,
        spec: SchedulerCursorAdvanceSpec,
    ) -> Result<SchedulerCursorAdvance, RepositoryError> {
        spec.validate()?;
        self.store
            .run_write(move |connection| {
                let transaction =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let advanced = transaction.execute(
                    "INSERT INTO scheduler_cursors(
                         plan_revision_id, last_evaluated_at_ms,
                         last_materialized_for_ms, last_error_code, updated_at_ms
                     )
                     SELECT
                         revision.id, ?2, ?3, ?4, ?2
                     FROM backup_plan_revisions AS revision
                     JOIN backup_plans AS plan
                       ON plan.current_revision_id = revision.id
                     WHERE revision.id = ?1
                       AND revision.published_at_ms IS NOT NULL
                       AND revision.schedule_kind = 'cron'
                       AND revision.published_at_ms <= ?2
                       AND plan.state = 'enabled'
                       AND plan.deleted_at_ms IS NULL
                     ON CONFLICT(plan_revision_id) DO UPDATE SET
                         last_evaluated_at_ms = excluded.last_evaluated_at_ms,
                         last_materialized_for_ms = COALESCE(
                             excluded.last_materialized_for_ms,
                             scheduler_cursors.last_materialized_for_ms
                         ),
                         last_error_code = excluded.last_error_code,
                         updated_at_ms = excluded.updated_at_ms
                     WHERE excluded.last_evaluated_at_ms >= scheduler_cursors.last_evaluated_at_ms",
                    params![
                        spec.plan_revision_id.as_str(),
                        spec.evaluated_at_ms,
                        spec.materialized_for_ms,
                        spec.error_code,
                    ],
                )?;
                transaction.commit()?;
                Ok(SchedulerCursorAdvance {
                    advanced: advanced == 1,
                })
            })
            .map_err(RepositoryError::from)?
    }
}
