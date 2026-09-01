PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;

-- Backup Scope v1 separates a selected database from the exact object set that
-- a Plan revision promises to protect. One revision may bind at most one Scope
-- for a Database Target; the Scope can be reused by later revisions while every
-- revision and run keeps an immutable selector snapshot.
CREATE TABLE backup_scopes (
    id TEXT PRIMARY KEY,
    database_target_id TEXT NOT NULL REFERENCES database_targets(id) ON DELETE RESTRICT,
    display_name TEXT NOT NULL CHECK(length(trim(display_name)) BETWEEN 1 AND 96),
    normalized_name TEXT NOT NULL CHECK(length(normalized_name) BETWEEN 1 AND 96),
    mode TEXT NOT NULL CHECK(mode IN ('whole_database', 'explicit_tables')),
    state TEXT NOT NULL CHECK(state IN ('active', 'attention', 'disabled', 'retired')),
    selector_json TEXT NOT NULL CHECK(json_valid(selector_json)),
    selector_hash_blake3 TEXT NOT NULL CHECK(length(selector_hash_blake3) = 64),
    source_fingerprint TEXT NOT NULL CHECK(length(trim(source_fingerprint)) > 0),
    database_source_fingerprint TEXT NOT NULL CHECK(length(trim(database_source_fingerprint)) > 0),
    resolved_object_count INTEGER NOT NULL CHECK(resolved_object_count >= 0),
    last_resolved_at_ms INTEGER CHECK(last_resolved_at_ms IS NULL OR last_resolved_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    UNIQUE(id, database_target_id),
    CHECK(json_extract(selector_json, '$.mode') = mode),
    CHECK(json_type(selector_json, '$.objects') = 'array'),
    CHECK(
        (mode = 'whole_database'
         AND json_array_length(json_extract(selector_json, '$.objects')) = 0)
        OR
        (mode = 'explicit_tables'
         AND resolved_object_count > 0
         AND json_array_length(json_extract(selector_json, '$.objects')) = resolved_object_count)
    )
) STRICT;

CREATE INDEX idx_backup_scopes_target_state
    ON backup_scopes(database_target_id, state, normalized_name);

CREATE UNIQUE INDEX idx_backup_scopes_active_name
    ON backup_scopes(database_target_id, normalized_name)
    WHERE state <> 'retired';

CREATE UNIQUE INDEX idx_backup_scopes_one_whole_database
    ON backup_scopes(database_target_id)
    WHERE mode = 'whole_database';

CREATE TRIGGER trg_database_target_default_scope
AFTER INSERT ON database_targets
BEGIN
    INSERT INTO backup_scopes(
        id, database_target_id, display_name, normalized_name, mode, state,
        selector_json, selector_hash_blake3, source_fingerprint, database_source_fingerprint,
        resolved_object_count, last_resolved_at_ms, created_at_ms, updated_at_ms
    ) VALUES (
        NEW.id, NEW.id, 'Whole database', 'whole database', 'whole_database', 'active',
        '{"mode":"whole_database","objects":[]}',
        '6ce25010c90ed719a256a023e91191516ac5ece6007e7cd786a599f30b156ccb',
        NEW.source_fingerprint, NEW.source_fingerprint,
        0, NEW.selected_at_ms, NEW.created_at_ms, NEW.updated_at_ms
    );
END;

CREATE TRIGGER trg_backup_scope_selector_immutable
BEFORE UPDATE OF database_target_id, mode, selector_json, selector_hash_blake3,
                 source_fingerprint, resolved_object_count
                 , database_source_fingerprint
ON backup_scopes
BEGIN
    SELECT RAISE(ABORT, 'backup scope selector is immutable');
END;

CREATE TRIGGER trg_backup_scope_delete_immutable
BEFORE DELETE ON backup_scopes
BEGIN
    SELECT RAISE(ABORT, 'backup scope is append-only');
END;

CREATE TABLE backup_scope_objects (
    backup_scope_id TEXT NOT NULL REFERENCES backup_scopes(id) ON DELETE CASCADE,
    object_kind TEXT NOT NULL CHECK(object_kind = 'table'),
    schema_name TEXT NOT NULL CHECK(length(schema_name) <= 256),
    object_name TEXT NOT NULL CHECK(length(object_name) BETWEEN 1 AND 256),
    engine_object_id TEXT NOT NULL CHECK(length(trim(engine_object_id)) > 0),
    object_fingerprint_blake3 TEXT NOT NULL CHECK(length(object_fingerprint_blake3) = 64),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (backup_scope_id, schema_name, object_name),
    UNIQUE(backup_scope_id, ordinal),
    UNIQUE(backup_scope_id, engine_object_id)
) STRICT;

CREATE INDEX idx_backup_scope_objects_identity
    ON backup_scope_objects(backup_scope_id, object_kind, schema_name, object_name);

CREATE TRIGGER trg_backup_scope_object_insert_mode
BEFORE INSERT ON backup_scope_objects
WHEN (SELECT mode FROM backup_scopes WHERE id = NEW.backup_scope_id) <> 'explicit_tables'
BEGIN
    SELECT RAISE(ABORT, 'SCOPE_MODE_MISMATCH');
END;

CREATE TRIGGER trg_backup_scope_object_update_mode
BEFORE UPDATE ON backup_scope_objects
WHEN (SELECT mode FROM backup_scopes WHERE id = NEW.backup_scope_id) <> 'explicit_tables'
BEGIN
    SELECT RAISE(ABORT, 'SCOPE_MODE_MISMATCH');
END;

CREATE TRIGGER trg_backup_scope_object_update_immutable
BEFORE UPDATE ON backup_scope_objects
BEGIN
    SELECT RAISE(ABORT, 'backup scope object is immutable');
END;

CREATE TRIGGER trg_backup_scope_object_delete_immutable
BEFORE DELETE ON backup_scope_objects
BEGIN
    SELECT RAISE(ABORT, 'backup scope object is immutable');
END;

CREATE TABLE plan_revision_scopes (
    plan_revision_id TEXT NOT NULL,
    database_target_id TEXT NOT NULL,
    backup_scope_id TEXT NOT NULL,
    scope_mode TEXT NOT NULL CHECK(scope_mode IN ('whole_database', 'explicit_tables')),
    source_fingerprint TEXT NOT NULL CHECK(length(trim(source_fingerprint)) > 0),
    database_source_fingerprint TEXT NOT NULL CHECK(length(trim(database_source_fingerprint)) > 0),
    resolved_object_count INTEGER NOT NULL CHECK(resolved_object_count >= 0),
    scope_snapshot_json TEXT NOT NULL CHECK(json_valid(scope_snapshot_json)),
    scope_snapshot_hash_blake3 TEXT NOT NULL CHECK(length(scope_snapshot_hash_blake3) = 64),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (plan_revision_id, database_target_id),
    UNIQUE(plan_revision_id, backup_scope_id),
    FOREIGN KEY (plan_revision_id, database_target_id)
        REFERENCES plan_revision_targets(plan_revision_id, database_target_id) ON DELETE CASCADE,
    FOREIGN KEY (backup_scope_id, database_target_id)
        REFERENCES backup_scopes(id, database_target_id) ON DELETE RESTRICT,
    CHECK(json_type(scope_snapshot_json, '$.objects') = 'array'),
    CHECK(json_extract(scope_snapshot_json, '$.mode') = scope_mode),
    CHECK(json_array_length(json_extract(scope_snapshot_json, '$.objects')) = resolved_object_count)
) STRICT;

CREATE INDEX idx_plan_revision_scopes_scope
    ON plan_revision_scopes(backup_scope_id, plan_revision_id);

CREATE TRIGGER trg_plan_scope_insert_published
BEFORE INSERT ON plan_revision_scopes
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = NEW.plan_revision_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan scope is immutable');
END;

CREATE TRIGGER trg_plan_scope_update_published
BEFORE UPDATE ON plan_revision_scopes
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = OLD.plan_revision_id) IS NOT NULL
  OR EXISTS (
      SELECT 1 FROM backup_runs
      WHERE plan_revision_id = OLD.plan_revision_id
  )
