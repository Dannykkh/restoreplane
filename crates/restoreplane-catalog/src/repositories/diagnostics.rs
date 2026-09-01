use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::OperationalStore;

use super::RepositoryError;

const MAX_ACTIVITY_EVENTS: i64 = 500;
const ACTIVITY_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

#[derive(Clone)]
pub struct DiagnosticsRepository {
    store: OperationalStore,
}

impl OperationalStore {
    pub fn diagnostics(&self) -> DiagnosticsRepository {
        DiagnosticsRepository {
            store: self.clone(),
        }
    }
}

impl DiagnosticsRepository {
    pub fn collect(&self, now_ms: i64) -> Result<ServiceDiagnostics, RepositoryError> {
        if now_ms < 0 {
            return Err(RepositoryError::DiagnosticsInputInvalid);
        }
        let writer = self.store.writer_diagnostics()?;
        let reader = self.store.open_reader()?;
        let connection = reader.connection();

        let queue_states = state_counts(
            connection,
            "SELECT state, COUNT(*) FROM work_items GROUP BY state ORDER BY state",
        )?;
        let queue_total = sum_counts(&queue_states);
        let active_leases = count(
            connection,
            "SELECT COUNT(*) FROM work_items
             WHERE state IN ('leased', 'starting', 'running', 'committing', 'cancel_requested')",
        )?;
        let stale_leases = connection.query_row(
            "SELECT COUNT(*) FROM work_items
             WHERE state IN ('leased', 'starting', 'running', 'committing', 'cancel_requested')
               AND lease_expires_at_ms < ?1",
            [now_ms],
            |row| row.get(0),
        )?;

        let enabled_capacity = count(
            connection,
            "SELECT COALESCE(SUM(capacity), 0) FROM resource_pools WHERE enabled = 1",
        )?;
        let active_allocations = count(
            connection,
            "SELECT COUNT(*) FROM resource_allocations WHERE released_at_ms IS NULL",
        )?;
        let allocated_units = count(
            connection,
            "SELECT COALESCE(SUM(units), 0)
             FROM resource_allocations WHERE released_at_ms IS NULL",
        )?;
        let expired_allocations = connection.query_row(
            "SELECT COUNT(*) FROM resource_allocations
             WHERE released_at_ms IS NULL AND expires_at_ms < ?1",
            [now_ms],
            |row| row.get(0),
        )?;

        let spool_states = state_counts(
            connection,
            "SELECT state, COUNT(*) FROM spool_volumes GROUP BY state ORDER BY state",
        )?;
        let active_reservations = count(
            connection,
            "SELECT COUNT(*) FROM spool_reservations WHERE released_at_ms IS NULL",
        )?;
        let reserved_bytes = count(
            connection,
            "SELECT COALESCE(SUM(reserved_bytes), 0)
             FROM spool_reservations WHERE released_at_ms IS NULL",
        )?;
        let reported_free_bytes: Option<i64> = connection.query_row(
            "SELECT SUM(last_free_bytes) FROM spool_volumes
             WHERE state <> 'offline' AND last_free_bytes IS NOT NULL",
            [],
            |row| row.get(0),
        )?;

        let tools = ToolHealth {
            trusted: count(
                connection,
                "SELECT COUNT(*) FROM native_tool_inventory
                 WHERE deleted_at_ms IS NULL AND trust_status = 'trusted'",
            )?,
            blocked: count(
                connection,
                "SELECT COUNT(*) FROM native_tool_inventory
                 WHERE deleted_at_ms IS NULL AND trust_status = 'blocked'",
            )?,
            unreviewed: count(
                connection,
                "SELECT COUNT(*) FROM native_tool_inventory
                 WHERE deleted_at_ms IS NULL AND trust_status = 'unreviewed'",
            )?,
            invalid_signatures: count(
                connection,
                "SELECT COUNT(*) FROM native_tool_inventory
                 WHERE deleted_at_ms IS NULL AND signature_status = 'invalid'",
            )?,
        };

        let destinations = DestinationHealth {
            active: count(
                connection,
                "SELECT COUNT(*) FROM destinations WHERE deleted_at_ms IS NULL",
            )?,
            ready: count(
                connection,
                "SELECT COUNT(*) FROM destinations
                 WHERE deleted_at_ms IS NULL AND state = 'ready'",
            )?,
            attention: count(
                connection,
                "SELECT COUNT(*) FROM destinations
                 WHERE deleted_at_ms IS NULL AND state = 'attention'",
            )?,
            failed_last_probe: count(
                connection,
                "SELECT COUNT(*) FROM destinations
                 WHERE deleted_at_ms IS NULL AND last_probe_status = 'failed'",
            )?,
        };

        let notifications = NotificationHealth {
            pending: count(
                connection,
                "SELECT COUNT(*) FROM notification_outbox WHERE state = 'pending'",
            )?,
            delivering: count(
                connection,
                "SELECT COUNT(*) FROM notification_outbox WHERE state = 'delivering'",
            )?,
            stale_deliveries: connection.query_row(
                "SELECT COUNT(*) FROM notification_outbox
                 WHERE state = 'delivering' AND lease_expires_at_ms < ?1",
                [now_ms],
                |row| row.get(0),
            )?,
            failed_terminal: count(
                connection,
                "SELECT COUNT(*) FROM notification_outbox WHERE state = 'failed_terminal'",
            )?,
        };

        let attention = AttentionHealth {
            open: count(
                connection,
                "SELECT COUNT(*) FROM attention_states
                 WHERE state IN ('open', 'acknowledged')",
            )?,
            critical: count(
                connection,
                "SELECT COUNT(*) FROM attention_states
                 WHERE state IN ('open', 'acknowledged') AND severity = 'critical'",
            )?,
        };

        let (retained, earliest_at_ms, latest_at_ms): (i64, Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT COUNT(*), MIN(occurred_at_ms), MAX(occurred_at_ms)
                 FROM activity_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let recent_activity = recent_activity(
            connection,
            now_ms.saturating_sub(ACTIVITY_WINDOW_MS),
            MAX_ACTIVITY_EVENTS,
        )?;
        let (archive_count, latest_archive_at_ms): (i64, Option<i64>) = connection.query_row(
            "SELECT COUNT(*), MAX(updated_at_ms)
             FROM app_meta
             WHERE key GLOB 'telemetry_archive.v1.*'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let latest_migration = connection
            .query_row(
                "SELECT version, checksum_blake3, applied_at_ms, app_version
                 FROM schema_migrations
                 ORDER BY version DESC
                 LIMIT 1",
                [],
                |row| {
                    Ok(MigrationHealth {
                        version: row.get(0)?,
                        checksum_blake3: row.get(1)?,
                        applied_at_ms: row.get(2)?,
                        app_version: row.get(3)?,
                    })
                },
            )
            .optional()?;

        Ok(ServiceDiagnostics {
            schema_version: 1,
            generated_at_ms: now_ms,
            schema: SchemaHealth {
                user_version: writer.user_version,
                application_table_count: writer.application_table_count,
                journal_mode: writer.journal_mode.to_ascii_lowercase(),
                synchronous: writer.synchronous,
                foreign_keys: writer.foreign_keys == 1,
                auto_vacuum: writer.auto_vacuum,
                busy_timeout_ms: writer.busy_timeout_ms,
                latest_migration,
            },
            queue: QueueHealth {
                total: queue_total,
                by_state: queue_states,
            },
            leases: LeaseHealth {
                active: active_leases,
                stale: stale_leases,
            },
            permits: PermitHealth {
                enabled_capacity,
                active_allocations,
                allocated_units,
                expired_allocations,
            },
            spool: SpoolHealth {
                volumes_by_state: spool_states,
                active_reservations,
                reserved_bytes,
                reported_free_bytes,
            },
            tools,
            destinations,
            notifications,
            attention,
            activity: ActivityHealth {
                retained,
                earliest_at_ms,
                latest_at_ms,
                archive_count,
                latest_archive_at_ms,
                recent: recent_activity,
            },
        })
    }
}

fn state_counts(connection: &Connection, sql: &str) -> Result<Vec<StateCount>, RepositoryError> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| {
        Ok(StateCount {
            state: row.get(0)?,
            count: row.get(1)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

fn count(connection: &Connection, sql: &str) -> Result<i64, RepositoryError> {
    connection
        .query_row(sql, [], |row| row.get(0))
        .map_err(Into::into)
}

fn sum_counts(counts: &[StateCount]) -> i64 {
    counts
        .iter()
        .fold(0_i64, |total, item| total.saturating_add(item.count))
}

fn recent_activity(
    connection: &Connection,
    since_ms: i64,
    limit: i64,
) -> Result<Vec<SupportActivityEvent>, RepositoryError> {
    let mut statement = connection.prepare(
        "SELECT occurred_at_ms, severity, event_code, entity_type, entity_id, redaction_version
         FROM activity_events
         WHERE occurred_at_ms >= ?1
         ORDER BY occurred_at_ms DESC, seq DESC
         LIMIT ?2",
    )?;
    let rows = statement.query_map([since_ms, limit], |row| {
        Ok(SupportActivityEvent {
            occurred_at_ms: row.get(0)?,
            severity: row.get(1)?,
            event_code: row.get(2)?,
            entity_type: row.get(3)?,
            entity_id: row.get(4)?,
            redaction_version: row.get(5)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StateCount {
    state: String,
    count: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MigrationHealth {
    version: i64,
    checksum_blake3: String,
    applied_at_ms: i64,
    app_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaHealth {
    user_version: i64,
    application_table_count: i64,
    journal_mode: String,
    synchronous: i64,
    foreign_keys: bool,
    auto_vacuum: i64,
    busy_timeout_ms: i64,
    latest_migration: Option<MigrationHealth>,
}

impl SchemaHealth {
    pub const fn user_version(&self) -> i64 {
        self.user_version
    }

    pub const fn application_table_count(&self) -> i64 {
        self.application_table_count
    }

    pub fn journal_mode(&self) -> &str {
        &self.journal_mode
    }

    pub const fn foreign_keys(&self) -> bool {
        self.foreign_keys
    }

    pub const fn auto_vacuum(&self) -> i64 {
        self.auto_vacuum
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueHealth {
    total: i64,
    by_state: Vec<StateCount>,
}

impl QueueHealth {
    pub const fn total(&self) -> i64 {
        self.total
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseHealth {
    active: i64,
    stale: i64,
}

impl LeaseHealth {
    pub const fn active(&self) -> i64 {
        self.active
    }

    pub const fn stale(&self) -> i64 {
        self.stale
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermitHealth {
    enabled_capacity: i64,
    active_allocations: i64,
    allocated_units: i64,
    expired_allocations: i64,
}

impl PermitHealth {
    pub const fn active_allocations(&self) -> i64 {
        self.active_allocations
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpoolHealth {
    volumes_by_state: Vec<StateCount>,
    active_reservations: i64,
    reserved_bytes: i64,
    reported_free_bytes: Option<i64>,
}

impl SpoolHealth {
    pub const fn active_reservations(&self) -> i64 {
        self.active_reservations
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolHealth {
    trusted: i64,
    blocked: i64,
    unreviewed: i64,
    invalid_signatures: i64,
}

impl ToolHealth {
    pub const fn trusted(&self) -> i64 {
        self.trusted
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DestinationHealth {
    active: i64,
    ready: i64,
    attention: i64,
    failed_last_probe: i64,
}

impl DestinationHealth {
    pub const fn active(&self) -> i64 {
        self.active
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationHealth {
    pending: i64,
    delivering: i64,
    stale_deliveries: i64,
    failed_terminal: i64,
}

impl NotificationHealth {
    pub const fn pending(&self) -> i64 {
        self.pending
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttentionHealth {
    open: i64,
    critical: i64,
}

impl AttentionHealth {
    pub const fn open(&self) -> i64 {
        self.open
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportActivityEvent {
    occurred_at_ms: i64,
    severity: String,
    event_code: String,
    entity_type: String,
    entity_id: String,
    redaction_version: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityHealth {
    retained: i64,
    earliest_at_ms: Option<i64>,
    latest_at_ms: Option<i64>,
    archive_count: i64,
    latest_archive_at_ms: Option<i64>,
    recent: Vec<SupportActivityEvent>,
}

impl ActivityHealth {
    pub const fn retained(&self) -> i64 {
        self.retained
    }

    pub const fn archive_count(&self) -> i64 {
        self.archive_count
    }

    pub const fn latest_archive_at_ms(&self) -> Option<i64> {
        self.latest_archive_at_ms
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDiagnostics {
    schema_version: u32,
    generated_at_ms: i64,
    schema: SchemaHealth,
    queue: QueueHealth,
    leases: LeaseHealth,
    permits: PermitHealth,
    spool: SpoolHealth,
    tools: ToolHealth,
    destinations: DestinationHealth,
    notifications: NotificationHealth,
    attention: AttentionHealth,
    activity: ActivityHealth,
}

impl ServiceDiagnostics {
    pub const fn generated_at_ms(&self) -> i64 {
        self.generated_at_ms
    }

    pub const fn schema(&self) -> &SchemaHealth {
        &self.schema
    }

    pub const fn queue(&self) -> &QueueHealth {
        &self.queue
    }

    pub const fn leases(&self) -> &LeaseHealth {
        &self.leases
    }

    pub const fn permits(&self) -> &PermitHealth {
        &self.permits
    }

    pub const fn spool(&self) -> &SpoolHealth {
        &self.spool
    }

    pub const fn tools(&self) -> &ToolHealth {
        &self.tools
    }

    pub const fn destinations(&self) -> &DestinationHealth {
        &self.destinations
    }

    pub const fn notifications(&self) -> &NotificationHealth {
        &self.notifications
    }

    pub const fn attention(&self) -> &AttentionHealth {
        &self.attention
    }

    pub const fn activity(&self) -> &ActivityHealth {
        &self.activity
    }
}
