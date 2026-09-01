PRAGMA foreign_keys = ON;
BEGIN IMMEDIATE;

-- Workspace v1 is a local operational ownership boundary. Global service
-- health, tool inventory, certifications, resource pools, and spool volumes
-- remain outside this boundary.
CREATE TABLE workspaces (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL CHECK(length(trim(display_name)) BETWEEN 1 AND 96),
    normalized_name TEXT NOT NULL UNIQUE CHECK(length(normalized_name) BETWEEN 1 AND 96),
    state TEXT NOT NULL CHECK(state IN ('active', 'archived')),
    is_default INTEGER NOT NULL CHECK(is_default IN (0, 1)),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE UNIQUE INDEX idx_workspaces_single_default
    ON workspaces(is_default)
    WHERE is_default = 1;

CREATE INDEX idx_workspaces_state_name
    ON workspaces(state, normalized_name);

CREATE TABLE workspace_connection_profiles (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    connection_profile_id TEXT NOT NULL UNIQUE REFERENCES connection_profiles(id) ON DELETE CASCADE,
    display_name_key TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (workspace_id, connection_profile_id),
    UNIQUE (workspace_id, display_name_key)
) STRICT;

CREATE INDEX idx_workspace_connections_workspace
    ON workspace_connection_profiles(workspace_id, connection_profile_id);

CREATE TABLE workspace_destinations (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    destination_id TEXT NOT NULL UNIQUE REFERENCES destinations(id) ON DELETE CASCADE,
    display_name_key TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (workspace_id, destination_id),
    UNIQUE (workspace_id, display_name_key)
) STRICT;

CREATE INDEX idx_workspace_destinations_workspace
    ON workspace_destinations(workspace_id, destination_id);

CREATE TABLE workspace_backup_plans (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    backup_plan_id TEXT NOT NULL UNIQUE REFERENCES backup_plans(id) ON DELETE CASCADE,
    display_name_key TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (workspace_id, backup_plan_id),
    UNIQUE (workspace_id, display_name_key)
) STRICT;

CREATE INDEX idx_workspace_plans_workspace
    ON workspace_backup_plans(workspace_id, backup_plan_id);

CREATE TABLE workspace_recovery_points (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    recovery_point_id TEXT NOT NULL UNIQUE REFERENCES catalog_recovery_points(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (workspace_id, recovery_point_id)
) STRICT;

CREATE INDEX idx_workspace_recovery_points_workspace
    ON workspace_recovery_points(workspace_id, recovery_point_id);

CREATE TABLE workspace_work_items (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    work_item_id TEXT NOT NULL UNIQUE REFERENCES work_items(id) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (workspace_id, work_item_id)
) STRICT;

CREATE INDEX idx_workspace_work_items_workspace
    ON workspace_work_items(workspace_id, work_item_id);

CREATE TABLE workspace_activity_events (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE RESTRICT,
    activity_event_seq INTEGER NOT NULL UNIQUE REFERENCES activity_events(seq) ON DELETE CASCADE,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (workspace_id, activity_event_seq)
) STRICT;

CREATE INDEX idx_workspace_activity_events_feed
    ON workspace_activity_events(workspace_id, activity_event_seq DESC);

CREATE TABLE workspace_dispatch_state (
    workspace_id TEXT PRIMARY KEY REFERENCES workspaces(id) ON DELETE CASCADE,
    last_claimed_at_ms INTEGER NOT NULL DEFAULT 0 CHECK(last_claimed_at_ms >= 0),
    claim_count INTEGER NOT NULL DEFAULT 0 CHECK(claim_count >= 0)
) STRICT;

-- Names are unique inside a Workspace, not across the whole installation.
DROP INDEX idx_connection_profiles_active_name;
DROP INDEX idx_destinations_active_name;
DROP INDEX idx_backup_plans_active_name;

CREATE TRIGGER trg_workspace_dispatch_state_insert
AFTER INSERT ON workspaces
BEGIN
    INSERT INTO workspace_dispatch_state(workspace_id, last_claimed_at_ms, claim_count)
    VALUES (NEW.id, 0, 0);
END;

INSERT INTO workspaces(
    id, display_name, normalized_name, state, is_default, created_at_ms, updated_at_ms
) VALUES (
    '00000000-0000-4000-8000-000000000001',
    'Default Workspace',
    'default workspace',
    'active',
    1,
    0,
    0
);

INSERT INTO workspace_connection_profiles(
    workspace_id, connection_profile_id, display_name_key, created_at_ms
)
SELECT
    '00000000-0000-4000-8000-000000000001',
    id,
    lower(trim(display_name)),
    created_at_ms
FROM connection_profiles;

INSERT INTO workspace_destinations(
    workspace_id, destination_id, display_name_key, created_at_ms
)
SELECT
    '00000000-0000-4000-8000-000000000001',
    id,
    lower(trim(display_name)),
    created_at_ms
FROM destinations;

INSERT INTO workspace_backup_plans(
    workspace_id, backup_plan_id, display_name_key, created_at_ms
)
SELECT
    '00000000-0000-4000-8000-000000000001',
    id,
    lower(trim(display_name)),
    created_at_ms
FROM backup_plans;

INSERT INTO workspace_recovery_points(workspace_id, recovery_point_id, created_at_ms)
SELECT
    COALESCE(
        (
            SELECT ownership.workspace_id
            FROM backup_runs AS run
            JOIN backup_plan_revisions AS revision ON revision.id = run.plan_revision_id
            JOIN workspace_backup_plans AS ownership
              ON ownership.backup_plan_id = revision.backup_plan_id
            WHERE run.id = recovery.origin_backup_run_id
        ),
        '00000000-0000-4000-8000-000000000001'
    ),
    recovery.id,
    recovery.first_ingested_at_ms
FROM catalog_recovery_points AS recovery;

INSERT INTO workspace_work_items(workspace_id, work_item_id, created_at_ms)
SELECT
    COALESCE(
        CASE WHEN work.kind = 'backup' THEN (
            SELECT ownership.workspace_id
            FROM backup_runs AS run
            JOIN backup_plan_revisions AS revision ON revision.id = run.plan_revision_id
            JOIN workspace_backup_plans AS ownership
              ON ownership.backup_plan_id = revision.backup_plan_id
            WHERE run.id = work.backup_run_id
        ) END,
        CASE WHEN work.kind = 'restore' THEN (
            SELECT ownership.workspace_id
            FROM restore_runs AS run
            JOIN workspace_recovery_points AS ownership
              ON ownership.recovery_point_id = run.recovery_point_id
            WHERE run.id = work.restore_run_id
        ) END,
        CASE WHEN work.kind = 'verification' THEN (
            SELECT ownership.workspace_id
            FROM verification_runs AS run
            JOIN workspace_recovery_points AS ownership
              ON ownership.recovery_point_id = run.recovery_point_id
            WHERE run.id = work.verification_run_id
        ) END,
        CASE WHEN work.kind = 'catalog_rebuild' THEN (
            SELECT ownership.workspace_id
            FROM catalog_rebuild_runs AS run
            JOIN workspace_destinations AS ownership ON ownership.destination_id = run.destination_id
            WHERE run.id = work.catalog_rebuild_run_id
        ) END,
        CASE WHEN work.kind = 'operation' THEN (
            SELECT COALESCE(connection_owner.workspace_id, destination_owner.workspace_id, recovery_owner.workspace_id)
            FROM operation_runs AS run
            LEFT JOIN workspace_connection_profiles AS connection_owner
              ON run.subject_type = 'connection_profile'
             AND connection_owner.connection_profile_id = run.subject_id
            LEFT JOIN workspace_destinations AS destination_owner
              ON run.subject_type = 'destination'
             AND destination_owner.destination_id = run.subject_id
            LEFT JOIN workspace_recovery_points AS recovery_owner
              ON run.subject_type = 'recovery_point'
             AND recovery_owner.recovery_point_id = run.subject_id
            WHERE run.id = work.operation_run_id
        ) END,
        '00000000-0000-4000-8000-000000000001'
    ),
    work.id,
    work.created_at_ms
FROM work_items AS work;

INSERT INTO workspace_activity_events(workspace_id, activity_event_seq, created_at_ms)
SELECT
    COALESCE(
        work_owner.workspace_id,
        connection_owner.workspace_id,
        destination_owner.workspace_id,
        plan_owner.workspace_id,
        recovery_owner.workspace_id,
        '00000000-0000-4000-8000-000000000001'
    ),
    event.seq,
    event.created_at_ms
FROM activity_events AS event
LEFT JOIN workspace_work_items AS work_owner ON work_owner.work_item_id = event.work_item_id
LEFT JOIN workspace_connection_profiles AS connection_owner
  ON event.entity_type = 'connection_profile'
 AND connection_owner.connection_profile_id = event.entity_id
LEFT JOIN workspace_destinations AS destination_owner
  ON event.entity_type = 'destination'
 AND destination_owner.destination_id = event.entity_id
LEFT JOIN workspace_backup_plans AS plan_owner
  ON event.entity_type = 'backup_plan'
 AND plan_owner.backup_plan_id = event.entity_id
LEFT JOIN workspace_recovery_points AS recovery_owner
  ON event.entity_type = 'recovery_point'
 AND recovery_owner.recovery_point_id = event.entity_id;

CREATE TRIGGER trg_connection_workspace_default
AFTER INSERT ON connection_profiles
BEGIN
    INSERT INTO workspace_connection_profiles(
        workspace_id, connection_profile_id, display_name_key, created_at_ms
    ) VALUES (
        '00000000-0000-4000-8000-000000000001',
        NEW.id,
        lower(trim(NEW.display_name)),
        NEW.created_at_ms
    );
END;

CREATE TRIGGER trg_connection_workspace_name
AFTER UPDATE OF display_name ON connection_profiles
BEGIN
    UPDATE workspace_connection_profiles
    SET display_name_key = lower(trim(NEW.display_name))
    WHERE connection_profile_id = NEW.id;
END;

CREATE TRIGGER trg_destination_workspace_default
AFTER INSERT ON destinations
BEGIN
    INSERT INTO workspace_destinations(
        workspace_id, destination_id, display_name_key, created_at_ms
    ) VALUES (
        '00000000-0000-4000-8000-000000000001',
        NEW.id,
        lower(trim(NEW.display_name)),
        NEW.created_at_ms
    );
END;

CREATE TRIGGER trg_destination_workspace_name
AFTER UPDATE OF display_name ON destinations
BEGIN
    UPDATE workspace_destinations
    SET display_name_key = lower(trim(NEW.display_name))
    WHERE destination_id = NEW.id;
END;

CREATE TRIGGER trg_plan_workspace_default
AFTER INSERT ON backup_plans
BEGIN
    INSERT INTO workspace_backup_plans(
        workspace_id, backup_plan_id, display_name_key, created_at_ms
    ) VALUES (
        '00000000-0000-4000-8000-000000000001',
        NEW.id,
        lower(trim(NEW.display_name)),
        NEW.created_at_ms
    );
END;

CREATE TRIGGER trg_plan_workspace_name
AFTER UPDATE OF display_name ON backup_plans
BEGIN
    UPDATE workspace_backup_plans
    SET display_name_key = lower(trim(NEW.display_name))
    WHERE backup_plan_id = NEW.id;
END;

CREATE TRIGGER trg_recovery_point_workspace_default
AFTER INSERT ON catalog_recovery_points
BEGIN
    INSERT INTO workspace_recovery_points(workspace_id, recovery_point_id, created_at_ms)
    VALUES (
        COALESCE(
            (
                SELECT ownership.workspace_id
                FROM backup_runs AS run
                JOIN backup_plan_revisions AS revision ON revision.id = run.plan_revision_id
                JOIN workspace_backup_plans AS ownership
                  ON ownership.backup_plan_id = revision.backup_plan_id
                WHERE run.id = NEW.origin_backup_run_id
            ),
            '00000000-0000-4000-8000-000000000001'
        ),
        NEW.id,
        NEW.first_ingested_at_ms
    );
END;

CREATE TRIGGER trg_work_item_workspace_inherit
AFTER INSERT ON work_items
BEGIN
    INSERT INTO workspace_work_items(workspace_id, work_item_id, created_at_ms)
    VALUES (
        COALESCE(
            CASE WHEN NEW.kind = 'backup' THEN (
                SELECT ownership.workspace_id
                FROM backup_runs AS run
                JOIN backup_plan_revisions AS revision ON revision.id = run.plan_revision_id
                JOIN workspace_backup_plans AS ownership
                  ON ownership.backup_plan_id = revision.backup_plan_id
                WHERE run.id = NEW.backup_run_id
            ) END,
            CASE WHEN NEW.kind = 'restore' THEN (
                SELECT ownership.workspace_id
                FROM restore_runs AS run
                JOIN workspace_recovery_points AS ownership
                  ON ownership.recovery_point_id = run.recovery_point_id
                WHERE run.id = NEW.restore_run_id
            ) END,
            CASE WHEN NEW.kind = 'verification' THEN (
                SELECT ownership.workspace_id
                FROM verification_runs AS run
                JOIN workspace_recovery_points AS ownership
                  ON ownership.recovery_point_id = run.recovery_point_id
                WHERE run.id = NEW.verification_run_id
            ) END,
            CASE WHEN NEW.kind = 'catalog_rebuild' THEN (
                SELECT ownership.workspace_id
                FROM catalog_rebuild_runs AS run
                JOIN workspace_destinations AS ownership ON ownership.destination_id = run.destination_id
                WHERE run.id = NEW.catalog_rebuild_run_id
            ) END,
            CASE WHEN NEW.kind = 'operation' THEN (
                SELECT COALESCE(
                    connection_owner.workspace_id,
                    destination_owner.workspace_id,
                    recovery_owner.workspace_id
                )
                FROM operation_runs AS run
                LEFT JOIN workspace_connection_profiles AS connection_owner
                  ON run.subject_type = 'connection_profile'
                 AND connection_owner.connection_profile_id = run.subject_id
                LEFT JOIN workspace_destinations AS destination_owner
                  ON run.subject_type = 'destination'
                 AND destination_owner.destination_id = run.subject_id
                LEFT JOIN workspace_recovery_points AS recovery_owner
                  ON run.subject_type = 'recovery_point'
                 AND recovery_owner.recovery_point_id = run.subject_id
                WHERE run.id = NEW.operation_run_id
            ) END,
            '00000000-0000-4000-8000-000000000001'
        ),
        NEW.id,
        NEW.created_at_ms
    );
END;

CREATE TRIGGER trg_activity_workspace_inherit
AFTER INSERT ON activity_events
BEGIN
    INSERT INTO workspace_activity_events(workspace_id, activity_event_seq, created_at_ms)
    VALUES (
        COALESCE(
            (SELECT workspace_id FROM workspace_work_items WHERE work_item_id = NEW.work_item_id),
            CASE WHEN NEW.entity_type = 'connection_profile' THEN (
                SELECT workspace_id FROM workspace_connection_profiles
                WHERE connection_profile_id = NEW.entity_id
            ) END,
            CASE WHEN NEW.entity_type = 'destination' THEN (
                SELECT workspace_id FROM workspace_destinations WHERE destination_id = NEW.entity_id
            ) END,
            CASE WHEN NEW.entity_type = 'backup_plan' THEN (
                SELECT workspace_id FROM workspace_backup_plans WHERE backup_plan_id = NEW.entity_id
            ) END,
            CASE WHEN NEW.entity_type = 'recovery_point' THEN (
                SELECT workspace_id FROM workspace_recovery_points WHERE recovery_point_id = NEW.entity_id
            ) END,
            '00000000-0000-4000-8000-000000000001'
        ),
        NEW.seq,
        NEW.created_at_ms
    );
END;

CREATE TRIGGER trg_plan_target_workspace_scope
BEFORE INSERT ON plan_revision_targets
WHEN (
    SELECT ownership.workspace_id
    FROM backup_plan_revisions AS revision
    JOIN workspace_backup_plans AS ownership
      ON ownership.backup_plan_id = revision.backup_plan_id
    WHERE revision.id = NEW.plan_revision_id
) IS NOT (
    SELECT ownership.workspace_id
    FROM database_targets AS target
    JOIN workspace_connection_profiles AS ownership
      ON ownership.connection_profile_id = target.connection_profile_id
    WHERE target.id = NEW.database_target_id
)
BEGIN
    SELECT RAISE(ABORT, 'WORKSPACE_SCOPE_MISMATCH');
END;

CREATE TRIGGER trg_plan_destination_workspace_scope
BEFORE INSERT ON plan_revision_destinations
WHEN (
    SELECT ownership.workspace_id
    FROM backup_plan_revisions AS revision
    JOIN workspace_backup_plans AS ownership
      ON ownership.backup_plan_id = revision.backup_plan_id
    WHERE revision.id = NEW.plan_revision_id
) IS NOT (
    SELECT ownership.workspace_id
    FROM workspace_destinations AS ownership
    WHERE ownership.destination_id = NEW.destination_id
)
BEGIN
    SELECT RAISE(ABORT, 'WORKSPACE_SCOPE_MISMATCH');
END;

PRAGMA user_version = 3;
COMMIT;