BEGIN
    SELECT RAISE(ABORT, 'published plan scope is immutable');
END;

CREATE TRIGGER trg_plan_scope_delete_published
BEFORE DELETE ON plan_revision_scopes
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = OLD.plan_revision_id) IS NOT NULL
  OR EXISTS (
      SELECT 1 FROM backup_runs
      WHERE plan_revision_id = OLD.plan_revision_id
  )
BEGIN
    SELECT RAISE(ABORT, 'published plan scope is immutable');
END;

CREATE TRIGGER trg_plan_target_default_scope
AFTER INSERT ON plan_revision_targets
WHEN NOT EXISTS (
    SELECT 1 FROM plan_revision_scopes
    WHERE plan_revision_id = NEW.plan_revision_id
      AND database_target_id = NEW.database_target_id
)
BEGIN
    INSERT INTO plan_revision_scopes(
        plan_revision_id, database_target_id, backup_scope_id,
        scope_mode, source_fingerprint, database_source_fingerprint, resolved_object_count,
        scope_snapshot_json, scope_snapshot_hash_blake3, created_at_ms
    )
    SELECT NEW.plan_revision_id, NEW.database_target_id, scope.id,
           scope.mode, scope.source_fingerprint, scope.database_source_fingerprint,
           scope.resolved_object_count,
           scope.selector_json, scope.selector_hash_blake3, NEW.created_at_ms
    FROM backup_scopes AS scope
    WHERE scope.id = NEW.database_target_id
      AND scope.database_target_id = NEW.database_target_id;
