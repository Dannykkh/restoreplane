use restoreplane_catalog::{
    apply_operational_store_v2, apply_operational_store_v3, apply_operational_store_v4,
    OperationalStore,
};
use rusqlite::Connection;
use tempfile::TempDir;

#[test]
fn operational_store_bootstraps_backup_scope_schema_v4() {
    let temp = TempDir::new().expect("temp directory");
    let db_path = temp.path().join("operational.db");
    let store = OperationalStore::open(db_path).expect("open store");
    let diagnostics = store.writer_diagnostics().expect("diagnostics");

    assert_eq!(diagnostics.user_version, 4);
    assert_eq!(diagnostics.application_table_count, 67);
}

#[test]
fn v4_creates_a_canonical_whole_scope_for_every_new_target() {
    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    apply_operational_store_v2(&connection).expect("schema v2");
    apply_operational_store_v3(&connection).expect("schema v3");
    apply_operational_store_v4(&connection).expect("schema v4");
    connection
        .execute_batch(
            "INSERT INTO connection_profiles(
                 id, display_name, engine, transport, host, port, file_path,
                 tls_mode, adapter_config_json, state, source_resource_key,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 'connection-new', 'New PostgreSQL', 'postgresql', 'tcp',
                 'localhost', 5432, NULL, 'preferred', '{}', 'ready',
                 'new-source', 100, 100
             );
             INSERT INTO database_targets(
                 id, connection_profile_id, engine_object_id, display_name,
                 normalized_name, source_fingerprint, state, selected_at_ms,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 'target-new', 'connection-new', 'app', 'app', 'app',
                 'fingerprint-new', 'active', 101, 101, 101
             );",
        )
        .expect("post-v4 target");

    let scope: (String, String, String, String, i64) = connection
        .query_row(
            "SELECT id, database_target_id, mode, source_fingerprint,
                    resolved_object_count
             FROM backup_scopes
             WHERE id = 'target-new'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("canonical post-v4 whole scope");
    assert_eq!(
        scope,
        (
            "target-new".to_owned(),
            "target-new".to_owned(),
            "whole_database".to_owned(),
            "fingerprint-new".to_owned(),
            0,
        )
    );
}

#[test]
fn v4_backfills_v3_targets_revisions_and_runs_as_whole_database_scopes() {
    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    apply_operational_store_v2(&connection).expect("schema v2");
    apply_operational_store_v3(&connection).expect("schema v3");
    seed_v3_backup(&connection, true);

    let outcome = apply_operational_store_v4(&connection).expect("schema v4");
    assert!(outcome.applied);
    assert_eq!(outcome.version, 4);

    let scope: (String, String, String, i64) = connection
        .query_row(
            "SELECT id, database_target_id, mode, resolved_object_count
             FROM backup_scopes WHERE id = 'target-v3'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("implicit whole database scope");
    assert_eq!(
        scope,
        (
            "target-v3".to_owned(),
            "target-v3".to_owned(),
            "whole_database".to_owned(),
            0,
        )
    );

    let revision_snapshot: (String, String, i64) = connection
        .query_row(
            "SELECT backup_scope_id,
                    json_extract(scope_snapshot_json, '$.mode'),
                    length(scope_snapshot_hash_blake3)
             FROM plan_revision_scopes WHERE plan_revision_id = 'revision-v3'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("revision scope snapshot");
    assert_eq!(
        revision_snapshot,
        ("target-v3".to_owned(), "whole_database".to_owned(), 64)
    );

    let run_snapshot: (String, String) = connection
        .query_row(
            "SELECT backup_scope_id, json_extract(scope_snapshot_json, '$.mode')
             FROM backup_run_scopes WHERE backup_run_id = 'backup-v3'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("run scope snapshot");
    assert_eq!(
        run_snapshot,
        ("target-v3".to_owned(), "whole_database".to_owned())
    );
}

#[test]
fn v4_enforces_exact_scope_membership_and_published_snapshot_immutability() {
    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("foreign keys");
    apply_operational_store_v2(&connection).expect("schema v2");
    apply_operational_store_v3(&connection).expect("schema v3");
    seed_v3_backup(&connection, false);
    apply_operational_store_v4(&connection).expect("schema v4");

    connection
        .execute(
            "DELETE FROM plan_revision_scopes WHERE plan_revision_id = 'revision-v3'",
            [],
        )
        .expect("replace implicit draft scope");
    connection
        .execute_batch(
            "INSERT INTO backup_scopes(
                 id, database_target_id, display_name, normalized_name, mode, state,
                 selector_json, selector_hash_blake3, source_fingerprint,
                 database_source_fingerprint, resolved_object_count,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 'scope-selected', 'target-v3', 'Selected tables', 'selected tables',
                 'explicit_tables', 'active',
                 '{\"mode\":\"explicit_tables\",\"objects\":[{\"kind\":\"table\",\"schema\":\"public\",\"name\":\"orders\"}]}',
                 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                 'scope-fingerprint-v3', 'fingerprint-v3', 1, 20, 20
             );
             INSERT INTO backup_scope_objects(
                 backup_scope_id, object_kind, schema_name, object_name, engine_object_id,
                 object_fingerprint_blake3, ordinal, created_at_ms
             ) VALUES (
                 'scope-selected', 'table', 'public', 'orders', 'public.orders',
                 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', 0, 20
             );",
        )
        .expect("explicit scope");

    let whole_object_error = connection
        .execute(
            "INSERT INTO backup_scope_objects(
                 backup_scope_id, object_kind, schema_name, object_name, engine_object_id,
                 object_fingerprint_blake3, ordinal, created_at_ms
             ) VALUES (
                 'target-v3', 'table', 'public', 'must-fail', 'public.must-fail',
                 'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd', 0, 20
             )",
            [],
        )
        .expect_err("whole database scopes cannot contain selected objects");
    assert!(whole_object_error
        .to_string()
        .contains("SCOPE_MODE_MISMATCH"));

    connection
        .execute(
            "INSERT INTO plan_revision_scopes(
                 plan_revision_id, database_target_id, backup_scope_id,
                 scope_mode, source_fingerprint, database_source_fingerprint,
                 resolved_object_count,
                 scope_snapshot_json, scope_snapshot_hash_blake3, created_at_ms
             ) VALUES (
                 'revision-v3', 'target-v3', 'scope-selected',
                 'explicit_tables', 'scope-fingerprint-v3', 'fingerprint-v3', 1,
                 (SELECT selector_json FROM backup_scopes WHERE id = 'scope-selected'),
                 (SELECT selector_hash_blake3 FROM backup_scopes WHERE id = 'scope-selected'), 20
             )",
            [],
        )
        .expect("draft revision scope");
    connection
        .execute(
            "UPDATE backup_plan_revisions SET published_at_ms = 30 WHERE id = 'revision-v3'",
            [],
        )
        .expect("publish revision");

    let immutable_error = connection
        .execute(
            "UPDATE plan_revision_scopes
             SET scope_snapshot_json = '{\"mode\":\"whole_database\"}'
             WHERE plan_revision_id = 'revision-v3'",
            [],
        )
        .expect_err("published scope snapshot is immutable");
    assert!(immutable_error
        .to_string()
        .contains("published plan scope is immutable"));
}

