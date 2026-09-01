use restoreplane_catalog::{
    apply_operational_store_v2, apply_operational_store_v3, OperationalStore, DEFAULT_WORKSPACE_ID,
};
use rusqlite::{params, Connection};
use tempfile::TempDir;

#[test]
fn operational_store_bootstraps_workspace_schema_v3() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path.clone()).expect("open store");
    let diagnostics = store.writer_diagnostics().expect("diagnostics");

    assert_eq!(diagnostics.user_version, 4);
    assert_eq!(diagnostics.application_table_count, 67);

    let reader = Connection::open(db_path).expect("reader");
    let default_workspace: (String, String, i64) = reader
        .query_row(
            "SELECT id, display_name, is_default FROM workspaces WHERE id = ?1",
            [DEFAULT_WORKSPACE_ID],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("default workspace");
    assert_eq!(
        default_workspace,
        (
            DEFAULT_WORKSPACE_ID.to_owned(),
            "Default Workspace".to_owned(),
            1
        )
    );
}

#[test]
fn workspace_migration_backfills_v2_roots_and_automatic_children() {
    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    apply_operational_store_v2(&connection).expect("schema v2");
    seed_v2_roots(&connection);

    let outcome = apply_operational_store_v3(&connection).expect("schema v3");
    assert!(outcome.applied);
    assert_eq!(outcome.version, 3);

    for (table, resource_column) in [
        ("workspace_connection_profiles", "connection_profile_id"),
        ("workspace_destinations", "destination_id"),
        ("workspace_backup_plans", "backup_plan_id"),
    ] {
        let sql = format!(
            "SELECT COUNT(*) FROM {table} WHERE workspace_id = ?1 AND {resource_column} IS NOT NULL"
        );
        let count: i64 = connection
            .query_row(&sql, [DEFAULT_WORKSPACE_ID], |row| row.get(0))
            .expect("backfilled ownership");
        assert_eq!(count, 1, "{table} was not backfilled");
    }

    connection
        .execute(
            "INSERT INTO connection_profiles(
                 id, display_name, engine, transport, host, port, file_path,
                 tls_mode, adapter_config_json, state, source_resource_key,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 'connection-after-v3', 'After v3', 'sqlite', 'file', NULL, NULL,
                 'D:/data/after-v3.db', 'disabled', '{}', 'draft', 'after-v3', 20, 20
             )",
            [],
        )
        .expect("insert connection after v3");
    let inherited_workspace: String = connection
        .query_row(
            "SELECT workspace_id
             FROM workspace_connection_profiles
             WHERE connection_profile_id = 'connection-after-v3'",
            [],
            |row| row.get(0),
        )
        .expect("automatic default ownership");
    assert_eq!(inherited_workspace, DEFAULT_WORKSPACE_ID);
}

#[test]
fn workspace_schema_rejects_cross_workspace_plan_membership() {
    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    apply_operational_store_v2(&connection).expect("schema v2");
    apply_operational_store_v3(&connection).expect("schema v3");

    connection
        .execute(
            "INSERT INTO workspaces(
                 id, display_name, normalized_name, state, is_default, created_at_ms, updated_at_ms
             ) VALUES (?1, 'Analytics', 'analytics', 'active', 0, 10, 10)",
            ["11111111-1111-4111-8111-111111111111"],
        )
        .expect("workspace");
    connection
        .execute_batch(
            "INSERT INTO connection_profiles(
                 id, display_name, engine, transport, host, port, file_path,
                 tls_mode, adapter_config_json, state, source_resource_key,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 'connection-analytics', 'Analytics SQLite', 'sqlite', 'file', NULL, NULL,
                 'D:/data/analytics.db', 'disabled', '{}', 'draft', 'analytics', 10, 10
             );
             INSERT INTO database_targets(
                 id, connection_profile_id, engine_object_id, display_name, normalized_name,
                 source_fingerprint, state, selected_at_ms, created_at_ms, updated_at_ms
             ) VALUES (
                 'target-analytics', 'connection-analytics', 'analytics', 'analytics', 'analytics',
                 'fingerprint-analytics', 'active', 10, 10, 10
             );
             INSERT INTO backup_plans(id, display_name, state, created_at_ms, updated_at_ms)
             VALUES ('plan-default', 'Default plan', 'draft', 10, 10);
             INSERT INTO backup_plan_revisions(
                 id, backup_plan_id, revision_no, schedule_kind, schedule_expression, timezone,
                 catchup_policy, overlap_policy, retention_policy_json,
                 verification_policy_json, execution_policy_json, content_hash_blake3,
                 created_at_ms
             ) VALUES (
                 'revision-default', 'plan-default', 1, 'manual', NULL, 'UTC',
                 'skip', 'coalesce_one', '{}', '{}', '{}',
                 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 10
             );",
        )
        .expect("plan fixtures");
    connection
        .execute(
            "UPDATE workspace_connection_profiles
             SET workspace_id = ?1
             WHERE connection_profile_id = 'connection-analytics'",
            ["11111111-1111-4111-8111-111111111111"],
        )
        .expect("move connection ownership");

    let error = connection
        .execute(
            "INSERT INTO plan_revision_targets(
                 plan_revision_id, database_target_id, target_options_json, created_at_ms
             ) VALUES ('revision-default', 'target-analytics', '{}', 10)",
            [],
        )
        .expect_err("cross-workspace target must be rejected");
    assert!(error.to_string().contains("WORKSPACE_SCOPE_MISMATCH"));
}

fn seed_v2_roots(connection: &Connection) {
    connection
        .execute(
            "INSERT INTO connection_profiles(
                 id, display_name, engine, transport, host, port, file_path,
                 tls_mode, adapter_config_json, state, source_resource_key,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 'connection-existing', 'Existing SQLite', 'sqlite', 'file', NULL, NULL,
                 'D:/data/existing.db', 'disabled', '{}', 'draft', 'existing', 10, 10
             )",
            [],
        )
        .expect("connection");
    connection
        .execute(
            "INSERT INTO destinations(
                 id, display_name, kind, endpoint_json, state,
                 declared_capabilities_json, observed_capabilities_json,
                 immutability_capability, created_at_ms, updated_at_ms
             ) VALUES (
                 'destination-existing', 'Existing Local', 'local', '{\"path\":\"D:/backup\"}',
                 'draft', '{}', '{}', 'none', 10, 10
             )",
            [],
        )
        .expect("destination");
    connection
        .execute(
            "INSERT INTO backup_plans(id, display_name, state, created_at_ms, updated_at_ms)
             VALUES (?1, 'Existing plan', 'draft', 10, 10)",
            params!["plan-existing"],
        )
        .expect("plan");
}