END;

CREATE TRIGGER trg_plan_revision_publish_requires_scopes
BEFORE UPDATE OF published_at_ms ON backup_plan_revisions
WHEN OLD.published_at_ms IS NULL AND NEW.published_at_ms IS NOT NULL
BEGIN
    SELECT CASE WHEN EXISTS (
        SELECT 1
        FROM plan_revision_targets AS target
        LEFT JOIN plan_revision_scopes AS scope
          ON scope.plan_revision_id = target.plan_revision_id
         AND scope.database_target_id = target.database_target_id
        WHERE target.plan_revision_id = NEW.id
          AND scope.backup_scope_id IS NULL
    ) THEN RAISE(ABORT, 'plan revision requires one scope per target') END;
    SELECT CASE WHEN EXISTS (
        SELECT 1
        FROM plan_revision_scopes AS revision_scope
        JOIN backup_scopes AS scope ON scope.id = revision_scope.backup_scope_id
        WHERE revision_scope.plan_revision_id = NEW.id
          AND (
              revision_scope.scope_snapshot_hash_blake3 <> scope.selector_hash_blake3
              OR revision_scope.scope_snapshot_json <> scope.selector_json
              OR revision_scope.scope_mode <> scope.mode
              OR revision_scope.source_fingerprint <> scope.source_fingerprint
              OR revision_scope.database_source_fingerprint <> scope.database_source_fingerprint
              OR revision_scope.resolved_object_count <> scope.resolved_object_count
              OR (
                  scope.mode = 'explicit_tables'
                  AND scope.resolved_object_count <> (
                      SELECT COUNT(*)
                      FROM backup_scope_objects AS object
                      WHERE object.backup_scope_id = scope.id
                  )
              )
          )
    ) THEN RAISE(ABORT, 'plan revision scope snapshot is stale') END;
END;

CREATE UNIQUE INDEX idx_backup_runs_id_target
    ON backup_runs(id, database_target_id);

CREATE TABLE backup_run_scopes (
    backup_run_id TEXT PRIMARY KEY,
    database_target_id TEXT NOT NULL,
    backup_scope_id TEXT NOT NULL,
    scope_mode TEXT NOT NULL CHECK(scope_mode IN ('whole_database', 'explicit_tables')),
    source_fingerprint TEXT NOT NULL CHECK(length(trim(source_fingerprint)) > 0),
    database_source_fingerprint TEXT NOT NULL CHECK(length(trim(database_source_fingerprint)) > 0),
    resolved_object_count INTEGER NOT NULL CHECK(resolved_object_count >= 0),
    scope_snapshot_json TEXT NOT NULL CHECK(json_valid(scope_snapshot_json)),
    scope_snapshot_hash_blake3 TEXT NOT NULL CHECK(length(scope_snapshot_hash_blake3) = 64),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    FOREIGN KEY (backup_run_id, database_target_id)
        REFERENCES backup_runs(id, database_target_id) ON DELETE RESTRICT,
    FOREIGN KEY (backup_scope_id, database_target_id)
        REFERENCES backup_scopes(id, database_target_id) ON DELETE RESTRICT,
    CHECK(json_type(scope_snapshot_json, '$.objects') = 'array'),
    CHECK(json_extract(scope_snapshot_json, '$.mode') = scope_mode),
    CHECK(json_array_length(json_extract(scope_snapshot_json, '$.objects')) = resolved_object_count)
) STRICT;