fn seed_v3_backup(connection: &Connection, with_backup_run: bool) {
    connection
        .execute_batch(
            "INSERT INTO connection_profiles(
                 id, display_name, engine, transport, host, port, file_path,
                 tls_mode, adapter_config_json, state, source_resource_key,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 'connection-v3', 'V3 PostgreSQL', 'postgresql', 'tcp', 'localhost', 5432,
                 NULL, 'preferred', '{}', 'ready', 'v3-source', 10, 10
             );
             INSERT INTO database_targets(
                 id, connection_profile_id, engine_object_id, display_name, normalized_name,
                 source_fingerprint, state, selected_at_ms, created_at_ms, updated_at_ms
             ) VALUES (
                 'target-v3', 'connection-v3', 'app', 'app', 'app',
                 'fingerprint-v3', 'active', 10, 10, 10
             );
             INSERT INTO destinations(
                 id, display_name, kind, endpoint_json, state,
                 declared_capabilities_json, observed_capabilities_json,
                 immutability_capability, created_at_ms, updated_at_ms
             ) VALUES (
                 'destination-v3', 'V3 Local', 'local', '{\"path\":\"D:/backup\"}',
                 'ready', '{}', '{}', 'none', 10, 10
             );
             INSERT INTO backup_plans(id, display_name, state, created_at_ms, updated_at_ms)
             VALUES ('plan-v3', 'V3 plan', 'enabled', 10, 10);
             INSERT INTO backup_plan_revisions(
                 id, backup_plan_id, revision_no, schedule_kind, schedule_expression, timezone,
                 catchup_policy, overlap_policy, retention_policy_json,
                 verification_policy_json, execution_policy_json, content_hash_blake3,
                 created_at_ms
             ) VALUES (
                 'revision-v3', 'plan-v3', 1, 'manual', NULL, 'UTC',
                 'skip', 'coalesce_one', '{}', '{}', '{}',
                 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 10
             );
             INSERT INTO plan_revision_targets(
                 plan_revision_id, database_target_id, target_options_json, created_at_ms
             ) VALUES ('revision-v3', 'target-v3', '{}', 10);
             INSERT INTO plan_revision_destinations(
                 plan_revision_id, destination_id, requirement, created_at_ms
             ) VALUES ('revision-v3', 'destination-v3', 'required', 10);",
        )
        .expect("v3 plan fixtures");
    if with_backup_run {
        connection
            .execute_batch(
                "INSERT INTO plan_occurrences(
                     id, plan_revision_id, origin, scheduled_for_ms, requested_at_ms,
                     request_idempotency_key, state, created_at_ms, updated_at_ms
                 ) VALUES (
                     'occurrence-v3', 'revision-v3', 'manual', NULL, 10,
                     'manual-v3', 'enqueued', 10, 10
                 );
                 INSERT INTO backup_runs(
                     id, plan_occurrence_id, plan_revision_id, database_target_id,
                     state, engine, source_fingerprint, created_at_ms, updated_at_ms
                 ) VALUES (
                     'backup-v3', 'occurrence-v3', 'revision-v3', 'target-v3',
                     'queued', 'postgresql', 'fingerprint-v3', 10, 10
                 );",
            )
            .expect("v3 backup fixture");
    }
}