CREATE INDEX idx_backup_run_scopes_scope_time
    ON backup_run_scopes(backup_scope_id, created_at_ms DESC);

CREATE TRIGGER trg_backup_run_requires_current_scope
BEFORE INSERT ON backup_runs
WHEN (
    SELECT COUNT(*)
    FROM plan_revision_scopes AS scope
    WHERE scope.plan_revision_id = NEW.plan_revision_id
      AND scope.database_target_id = NEW.database_target_id
      AND scope.source_fingerprint = NEW.source_fingerprint
) <> 1
BEGIN
    SELECT RAISE(ABORT, 'BACKUP_SCOPE_STALE_OR_MISSING');
END;

CREATE TRIGGER trg_backup_run_scope_snapshot
AFTER INSERT ON backup_runs
BEGIN
    INSERT INTO backup_run_scopes(
        backup_run_id, database_target_id, backup_scope_id,
        scope_mode, source_fingerprint, database_source_fingerprint, resolved_object_count,
        scope_snapshot_json, scope_snapshot_hash_blake3, created_at_ms
    )
    SELECT NEW.id, NEW.database_target_id, scope.backup_scope_id,
           scope.scope_mode, scope.source_fingerprint, scope.database_source_fingerprint,
           scope.resolved_object_count,
           scope.scope_snapshot_json, scope.scope_snapshot_hash_blake3, NEW.created_at_ms
    FROM plan_revision_scopes AS scope
    WHERE scope.plan_revision_id = NEW.plan_revision_id
      AND scope.database_target_id = NEW.database_target_id;
END;

CREATE TRIGGER trg_backup_run_scope_update
BEFORE UPDATE ON backup_run_scopes
BEGIN
    SELECT RAISE(ABORT, 'backup run scope is immutable');
END;

CREATE TRIGGER trg_backup_run_scope_delete
BEFORE DELETE ON backup_run_scopes
BEGIN
    SELECT RAISE(ABORT, 'backup run scope is immutable');
END;

-- The scheduler cursor is durable engine state, not an operating-system task.
CREATE TABLE scheduler_cursors (
    plan_revision_id TEXT PRIMARY KEY REFERENCES backup_plan_revisions(id) ON DELETE CASCADE,
    last_evaluated_at_ms INTEGER NOT NULL CHECK(last_evaluated_at_ms >= 0),
    last_materialized_for_ms INTEGER CHECK(
        last_materialized_for_ms IS NULL OR last_materialized_for_ms >= 0
    ),
    last_error_code TEXT,
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= last_evaluated_at_ms)
) STRICT;

CREATE INDEX idx_scheduler_cursors_updated
    ON scheduler_cursors(updated_at_ms, plan_revision_id);

-- Object inventory is paged independently from the operation result so a
-- database with thousands of tables cannot overflow the 1 MiB IPC envelope.
CREATE TABLE discovered_database_objects (
    operation_run_id TEXT NOT NULL REFERENCES operation_runs(id) ON DELETE CASCADE,
    database_target_id TEXT NOT NULL REFERENCES database_targets(id) ON DELETE CASCADE,
    engine_object_id TEXT NOT NULL,
    schema_name TEXT NOT NULL CHECK(length(schema_name) <= 256),
    object_name TEXT NOT NULL CHECK(length(object_name) BETWEEN 1 AND 256),
    object_type TEXT NOT NULL CHECK(object_type IN (
        'table', 'partitioned_table', 'view', 'materialized_view', 'foreign_table'
    )),
    eligibility TEXT NOT NULL CHECK(eligibility IN ('eligible', 'excluded', 'unsupported')),
    exclusion_reason TEXT,
    object_fingerprint_blake3 TEXT NOT NULL CHECK(length(object_fingerprint_blake3) = 64),
    ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
    observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms >= 0),
    PRIMARY KEY (operation_run_id, engine_object_id),
    UNIQUE(operation_run_id, ordinal)
) STRICT;

CREATE INDEX idx_discovered_database_objects_target_time
    ON discovered_database_objects(database_target_id, observed_at_ms DESC, ordinal);

PRAGMA user_version = 4;
COMMIT;
