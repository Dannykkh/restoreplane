-- Connected Backup Platform v2 Operational Store
-- Target: SQLite 3.45+ through rusqlite "bundled".
-- Application timestamps are UTC Unix epoch milliseconds.
-- The service owns the only read-write connection. Other connections are read-only.

PRAGMA foreign_keys = ON;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
PRAGMA busy_timeout = 5000;

BEGIN IMMEDIATE;

-- ---------------------------------------------------------------------------
-- Schema lifecycle and control-store recovery
-- ---------------------------------------------------------------------------

CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    checksum_blake3 TEXT NOT NULL CHECK(length(checksum_blake3) = 64),
    applied_at_ms INTEGER NOT NULL CHECK(applied_at_ms >= 0),
    app_version TEXT NOT NULL,
    execution_ms INTEGER NOT NULL CHECK(execution_ms >= 0)
) STRICT;

CREATE TABLE app_meta (
    key TEXT PRIMARY KEY,
    value_text TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= 0)
) STRICT;

CREATE TABLE control_store_backups (
    id TEXT PRIMARY KEY,
    storage_kind TEXT NOT NULL CHECK(storage_kind IN ('local', 'destination')),
    locator TEXT NOT NULL,
    schema_version INTEGER NOT NULL CHECK(schema_version > 0),
    checksum_blake3 TEXT CHECK(checksum_blake3 IS NULL OR length(checksum_blake3) = 64),
    encryption_key_ref TEXT REFERENCES secret_refs(id) ON DELETE RESTRICT,
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms >= started_at_ms),
    status TEXT NOT NULL CHECK(status IN ('running', 'succeeded', 'failed', 'cancelled')),
    integrity_result TEXT CHECK(integrity_result IS NULL OR integrity_result IN ('passed', 'failed', 'not_run')),
    rehearsal_result TEXT CHECK(rehearsal_result IS NULL OR rehearsal_result IN ('passed', 'failed', 'not_run')),
    rehearsal_at_ms INTEGER CHECK(rehearsal_at_ms IS NULL OR rehearsal_at_ms >= 0),
    error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
) STRICT;

CREATE INDEX idx_control_store_backups_completed
    ON control_store_backups(completed_at_ms DESC)
    WHERE status = 'succeeded';

CREATE TABLE config_import_runs (
    id TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('legacy_toml', 'v2_export', 'recovery_bundle')),
    source_checksum_blake3 TEXT NOT NULL CHECK(length(source_checksum_blake3) = 64),
    mode TEXT NOT NULL CHECK(mode IN ('dry_run', 'shadow', 'cutover')),
    status TEXT NOT NULL CHECK(status IN ('running', 'succeeded', 'failed', 'cancelled')),
    diff_json TEXT NOT NULL CHECK(json_valid(diff_json)),
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms >= started_at_ms),
    cutover_marker TEXT,
    confirmation_token_id TEXT REFERENCES confirmation_tokens(id) ON DELETE RESTRICT,
    error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    CHECK(
        (mode = 'cutover' AND confirmation_token_id IS NOT NULL)
        OR
        (mode <> 'cutover' AND confirmation_token_id IS NULL)
    )
) STRICT;

CREATE UNIQUE INDEX idx_config_import_cutover_marker
    ON config_import_runs(cutover_marker)
    WHERE cutover_marker IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Secret metadata, native tools, and support certification
-- Secret payloads never live in this database.
-- ---------------------------------------------------------------------------

CREATE TABLE secret_refs (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL CHECK(provider IN ('windows_dpapi', 'external')),
    provider_locator TEXT NOT NULL,
    purpose TEXT NOT NULL,
    version INTEGER NOT NULL CHECK(version > 0),
    state TEXT NOT NULL CHECK(state IN ('active', 'retired', 'revoked')),
    fingerprint TEXT NOT NULL,
    rotated_from_id TEXT REFERENCES secret_refs(id) ON DELETE RESTRICT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    retired_at_ms INTEGER CHECK(retired_at_ms IS NULL OR retired_at_ms >= created_at_ms)
) STRICT;

CREATE UNIQUE INDEX idx_secret_refs_provider_locator_version
    ON secret_refs(provider, provider_locator, version);

CREATE INDEX idx_secret_refs_rotated_from
    ON secret_refs(rotated_from_id)
    WHERE rotated_from_id IS NOT NULL;

-- ---------------------------------------------------------------------------
-- Local IPC replay protection and destructive confirmation
-- Only hashes/digests are stored. Raw confirmation tokens never enter SQLite.
-- ---------------------------------------------------------------------------

CREATE TABLE ipc_request_ledger (
    request_id TEXT PRIMARY KEY,
    command_name TEXT NOT NULL,
    client_sid_hash TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    payload_digest_blake3 TEXT NOT NULL CHECK(length(payload_digest_blake3) = 64),
    state TEXT NOT NULL CHECK(state IN ('accepted', 'running', 'succeeded', 'failed_terminal')),
    result_ref_type TEXT,
    result_ref_id TEXT,
    response_json TEXT CHECK(response_json IS NULL OR json_valid(response_json)),
    accepted_at_ms INTEGER NOT NULL CHECK(accepted_at_ms >= 0),
    completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms >= accepted_at_ms),
    expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms > accepted_at_ms),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    UNIQUE(client_sid_hash, command_name, idempotency_key)
) STRICT;

CREATE INDEX idx_ipc_request_ledger_expiry
    ON ipc_request_ledger(expires_at_ms, state);

CREATE TABLE confirmation_tokens (
    id TEXT PRIMARY KEY,
    token_digest_blake3 TEXT NOT NULL UNIQUE CHECK(length(token_digest_blake3) = 64),
    action TEXT NOT NULL CHECK(action IN (
        'restore_overwrite', 'retention_delete', 'connection_archive',
        'destination_archive', 'secret_rotate', 'config_cutover',
        'control_store_restore', 'acl_change'
    )),
    client_sid_hash TEXT NOT NULL,
    resource_type TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    resource_fingerprint TEXT NOT NULL,
    payload_digest_blake3 TEXT NOT NULL CHECK(length(payload_digest_blake3) = 64),
    state TEXT NOT NULL CHECK(state IN ('issued', 'consumed', 'revoked', 'expired')),
    issued_at_ms INTEGER NOT NULL CHECK(issued_at_ms >= 0),
    expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms > issued_at_ms),
    consumed_at_ms INTEGER CHECK(consumed_at_ms IS NULL OR consumed_at_ms >= issued_at_ms),
    consumed_by_request_id TEXT REFERENCES ipc_request_ledger(request_id) ON DELETE RESTRICT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    CHECK(
        (state = 'consumed' AND consumed_at_ms IS NOT NULL AND consumed_by_request_id IS NOT NULL)
        OR
        (state <> 'consumed' AND consumed_at_ms IS NULL AND consumed_by_request_id IS NULL)
    )
) STRICT;

CREATE INDEX idx_confirmation_tokens_subject_expiry
    ON confirmation_tokens(client_sid_hash, action, expires_at_ms)
    WHERE state = 'issued';

CREATE TRIGGER trg_confirmation_token_binding_immutable
BEFORE UPDATE ON confirmation_tokens
WHEN
    NEW.id IS NOT OLD.id
    OR NEW.token_digest_blake3 IS NOT OLD.token_digest_blake3
    OR NEW.action IS NOT OLD.action
    OR NEW.client_sid_hash IS NOT OLD.client_sid_hash
    OR NEW.resource_type IS NOT OLD.resource_type
    OR NEW.resource_id IS NOT OLD.resource_id
    OR NEW.resource_fingerprint IS NOT OLD.resource_fingerprint
    OR NEW.payload_digest_blake3 IS NOT OLD.payload_digest_blake3
    OR NEW.issued_at_ms IS NOT OLD.issued_at_ms
    OR NEW.expires_at_ms IS NOT OLD.expires_at_ms
BEGIN
    SELECT RAISE(ABORT, 'confirmation token binding is immutable');
END;

CREATE TRIGGER trg_confirmation_token_state_transition
BEFORE UPDATE OF state ON confirmation_tokens
WHEN NOT (
    NEW.state = OLD.state
    OR (
        OLD.state = 'issued'
        AND NEW.state IN ('consumed', 'revoked', 'expired')
    )
)
BEGIN
    SELECT RAISE(ABORT, 'invalid confirmation token state transition');
END;

CREATE TRIGGER trg_confirmation_token_terminal_immutable
BEFORE UPDATE ON confirmation_tokens
WHEN OLD.state IN ('consumed', 'revoked', 'expired')
BEGIN
    SELECT RAISE(ABORT, 'terminal confirmation token is immutable');
END;

CREATE TABLE native_tool_inventory (
    id TEXT PRIMARY KEY,
    tool_name TEXT NOT NULL,
    executable_path TEXT NOT NULL,
    version_text TEXT NOT NULL,
    architecture TEXT NOT NULL,
    file_sha256 TEXT NOT NULL CHECK(length(file_sha256) = 64),
    publisher TEXT,
    signature_status TEXT NOT NULL CHECK(signature_status IN ('valid', 'invalid', 'unsigned', 'not_checked')),
    trust_status TEXT NOT NULL CHECK(trust_status IN ('trusted', 'blocked', 'unreviewed')),
    source_kind TEXT NOT NULL CHECK(source_kind IN ('bundled', 'system', 'operator')),
    first_seen_at_ms INTEGER NOT NULL CHECK(first_seen_at_ms >= 0),
    last_seen_at_ms INTEGER NOT NULL CHECK(last_seen_at_ms >= first_seen_at_ms),
    last_verified_at_ms INTEGER CHECK(last_verified_at_ms IS NULL OR last_verified_at_ms >= first_seen_at_ms),
    deleted_at_ms INTEGER CHECK(deleted_at_ms IS NULL OR deleted_at_ms >= first_seen_at_ms),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE UNIQUE INDEX idx_native_tool_active_path
    ON native_tool_inventory(executable_path)
    WHERE deleted_at_ms IS NULL;

CREATE INDEX idx_native_tool_lookup
    ON native_tool_inventory(tool_name, trust_status, version_text);

CREATE TABLE engine_certifications (
    id TEXT PRIMARY KEY,
    engine TEXT NOT NULL CHECK(engine IN (
        'postgresql', 'mysql', 'mariadb', 'sqlserver', 'sqlite',
        'mongodb', 'firebird', 'redis', 'valkey'
    )),
    server_version_range TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    tool_version_range TEXT NOT NULL,
    operating_system TEXT NOT NULL,
    architecture TEXT NOT NULL,
    dump_format TEXT NOT NULL,
    consistency_grade TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('candidate', 'certified', 'suspended', 'expired')),
    evidence_locator TEXT NOT NULL,
    evidence_checksum_blake3 TEXT NOT NULL CHECK(length(evidence_checksum_blake3) = 64),
    certified_at_ms INTEGER CHECK(certified_at_ms IS NULL OR certified_at_ms >= 0),
    expires_at_ms INTEGER CHECK(expires_at_ms IS NULL OR expires_at_ms >= 0),
    suspended_reason TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    CHECK(expires_at_ms IS NULL OR certified_at_ms IS NULL OR expires_at_ms > certified_at_ms)
) STRICT;

CREATE INDEX idx_engine_certifications_match
    ON engine_certifications(engine, status, expires_at_ms);

CREATE TABLE engine_certification_capabilities (
    certification_id TEXT NOT NULL REFERENCES engine_certifications(id) ON DELETE CASCADE,
    capability TEXT NOT NULL CHECK(capability IN (
        'probe', 'discovery', 'tool_preflight', 'backup', 'verify_l1', 'verify_l2',
        'restore_create_new', 'validation', 'restore_l3'
    )),
    support_status TEXT NOT NULL CHECK(support_status IN ('passed', 'limited', 'failed', 'not_applicable')),
    limitation_text TEXT,
    evidence_locator TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (certification_id, capability)
) STRICT;

-- ---------------------------------------------------------------------------
-- Connections, discovery, and protected database targets
-- ---------------------------------------------------------------------------

CREATE TABLE connection_profiles (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    engine TEXT NOT NULL CHECK(engine IN (
        'postgresql', 'mysql', 'mariadb', 'sqlserver', 'sqlite',
        'mongodb', 'firebird', 'redis', 'valkey'
    )),
    transport TEXT NOT NULL CHECK(transport IN ('tcp', 'unix_socket', 'file')),
    host TEXT,
    port INTEGER CHECK(port IS NULL OR port BETWEEN 1 AND 65535),
    file_path TEXT,
    database_hint TEXT,
    tls_mode TEXT NOT NULL CHECK(tls_mode IN ('disabled', 'preferred', 'required', 'verify_ca', 'verify_full')),
    tunnel_config_json TEXT CHECK(tunnel_config_json IS NULL OR json_valid(tunnel_config_json)),
    adapter_config_json TEXT NOT NULL CHECK(json_valid(adapter_config_json)),
    state TEXT NOT NULL CHECK(state IN ('draft', 'ready', 'attention', 'disabled')),
    last_probe_status TEXT CHECK(last_probe_status IS NULL OR last_probe_status IN ('passed', 'failed', 'cancelled')),
    last_probe_at_ms INTEGER CHECK(last_probe_at_ms IS NULL OR last_probe_at_ms >= 0),
    source_resource_key TEXT NOT NULL,
    deleted_at_ms INTEGER CHECK(deleted_at_ms IS NULL OR deleted_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    CHECK(
        (transport = 'file' AND file_path IS NOT NULL AND host IS NULL AND port IS NULL)
        OR
        (transport <> 'file' AND host IS NOT NULL AND file_path IS NULL)
    )
) STRICT;

CREATE UNIQUE INDEX idx_connection_profiles_active_name
    ON connection_profiles(display_name)
    WHERE deleted_at_ms IS NULL;

CREATE INDEX idx_connection_profiles_engine_state
    ON connection_profiles(engine, state)
    WHERE deleted_at_ms IS NULL;

CREATE TABLE connection_secret_bindings (
    connection_profile_id TEXT NOT NULL REFERENCES connection_profiles(id) ON DELETE CASCADE,
    secret_role TEXT NOT NULL CHECK(secret_role IN (
        'database_auth', 'tls_client_key', 'tls_ca', 'tunnel_auth'
    )),
    secret_ref_id TEXT NOT NULL REFERENCES secret_refs(id) ON DELETE RESTRICT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (connection_profile_id, secret_role)
) STRICT;

CREATE INDEX idx_connection_secret_bindings_secret
    ON connection_secret_bindings(secret_ref_id);

CREATE TABLE connection_probe_runs (
    id TEXT PRIMARY KEY,
    connection_profile_id TEXT NOT NULL REFERENCES connection_profiles(id) ON DELETE RESTRICT,
    trigger_kind TEXT NOT NULL CHECK(trigger_kind IN ('create', 'manual', 'scheduled', 'preflight')),
    status TEXT NOT NULL CHECK(status IN ('running', 'passed', 'failed', 'cancelled')),
    failure_class TEXT CHECK(failure_class IS NULL OR failure_class IN (
        'dns', 'network', 'tls', 'authentication', 'authorization',
        'tool_missing', 'tool_untrusted', 'unsupported_version', 'unknown'
    )),
    server_version TEXT,
    tool_inventory_id TEXT REFERENCES native_tool_inventory(id) ON DELETE SET NULL,
    certification_id TEXT REFERENCES engine_certifications(id) ON DELETE SET NULL,
    observed_capabilities_json TEXT NOT NULL CHECK(json_valid(observed_capabilities_json)),
    redacted_diagnostics TEXT,
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms >= started_at_ms),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
) STRICT;

CREATE INDEX idx_connection_probe_profile_time
    ON connection_probe_runs(connection_profile_id, started_at_ms DESC);

CREATE TABLE discovery_runs (
    id TEXT PRIMARY KEY,
    connection_profile_id TEXT NOT NULL REFERENCES connection_profiles(id) ON DELETE RESTRICT,
    probe_run_id TEXT REFERENCES connection_probe_runs(id) ON DELETE SET NULL,
    status TEXT NOT NULL CHECK(status IN ('running', 'succeeded', 'failed', 'cancelled')),
    added_count INTEGER NOT NULL DEFAULT 0 CHECK(added_count >= 0),
    missing_count INTEGER NOT NULL DEFAULT 0 CHECK(missing_count >= 0),
    changed_count INTEGER NOT NULL DEFAULT 0 CHECK(changed_count >= 0),
    error_code TEXT,
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms >= started_at_ms),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
) STRICT;

CREATE INDEX idx_discovery_runs_profile_time
    ON discovery_runs(connection_profile_id, started_at_ms DESC);

CREATE TABLE discovered_databases (
    id TEXT PRIMARY KEY,
    discovery_run_id TEXT NOT NULL REFERENCES discovery_runs(id) ON DELETE CASCADE,
    connection_profile_id TEXT NOT NULL REFERENCES connection_profiles(id) ON DELETE RESTRICT,
    engine_object_id TEXT NOT NULL,
    database_name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    eligibility TEXT NOT NULL CHECK(eligibility IN ('eligible', 'excluded', 'offline', 'unknown')),
    exclusion_reason TEXT,
    attributes_json TEXT NOT NULL CHECK(json_valid(attributes_json)),
    fingerprint TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    UNIQUE(discovery_run_id, engine_object_id)
) STRICT;

CREATE INDEX idx_discovered_databases_profile_name
    ON discovered_databases(connection_profile_id, normalized_name);

CREATE TABLE database_targets (
    id TEXT PRIMARY KEY,
    connection_profile_id TEXT NOT NULL REFERENCES connection_profiles(id) ON DELETE RESTRICT,
    engine_object_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('active', 'attention', 'disabled', 'retired')),
    missing_since_ms INTEGER CHECK(missing_since_ms IS NULL OR missing_since_ms >= 0),
    last_seen_discovery_run_id TEXT REFERENCES discovery_runs(id) ON DELETE SET NULL,
    selected_at_ms INTEGER NOT NULL CHECK(selected_at_ms >= 0),
    retired_at_ms INTEGER CHECK(retired_at_ms IS NULL OR retired_at_ms >= selected_at_ms),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    UNIQUE(connection_profile_id, engine_object_id)
) STRICT;

CREATE INDEX idx_database_targets_connection_state
    ON database_targets(connection_profile_id, state);

-- ---------------------------------------------------------------------------
-- Destinations and destination health
-- ---------------------------------------------------------------------------

CREATE TABLE destinations (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('local', 's3', 'ssh_sftp', 'azure_blob', 'smb', 'rclone')),
    endpoint_json TEXT NOT NULL CHECK(json_valid(endpoint_json)),
    state TEXT NOT NULL CHECK(state IN ('draft', 'ready', 'attention', 'disabled')),
    declared_capabilities_json TEXT NOT NULL CHECK(json_valid(declared_capabilities_json)),
    observed_capabilities_json TEXT NOT NULL CHECK(json_valid(observed_capabilities_json)),
    immutability_capability TEXT NOT NULL CHECK(immutability_capability IN (
        'none', 'governance', 'compliance', 'server_append_only'
    )),
    last_probe_status TEXT CHECK(last_probe_status IS NULL OR last_probe_status IN ('passed', 'failed', 'cancelled')),
    last_probe_at_ms INTEGER CHECK(last_probe_at_ms IS NULL OR last_probe_at_ms >= 0),
    deleted_at_ms INTEGER CHECK(deleted_at_ms IS NULL OR deleted_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE UNIQUE INDEX idx_destinations_active_name
    ON destinations(display_name)
    WHERE deleted_at_ms IS NULL;

CREATE INDEX idx_destinations_kind_state
    ON destinations(kind, state)
    WHERE deleted_at_ms IS NULL;

CREATE TABLE destination_secret_bindings (
    destination_id TEXT NOT NULL REFERENCES destinations(id) ON DELETE CASCADE,
    secret_role TEXT NOT NULL CHECK(secret_role IN (
        'backup_writer', 'restore_reader', 'delete_operator', 'host_auth'
    )),
    secret_ref_id TEXT NOT NULL REFERENCES secret_refs(id) ON DELETE RESTRICT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (destination_id, secret_role)
) STRICT;

CREATE INDEX idx_destination_secret_bindings_secret
    ON destination_secret_bindings(secret_ref_id);

CREATE TABLE destination_probe_runs (
    id TEXT PRIMARY KEY,
    destination_id TEXT NOT NULL REFERENCES destinations(id) ON DELETE RESTRICT,
    credential_role TEXT NOT NULL CHECK(credential_role IN ('backup_writer', 'restore_reader', 'delete_operator')),
    status TEXT NOT NULL CHECK(status IN ('running', 'passed', 'failed', 'cancelled')),
    observed_capabilities_json TEXT NOT NULL CHECK(json_valid(observed_capabilities_json)),
    free_bytes INTEGER CHECK(free_bytes IS NULL OR free_bytes >= 0),
    latency_ms INTEGER CHECK(latency_ms IS NULL OR latency_ms >= 0),
    failure_class TEXT,
    redacted_diagnostics TEXT,
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms >= started_at_ms),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
) STRICT;

CREATE INDEX idx_destination_probe_destination_time
    ON destination_probe_runs(destination_id, started_at_ms DESC);

-- ---------------------------------------------------------------------------
-- Versioned plans and idempotent occurrences
-- ---------------------------------------------------------------------------

CREATE TABLE backup_plans (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('draft', 'enabled', 'paused', 'attention', 'retired')),
    current_revision_id TEXT REFERENCES backup_plan_revisions(id) ON DELETE RESTRICT,
    deleted_at_ms INTEGER CHECK(deleted_at_ms IS NULL OR deleted_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE UNIQUE INDEX idx_backup_plans_active_name
    ON backup_plans(display_name)
    WHERE deleted_at_ms IS NULL;

CREATE INDEX idx_backup_plans_state
    ON backup_plans(state)
    WHERE deleted_at_ms IS NULL;

CREATE TABLE backup_plan_revisions (
    id TEXT PRIMARY KEY,
    backup_plan_id TEXT NOT NULL REFERENCES backup_plans(id) ON DELETE RESTRICT,
    revision_no INTEGER NOT NULL CHECK(revision_no > 0),
    schedule_kind TEXT NOT NULL CHECK(schedule_kind IN ('cron', 'interval', 'manual')),
    schedule_expression TEXT,
    timezone TEXT NOT NULL,
    catchup_policy TEXT NOT NULL CHECK(catchup_policy IN ('skip', 'bounded_latest_one')),
    overlap_policy TEXT NOT NULL CHECK(overlap_policy IN ('coalesce_one', 'queue_all', 'skip')),
    rpo_ms INTEGER CHECK(rpo_ms IS NULL OR rpo_ms > 0),
    retention_policy_json TEXT NOT NULL CHECK(json_valid(retention_policy_json)),
    verification_policy_json TEXT NOT NULL CHECK(json_valid(verification_policy_json)),
    execution_policy_json TEXT NOT NULL CHECK(json_valid(execution_policy_json)),
    content_hash_blake3 TEXT NOT NULL CHECK(length(content_hash_blake3) = 64),
    published_at_ms INTEGER CHECK(published_at_ms IS NULL OR published_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    UNIQUE(backup_plan_id, revision_no),
    UNIQUE(id, backup_plan_id),
    CHECK(schedule_kind = 'manual' OR schedule_expression IS NOT NULL)
) STRICT;

CREATE INDEX idx_backup_plan_revisions_plan
    ON backup_plan_revisions(backup_plan_id, revision_no DESC);

CREATE TABLE plan_revision_targets (
    plan_revision_id TEXT NOT NULL REFERENCES backup_plan_revisions(id) ON DELETE RESTRICT,
    database_target_id TEXT NOT NULL REFERENCES database_targets(id) ON DELETE RESTRICT,
    target_options_json TEXT NOT NULL CHECK(json_valid(target_options_json)),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (plan_revision_id, database_target_id)
) STRICT;

CREATE INDEX idx_plan_revision_targets_target
    ON plan_revision_targets(database_target_id, plan_revision_id);

CREATE TABLE plan_revision_destinations (
    plan_revision_id TEXT NOT NULL REFERENCES backup_plan_revisions(id) ON DELETE RESTRICT,
    destination_id TEXT NOT NULL REFERENCES destinations(id) ON DELETE RESTRICT,
    requirement TEXT NOT NULL CHECK(requirement IN ('required', 'best_effort')),
    retention_override_json TEXT CHECK(retention_override_json IS NULL OR json_valid(retention_override_json)),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (plan_revision_id, destination_id)
) STRICT;

CREATE INDEX idx_plan_revision_destinations_destination
    ON plan_revision_destinations(destination_id, plan_revision_id);

CREATE TABLE plan_occurrences (
    id TEXT PRIMARY KEY,
    plan_revision_id TEXT NOT NULL REFERENCES backup_plan_revisions(id) ON DELETE RESTRICT,
    origin TEXT NOT NULL CHECK(origin IN ('scheduled', 'catch_up', 'manual')),
    scheduled_for_ms INTEGER,
    requested_at_ms INTEGER NOT NULL CHECK(requested_at_ms >= 0),
    request_idempotency_key TEXT,
    state TEXT NOT NULL CHECK(state IN (
        'pending', 'enqueued', 'running', 'succeeded',
        'partially_succeeded', 'failed', 'missed', 'cancelled'
    )),
    missed_reason TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    UNIQUE(id, plan_revision_id),
    CHECK(
        (origin IN ('scheduled', 'catch_up') AND scheduled_for_ms IS NOT NULL AND request_idempotency_key IS NULL)
        OR
        (origin = 'manual' AND scheduled_for_ms IS NULL AND request_idempotency_key IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX idx_plan_occurrences_scheduled_identity
    ON plan_occurrences(plan_revision_id, scheduled_for_ms)
    WHERE origin IN ('scheduled', 'catch_up');

CREATE UNIQUE INDEX idx_plan_occurrences_manual_identity
    ON plan_occurrences(request_idempotency_key)
    WHERE request_idempotency_key IS NOT NULL;

CREATE INDEX idx_plan_occurrences_state_due
    ON plan_occurrences(state, scheduled_for_ms, requested_at_ms);

-- A revision is mutable only while unpublished. Membership rows follow the same rule.
CREATE TRIGGER trg_plan_revision_published_update
BEFORE UPDATE ON backup_plan_revisions
WHEN OLD.published_at_ms IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan revision is immutable');
END;

CREATE TRIGGER trg_plan_revision_publish_requires_members
BEFORE UPDATE OF published_at_ms ON backup_plan_revisions
WHEN OLD.published_at_ms IS NULL AND NEW.published_at_ms IS NOT NULL
BEGIN
    SELECT CASE
        WHEN NOT EXISTS (
            SELECT 1 FROM plan_revision_targets WHERE plan_revision_id = NEW.id
        ) THEN RAISE(ABORT, 'plan revision requires at least one target')
        WHEN NOT EXISTS (
            SELECT 1 FROM plan_revision_destinations WHERE plan_revision_id = NEW.id
        ) THEN RAISE(ABORT, 'plan revision requires at least one destination')
        WHEN NOT EXISTS (
            SELECT 1
            FROM plan_revision_destinations
            WHERE plan_revision_id = NEW.id AND requirement = 'required'
        ) THEN RAISE(ABORT, 'plan revision requires at least one required destination')
    END;
END;

CREATE TRIGGER trg_plan_revision_direct_published_insert
BEFORE INSERT ON backup_plan_revisions
WHEN NEW.published_at_ms IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'plan revision must be inserted as draft and then published');
END;

CREATE TRIGGER trg_backup_plan_current_revision
BEFORE UPDATE OF current_revision_id ON backup_plans
WHEN NEW.current_revision_id IS NOT NULL
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1
        FROM backup_plan_revisions
        WHERE id = NEW.current_revision_id
          AND backup_plan_id = NEW.id
          AND published_at_ms IS NOT NULL
    ) THEN RAISE(ABORT, 'current revision must be a published revision of this plan') END;
END;

CREATE TRIGGER trg_plan_revision_published_delete
BEFORE DELETE ON backup_plan_revisions
WHEN OLD.published_at_ms IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan revision cannot be deleted');
END;

CREATE TRIGGER trg_plan_target_insert_published
BEFORE INSERT ON plan_revision_targets
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = NEW.plan_revision_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan membership is immutable');
END;

CREATE TRIGGER trg_plan_target_update_published
BEFORE UPDATE ON plan_revision_targets
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = OLD.plan_revision_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan membership is immutable');
END;

CREATE TRIGGER trg_plan_target_delete_published
BEFORE DELETE ON plan_revision_targets
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = OLD.plan_revision_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan membership is immutable');
END;

CREATE TRIGGER trg_plan_destination_insert_published
BEFORE INSERT ON plan_revision_destinations
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = NEW.plan_revision_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan membership is immutable');
END;

CREATE TRIGGER trg_plan_destination_update_published
BEFORE UPDATE ON plan_revision_destinations
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = OLD.plan_revision_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan membership is immutable');
END;

CREATE TRIGGER trg_plan_destination_delete_published
BEFORE DELETE ON plan_revision_destinations
WHEN (SELECT published_at_ms FROM backup_plan_revisions WHERE id = OLD.plan_revision_id) IS NOT NULL
BEGIN
    SELECT RAISE(ABORT, 'published plan membership is immutable');
END;

-- ---------------------------------------------------------------------------
-- Runs, durable work queue, attempts, process identity, and resource control
-- ---------------------------------------------------------------------------

CREATE TABLE catalog_rebuild_runs (
    id TEXT PRIMARY KEY,
    destination_id TEXT REFERENCES destinations(id) ON DELETE SET NULL,
    source_kind TEXT NOT NULL CHECK(source_kind IN ('local', 's3', 'ssh_sftp', 'all_configured')),
    mode TEXT NOT NULL CHECK(mode IN ('dry_run', 'merge', 'replace_projection')),
    status TEXT NOT NULL CHECK(status IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    accepted_count INTEGER NOT NULL DEFAULT 0 CHECK(accepted_count >= 0),
    rejected_count INTEGER NOT NULL DEFAULT 0 CHECK(rejected_count >= 0),
    started_at_ms INTEGER CHECK(started_at_ms IS NULL OR started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(
        completed_at_ms IS NULL OR started_at_ms IS NULL OR completed_at_ms >= started_at_ms
    ),
    error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
) STRICT;

CREATE TABLE operation_runs (
    id TEXT PRIMARY KEY,
    operation_kind TEXT NOT NULL CHECK(operation_kind IN (
        'connection_probe', 'connection_discovery', 'destination_probe',
        'replica_repair', 'retention_delete', 'orphan_cleanup',
        'tool_rescan', 'notification_test', 'notification_delivery',
        'config_import', 'config_cutover', 'control_store_backup',
        'control_store_restore', 'support_bundle', 'service_drain'
    )),
    subject_type TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    request_json TEXT NOT NULL CHECK(json_valid(request_json)),
    state TEXT NOT NULL CHECK(state IN (
        'created', 'queued', 'running', 'succeeded',
        'partially_succeeded', 'failed', 'cancelled'
    )),
    result_json TEXT CHECK(result_json IS NULL OR json_valid(result_json)),
    started_at_ms INTEGER CHECK(started_at_ms IS NULL OR started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(
        completed_at_ms IS NULL OR started_at_ms IS NULL OR completed_at_ms >= started_at_ms
    ),
    error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE INDEX idx_operation_runs_subject_time
    ON operation_runs(subject_type, subject_id, created_at_ms DESC);

CREATE INDEX idx_operation_runs_kind_state
    ON operation_runs(operation_kind, state, created_at_ms);

CREATE TABLE backup_runs (
    id TEXT PRIMARY KEY,
    plan_occurrence_id TEXT NOT NULL,
    plan_revision_id TEXT NOT NULL,
    database_target_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN (
        'created', 'queued', 'running', 'committing', 'succeeded',
        'partially_succeeded', 'failed', 'cancelled'
    )),
    consistency_grade TEXT,
    engine TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL,
    tool_inventory_id TEXT REFERENCES native_tool_inventory(id) ON DELETE SET NULL,
    started_at_ms INTEGER CHECK(started_at_ms IS NULL OR started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(
        completed_at_ms IS NULL OR started_at_ms IS NULL OR completed_at_ms >= started_at_ms
    ),
    plaintext_bytes INTEGER CHECK(plaintext_bytes IS NULL OR plaintext_bytes >= 0),
    cipher_bytes INTEGER CHECK(cipher_bytes IS NULL OR cipher_bytes >= 0),
    recovery_point_id TEXT,
    error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    UNIQUE(plan_occurrence_id, database_target_id),
    FOREIGN KEY (plan_occurrence_id, plan_revision_id)
        REFERENCES plan_occurrences(id, plan_revision_id) ON DELETE RESTRICT,
    FOREIGN KEY (plan_revision_id, database_target_id)
        REFERENCES plan_revision_targets(plan_revision_id, database_target_id) ON DELETE RESTRICT
) STRICT;

CREATE INDEX idx_backup_runs_target_time
    ON backup_runs(database_target_id, created_at_ms DESC);

CREATE INDEX idx_backup_runs_state_time
    ON backup_runs(state, created_at_ms);

CREATE TABLE restore_runs (
    id TEXT PRIMARY KEY,
    recovery_point_id TEXT NOT NULL,
    source_replica_id TEXT,
    target_connection_profile_id TEXT NOT NULL REFERENCES connection_profiles(id) ON DELETE RESTRICT,
    target_database_target_id TEXT REFERENCES database_targets(id) ON DELETE RESTRICT,
    target_database_name TEXT NOT NULL,
    target_fingerprint TEXT NOT NULL,
    purpose TEXT NOT NULL CHECK(purpose IN ('incident_recovery', 'restore_drill', 'manual_validation')),
    conflict_policy TEXT NOT NULL CHECK(conflict_policy IN ('create_new', 'fail_if_exists', 'overwrite')),
    eligibility_snapshot_json TEXT NOT NULL CHECK(json_valid(eligibility_snapshot_json)),
    confirmation_token_id TEXT REFERENCES confirmation_tokens(id) ON DELETE RESTRICT,
    state TEXT NOT NULL CHECK(state IN (
        'created', 'blocked', 'queued', 'running', 'validating',
        'succeeded', 'failed', 'cancelled', 'cleanup_required'
    )),
    started_at_ms INTEGER CHECK(started_at_ms IS NULL OR started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(
        completed_at_ms IS NULL OR started_at_ms IS NULL OR completed_at_ms >= started_at_ms
    ),
    validation_result_json TEXT CHECK(validation_result_json IS NULL OR json_valid(validation_result_json)),
    measured_rto_ms INTEGER CHECK(measured_rto_ms IS NULL OR measured_rto_ms >= 0),
    error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    CHECK(
        (conflict_policy = 'overwrite' AND confirmation_token_id IS NOT NULL)
        OR
        (conflict_policy <> 'overwrite' AND confirmation_token_id IS NULL)
    )
) STRICT;

CREATE INDEX idx_restore_runs_recovery_point
    ON restore_runs(recovery_point_id, created_at_ms DESC);

CREATE INDEX idx_restore_runs_state_time
    ON restore_runs(state, created_at_ms);

CREATE TABLE verification_runs (
    id TEXT PRIMARY KEY,
    recovery_point_id TEXT NOT NULL,
    source_replica_id TEXT,
    requested_level INTEGER NOT NULL CHECK(requested_level BETWEEN 1 AND 3),
    achieved_level INTEGER CHECK(achieved_level IS NULL OR achieved_level BETWEEN 1 AND requested_level),
    trigger_kind TEXT NOT NULL CHECK(trigger_kind IN ('post_backup', 'scheduled', 'manual', 'restore_drill')),
    state TEXT NOT NULL CHECK(state IN ('created', 'queued', 'running', 'passed', 'failed', 'cancelled', 'unsupported')),
    restore_run_id TEXT REFERENCES restore_runs(id) ON DELETE RESTRICT,
    evidence_json TEXT CHECK(evidence_json IS NULL OR json_valid(evidence_json)),
    started_at_ms INTEGER CHECK(started_at_ms IS NULL OR started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(
        completed_at_ms IS NULL OR started_at_ms IS NULL OR completed_at_ms >= started_at_ms
    ),
    error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    CHECK(
        (
            requested_level = 3
            AND (
                restore_run_id IS NOT NULL
                OR (state = 'unsupported' AND restore_run_id IS NULL)
            )
        )
        OR
        (requested_level < 3 AND restore_run_id IS NULL)
    )
) STRICT;

CREATE INDEX idx_verification_runs_recovery_point
    ON verification_runs(recovery_point_id, requested_level, created_at_ms DESC);

CREATE INDEX idx_verification_runs_state
    ON verification_runs(state, created_at_ms);

CREATE TABLE work_items (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL CHECK(kind IN (
        'backup', 'restore', 'verification', 'catalog_rebuild', 'operation'
    )),
    backup_run_id TEXT REFERENCES backup_runs(id) ON DELETE RESTRICT,
    restore_run_id TEXT REFERENCES restore_runs(id) ON DELETE RESTRICT,
    verification_run_id TEXT REFERENCES verification_runs(id) ON DELETE RESTRICT,
    catalog_rebuild_run_id TEXT REFERENCES catalog_rebuild_runs(id) ON DELETE RESTRICT,
    operation_run_id TEXT REFERENCES operation_runs(id) ON DELETE RESTRICT,
    priority INTEGER NOT NULL DEFAULT 100 CHECK(priority BETWEEN 0 AND 1000),
    state TEXT NOT NULL CHECK(state IN (
        'queued', 'delayed', 'leased', 'starting', 'running', 'committing',
        'cancel_requested', 'succeeded', 'partially_succeeded',
        'failed_terminal', 'cancelled'
    )),
    available_at_ms INTEGER NOT NULL CHECK(available_at_ms >= 0),
    deadline_at_ms INTEGER CHECK(deadline_at_ms IS NULL OR deadline_at_ms >= available_at_ms),
    attempt_limit INTEGER NOT NULL CHECK(attempt_limit > 0),
    current_attempt_no INTEGER NOT NULL DEFAULT 0 CHECK(current_attempt_no >= 0),
    fencing_token INTEGER NOT NULL DEFAULT 0 CHECK(fencing_token >= 0),
    lease_owner TEXT,
    lease_boot_id TEXT,
    lease_expires_at_ms INTEGER CHECK(lease_expires_at_ms IS NULL OR lease_expires_at_ms >= 0),
    heartbeat_at_ms INTEGER CHECK(heartbeat_at_ms IS NULL OR heartbeat_at_ms >= 0),
    wait_reason TEXT,
    cancellation_requested_at_ms INTEGER CHECK(cancellation_requested_at_ms IS NULL OR cancellation_requested_at_ms >= 0),
    terminal_at_ms INTEGER CHECK(terminal_at_ms IS NULL OR terminal_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    CHECK(
        (backup_run_id IS NOT NULL) +
        (restore_run_id IS NOT NULL) +
        (verification_run_id IS NOT NULL) +
        (catalog_rebuild_run_id IS NOT NULL) +
        (operation_run_id IS NOT NULL) = 1
    ),
    CHECK(
        (kind = 'backup' AND backup_run_id IS NOT NULL)
        OR (kind = 'restore' AND restore_run_id IS NOT NULL)
        OR (kind = 'verification' AND verification_run_id IS NOT NULL)
        OR (kind = 'catalog_rebuild' AND catalog_rebuild_run_id IS NOT NULL)
        OR (kind = 'operation' AND operation_run_id IS NOT NULL)
    ),
    CHECK(
        (state IN ('leased', 'starting', 'running', 'committing', 'cancel_requested')
            AND lease_owner IS NOT NULL AND lease_expires_at_ms IS NOT NULL)
        OR
        (state NOT IN ('leased', 'starting', 'running', 'committing', 'cancel_requested'))
    )
) STRICT;

CREATE UNIQUE INDEX idx_work_items_backup_run
    ON work_items(backup_run_id)
    WHERE backup_run_id IS NOT NULL;

CREATE UNIQUE INDEX idx_work_items_restore_run
    ON work_items(restore_run_id)
    WHERE restore_run_id IS NOT NULL;

CREATE UNIQUE INDEX idx_work_items_verification_run
    ON work_items(verification_run_id)
    WHERE verification_run_id IS NOT NULL;

CREATE UNIQUE INDEX idx_work_items_catalog_rebuild_run
    ON work_items(catalog_rebuild_run_id)
    WHERE catalog_rebuild_run_id IS NOT NULL;

CREATE UNIQUE INDEX idx_work_items_operation_run
    ON work_items(operation_run_id)
    WHERE operation_run_id IS NOT NULL;

CREATE INDEX idx_work_items_claim
    ON work_items(priority DESC, available_at_ms, created_at_ms)
    WHERE state IN ('queued', 'delayed');

CREATE INDEX idx_work_items_active_lease
    ON work_items(lease_expires_at_ms)
    WHERE state IN ('leased', 'starting', 'running', 'committing', 'cancel_requested');

CREATE TABLE work_attempts (
    id TEXT PRIMARY KEY,
    work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
    attempt_no INTEGER NOT NULL CHECK(attempt_no > 0),
    fencing_token INTEGER NOT NULL CHECK(fencing_token > 0),
    worker_id TEXT NOT NULL,
    worker_boot_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN (
        'claimed', 'starting', 'running', 'committing',
        'succeeded', 'failed', 'cancelled', 'abandoned'
    )),
    lease_acquired_at_ms INTEGER NOT NULL CHECK(lease_acquired_at_ms >= 0),
    last_heartbeat_at_ms INTEGER NOT NULL CHECK(last_heartbeat_at_ms >= lease_acquired_at_ms),
    lease_expires_at_ms INTEGER NOT NULL CHECK(lease_expires_at_ms >= last_heartbeat_at_ms),
    started_at_ms INTEGER CHECK(started_at_ms IS NULL OR started_at_ms >= lease_acquired_at_ms),
    completed_at_ms INTEGER CHECK(
        completed_at_ms IS NULL OR started_at_ms IS NULL OR completed_at_ms >= started_at_ms
    ),
    retry_class TEXT CHECK(retry_class IS NULL OR retry_class IN ('retryable', 'terminal', 'cancelled', 'reconcile')),
    retry_after_ms INTEGER CHECK(retry_after_ms IS NULL OR retry_after_ms >= 0),
    error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    UNIQUE(work_item_id, attempt_no),
    UNIQUE(work_item_id, fencing_token)
) STRICT;

CREATE INDEX idx_work_attempts_item_time
    ON work_attempts(work_item_id, attempt_no DESC);

CREATE INDEX idx_work_attempts_active_lease
    ON work_attempts(lease_expires_at_ms)
    WHERE state IN ('claimed', 'starting', 'running', 'committing');

CREATE TABLE resource_pools (
    id TEXT PRIMARY KEY,
    resource_type TEXT NOT NULL CHECK(resource_type IN (
        'global_job', 'source', 'cpu_encode', 'destination', 'restore', 'database_exclusive'
    )),
    resource_key TEXT NOT NULL,
    capacity INTEGER NOT NULL CHECK(capacity > 0),
    enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    UNIQUE(resource_type, resource_key)
) STRICT;

CREATE TABLE resource_allocations (
    id TEXT PRIMARY KEY,
    resource_pool_id TEXT NOT NULL REFERENCES resource_pools(id) ON DELETE RESTRICT,
    work_attempt_id TEXT NOT NULL REFERENCES work_attempts(id) ON DELETE RESTRICT,
    units INTEGER NOT NULL DEFAULT 1 CHECK(units > 0),
    fencing_token INTEGER NOT NULL CHECK(fencing_token > 0),
    acquired_at_ms INTEGER NOT NULL CHECK(acquired_at_ms >= 0),
    expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms >= acquired_at_ms),
    released_at_ms INTEGER CHECK(released_at_ms IS NULL OR released_at_ms >= acquired_at_ms),
    release_reason TEXT CHECK(release_reason IS NULL OR release_reason IN (
        'completed', 'failed', 'cancelled', 'lease_expired', 'reconciled'
    )),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
) STRICT;

CREATE UNIQUE INDEX idx_resource_allocations_active_attempt_pool
    ON resource_allocations(work_attempt_id, resource_pool_id)
    WHERE released_at_ms IS NULL;

CREATE INDEX idx_resource_allocations_active_pool
    ON resource_allocations(resource_pool_id, expires_at_ms)
    WHERE released_at_ms IS NULL;

CREATE TABLE spool_volumes (
    id TEXT PRIMARY KEY,
    volume_key TEXT NOT NULL UNIQUE,
    root_path TEXT NOT NULL,
    hard_limit_bytes INTEGER NOT NULL CHECK(hard_limit_bytes > 0),
    soft_limit_bytes INTEGER NOT NULL CHECK(soft_limit_bytes > 0 AND soft_limit_bytes <= hard_limit_bytes),
    reserve_floor_bytes INTEGER NOT NULL DEFAULT 0 CHECK(reserve_floor_bytes >= 0),
    state TEXT NOT NULL CHECK(state IN ('ready', 'throttled', 'full', 'offline')),
    last_free_bytes INTEGER CHECK(last_free_bytes IS NULL OR last_free_bytes >= 0),
    last_probed_at_ms INTEGER CHECK(last_probed_at_ms IS NULL OR last_probed_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE TABLE spool_reservations (
    id TEXT PRIMARY KEY,
    spool_volume_id TEXT NOT NULL REFERENCES spool_volumes(id) ON DELETE RESTRICT,
    work_attempt_id TEXT NOT NULL REFERENCES work_attempts(id) ON DELETE RESTRICT,
    reserved_bytes INTEGER NOT NULL CHECK(reserved_bytes > 0),
    actual_bytes INTEGER CHECK(actual_bytes IS NULL OR actual_bytes >= 0),
    fencing_token INTEGER NOT NULL CHECK(fencing_token > 0),
    spool_path TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('reserved', 'materializing', 'ready', 'quarantined', 'released')),
    acquired_at_ms INTEGER NOT NULL CHECK(acquired_at_ms >= 0),
    released_at_ms INTEGER CHECK(released_at_ms IS NULL OR released_at_ms >= acquired_at_ms),
    cleanup_error TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
) STRICT;

CREATE UNIQUE INDEX idx_spool_reservations_active_attempt
    ON spool_reservations(work_attempt_id)
    WHERE released_at_ms IS NULL;

CREATE INDEX idx_spool_reservations_active_volume
    ON spool_reservations(spool_volume_id, acquired_at_ms)
    WHERE released_at_ms IS NULL;

CREATE TABLE process_instances (
    id TEXT PRIMARY KEY,
    work_attempt_id TEXT NOT NULL REFERENCES work_attempts(id) ON DELETE RESTRICT,
    phase TEXT NOT NULL,
    process_id INTEGER NOT NULL CHECK(process_id > 0),
    process_start_time_ms INTEGER NOT NULL CHECK(process_start_time_ms >= 0),
    service_boot_id TEXT NOT NULL,
    job_object_key TEXT NOT NULL,
    executable_path TEXT NOT NULL,
    executable_sha256 TEXT CHECK(executable_sha256 IS NULL OR length(executable_sha256) = 64),
    state TEXT NOT NULL CHECK(state IN ('starting', 'running', 'exited', 'killed', 'orphaned')),
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
    ended_at_ms INTEGER CHECK(ended_at_ms IS NULL OR ended_at_ms >= started_at_ms),
    exit_code INTEGER,
    termination_reason TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    UNIQUE(service_boot_id, process_id, process_start_time_ms)
) STRICT;

CREATE INDEX idx_process_instances_attempt
    ON process_instances(work_attempt_id, started_at_ms);

CREATE INDEX idx_process_instances_active
    ON process_instances(state, service_boot_id)
    WHERE state IN ('starting', 'running', 'orphaned');

-- ---------------------------------------------------------------------------
-- Activity and operator attention
-- ---------------------------------------------------------------------------

CREATE TABLE activity_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    occurred_at_ms INTEGER NOT NULL CHECK(occurred_at_ms >= 0),
    severity TEXT NOT NULL CHECK(severity IN ('debug', 'info', 'warning', 'error', 'critical')),
    event_code TEXT NOT NULL,
    phase TEXT,
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    correlation_id TEXT,
    work_item_id TEXT REFERENCES work_items(id) ON DELETE SET NULL,
    attempt_id TEXT REFERENCES work_attempts(id) ON DELETE SET NULL,
    retryability TEXT CHECK(retryability IS NULL OR retryability IN ('retryable', 'terminal', 'unknown')),
    message TEXT NOT NULL,
    attributes_json TEXT NOT NULL CHECK(json_valid(attributes_json)),
    redaction_version INTEGER NOT NULL CHECK(redaction_version > 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0)
) STRICT;

CREATE INDEX idx_activity_events_time
    ON activity_events(occurred_at_ms DESC, seq DESC);

CREATE INDEX idx_activity_events_entity_time
    ON activity_events(entity_type, entity_id, occurred_at_ms DESC);

CREATE INDEX idx_activity_events_work_item
    ON activity_events(work_item_id, occurred_at_ms)
    WHERE work_item_id IS NOT NULL;

CREATE INDEX idx_activity_events_error_time
    ON activity_events(severity, occurred_at_ms DESC)
    WHERE severity IN ('error', 'critical');

CREATE TABLE attention_states (
    id TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    category TEXT NOT NULL CHECK(category IN (
        'run_failure', 'rpo_violation', 'verification_overdue', 'discovery_drift',
        'stale_lease', 'destination_health', 'tool_health', 'notification_failure',
        'control_store_health', 'protection_compliance'
    )),
    severity TEXT NOT NULL CHECK(severity IN ('warning', 'error', 'critical')),
    entity_type TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('open', 'acknowledged', 'resolved')),
    summary TEXT NOT NULL,
    first_seen_at_ms INTEGER NOT NULL CHECK(first_seen_at_ms >= 0),
    last_seen_at_ms INTEGER NOT NULL CHECK(last_seen_at_ms >= first_seen_at_ms),
    occurrence_count INTEGER NOT NULL DEFAULT 1 CHECK(occurrence_count > 0),
    acknowledged_at_ms INTEGER CHECK(acknowledged_at_ms IS NULL OR acknowledged_at_ms >= first_seen_at_ms),
    resolved_at_ms INTEGER CHECK(resolved_at_ms IS NULL OR resolved_at_ms >= first_seen_at_ms),
    resolution_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE UNIQUE INDEX idx_attention_states_active_fingerprint
    ON attention_states(fingerprint)
    WHERE state IN ('open', 'acknowledged');

CREATE INDEX idx_attention_states_open
    ON attention_states(severity, last_seen_at_ms DESC)
    WHERE state IN ('open', 'acknowledged');

CREATE INDEX idx_attention_states_entity
    ON attention_states(entity_type, entity_id, state);

CREATE TABLE attention_event_links (
    attention_state_id TEXT NOT NULL REFERENCES attention_states(id) ON DELETE CASCADE,
    activity_event_seq INTEGER NOT NULL REFERENCES activity_events(seq) ON DELETE RESTRICT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (attention_state_id, activity_event_seq)
) STRICT;

CREATE INDEX idx_attention_event_links_event
    ON attention_event_links(activity_event_seq);

-- ---------------------------------------------------------------------------
-- Notification routing and transactional outbox
-- ---------------------------------------------------------------------------

CREATE TABLE notification_channels (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('email', 'webhook', 'slack', 'teams', 'discord')),
    config_json TEXT NOT NULL CHECK(json_valid(config_json)),
    state TEXT NOT NULL CHECK(state IN ('draft', 'ready', 'disabled', 'attention')),
    last_probe_status TEXT CHECK(last_probe_status IS NULL OR last_probe_status IN ('passed', 'failed', 'cancelled')),
    last_probe_at_ms INTEGER CHECK(last_probe_at_ms IS NULL OR last_probe_at_ms >= 0),
    deleted_at_ms INTEGER CHECK(deleted_at_ms IS NULL OR deleted_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE UNIQUE INDEX idx_notification_channels_active_name
    ON notification_channels(display_name)
    WHERE deleted_at_ms IS NULL;

CREATE TABLE notification_channel_secret_bindings (
    notification_channel_id TEXT NOT NULL REFERENCES notification_channels(id) ON DELETE CASCADE,
    secret_role TEXT NOT NULL CHECK(secret_role IN ('authentication', 'signing')),
    secret_ref_id TEXT NOT NULL REFERENCES secret_refs(id) ON DELETE RESTRICT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (notification_channel_id, secret_role)
) STRICT;

CREATE INDEX idx_notification_channel_secrets_secret
    ON notification_channel_secret_bindings(secret_ref_id);

CREATE TABLE notification_rules (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    notification_channel_id TEXT NOT NULL REFERENCES notification_channels(id) ON DELETE RESTRICT,
    enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
    event_filter_json TEXT NOT NULL CHECK(json_valid(event_filter_json)),
    severity_threshold TEXT NOT NULL CHECK(severity_threshold IN ('info', 'warning', 'error', 'critical')),
    cooldown_ms INTEGER NOT NULL DEFAULT 0 CHECK(cooldown_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms)
) STRICT;

CREATE INDEX idx_notification_rules_channel
    ON notification_rules(notification_channel_id, enabled);

CREATE TABLE plan_notification_rules (
    backup_plan_id TEXT NOT NULL REFERENCES backup_plans(id) ON DELETE CASCADE,
    notification_rule_id TEXT NOT NULL REFERENCES notification_rules(id) ON DELETE RESTRICT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    PRIMARY KEY (backup_plan_id, notification_rule_id)
) STRICT;

CREATE INDEX idx_plan_notification_rules_rule
    ON plan_notification_rules(notification_rule_id);

CREATE TABLE notification_outbox (
    id TEXT PRIMARY KEY,
    notification_rule_id TEXT NOT NULL REFERENCES notification_rules(id) ON DELETE RESTRICT,
    activity_event_seq INTEGER REFERENCES activity_events(seq) ON DELETE SET NULL,
    deduplication_key TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending', 'delivering', 'delivered', 'failed_terminal', 'cancelled')),
    payload_json TEXT NOT NULL CHECK(json_valid(payload_json)),
    available_at_ms INTEGER NOT NULL CHECK(available_at_ms >= 0),
    attempt_limit INTEGER NOT NULL CHECK(attempt_limit > 0),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
    fencing_token INTEGER NOT NULL DEFAULT 0 CHECK(fencing_token >= 0),
    lease_owner TEXT,
    lease_boot_id TEXT,
    heartbeat_at_ms INTEGER CHECK(heartbeat_at_ms IS NULL OR heartbeat_at_ms >= 0),
    lease_expires_at_ms INTEGER CHECK(lease_expires_at_ms IS NULL OR lease_expires_at_ms >= 0),
    delivered_at_ms INTEGER CHECK(delivered_at_ms IS NULL OR delivered_at_ms >= available_at_ms),
    last_error_code TEXT,
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms >= created_at_ms),
    UNIQUE(notification_rule_id, deduplication_key),
    CHECK(
        (state = 'delivering' AND lease_owner IS NOT NULL AND lease_expires_at_ms IS NOT NULL)
        OR
        (state <> 'delivering' AND lease_owner IS NULL AND lease_expires_at_ms IS NULL)
    )
) STRICT;

CREATE INDEX idx_notification_outbox_delivery
    ON notification_outbox(available_at_ms, created_at_ms)
    WHERE state = 'pending';

CREATE INDEX idx_notification_outbox_active_lease
    ON notification_outbox(lease_expires_at_ms)
    WHERE state = 'delivering';

CREATE TABLE notification_delivery_attempts (
    id TEXT PRIMARY KEY,
    outbox_id TEXT NOT NULL REFERENCES notification_outbox(id) ON DELETE RESTRICT,
    attempt_no INTEGER NOT NULL CHECK(attempt_no > 0),
    state TEXT NOT NULL CHECK(state IN ('running', 'delivered', 'failed', 'cancelled')),
    response_code TEXT,
    response_summary TEXT,
    started_at_ms INTEGER NOT NULL CHECK(started_at_ms >= 0),
    completed_at_ms INTEGER CHECK(completed_at_ms IS NULL OR completed_at_ms >= started_at_ms),
    next_retry_at_ms INTEGER CHECK(next_retry_at_ms IS NULL OR next_retry_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    UNIQUE(outbox_id, attempt_no)
) STRICT;

CREATE INDEX idx_notification_attempts_outbox
    ON notification_delivery_attempts(outbox_id, attempt_no DESC);

-- ---------------------------------------------------------------------------
-- Rebuildable Catalog Projection
-- These tables may be dropped and rebuilt from signed Manifests.
-- No authoritative control table has a foreign key to this section.
-- ---------------------------------------------------------------------------

CREATE TABLE catalog_recovery_points (
    id TEXT PRIMARY KEY,
    origin_instance_id TEXT NOT NULL,
    origin_occurrence_id TEXT,
    origin_backup_run_id TEXT,
    source_target_identity TEXT NOT NULL,
    source_display_name TEXT NOT NULL,
    engine TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('complete', 'incomplete', 'orphaned', 'quarantined')),
    consistency_grade TEXT NOT NULL,
    committed_at_ms INTEGER CHECK(committed_at_ms IS NULL OR committed_at_ms >= 0),
    manifest_schema_version INTEGER NOT NULL CHECK(manifest_schema_version > 0),
    set_manifest_checksum_blake3 TEXT NOT NULL CHECK(length(set_manifest_checksum_blake3) = 64),
    signing_key_id TEXT NOT NULL,
    signature_status TEXT NOT NULL CHECK(signature_status IN ('valid', 'invalid', 'unknown_key', 'not_checked')),
    required_part_count INTEGER NOT NULL CHECK(required_part_count >= 0),
    present_part_count INTEGER NOT NULL CHECK(present_part_count >= 0),
    plaintext_bytes INTEGER CHECK(plaintext_bytes IS NULL OR plaintext_bytes >= 0),
    cipher_bytes INTEGER CHECK(cipher_bytes IS NULL OR cipher_bytes >= 0),
    engine_metadata_json TEXT NOT NULL CHECK(json_valid(engine_metadata_json)),
    first_ingested_at_ms INTEGER NOT NULL CHECK(first_ingested_at_ms >= 0),
    last_ingested_at_ms INTEGER NOT NULL CHECK(last_ingested_at_ms >= first_ingested_at_ms),
    CHECK(state <> 'complete' OR (
        committed_at_ms IS NOT NULL
        AND signature_status = 'valid'
        AND required_part_count > 0
        AND present_part_count >= required_part_count
    ))
) STRICT;

CREATE UNIQUE INDEX idx_catalog_recovery_origin
    ON catalog_recovery_points(origin_instance_id, origin_occurrence_id, source_target_identity)
    WHERE origin_occurrence_id IS NOT NULL AND state = 'complete';

CREATE INDEX idx_catalog_recovery_target_time
    ON catalog_recovery_points(source_target_identity, committed_at_ms DESC);

CREATE INDEX idx_catalog_recovery_engine_time
    ON catalog_recovery_points(engine, committed_at_ms DESC);

CREATE TABLE catalog_artifact_parts (
    id TEXT PRIMARY KEY,
    recovery_point_id TEXT NOT NULL REFERENCES catalog_recovery_points(id) ON DELETE CASCADE,
    part_role TEXT NOT NULL CHECK(part_role IN (
        'database_dump', 'globals', 'metadata', 'wal', 'auxiliary'
    )),
    logical_name TEXT NOT NULL,
    required INTEGER NOT NULL CHECK(required IN (0, 1)),
    dump_format TEXT NOT NULL,
    compression_format TEXT NOT NULL,
    encryption_format TEXT NOT NULL,
    plaintext_checksum_blake3 TEXT CHECK(
        plaintext_checksum_blake3 IS NULL OR length(plaintext_checksum_blake3) = 64
    ),
    plaintext_bytes INTEGER CHECK(plaintext_bytes IS NULL OR plaintext_bytes >= 0),
    cipher_bytes INTEGER CHECK(cipher_bytes IS NULL OR cipher_bytes >= 0),
    metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    UNIQUE(recovery_point_id, part_role, logical_name),
    UNIQUE(id, recovery_point_id)
) STRICT;

CREATE INDEX idx_catalog_parts_recovery
    ON catalog_artifact_parts(recovery_point_id, required);

CREATE TABLE catalog_artifact_replicas (
    id TEXT PRIMARY KEY,
    recovery_point_id TEXT NOT NULL REFERENCES catalog_recovery_points(id) ON DELETE CASCADE,
    origin_destination_id TEXT,
    destination_fingerprint TEXT NOT NULL,
    destination_kind TEXT NOT NULL,
    requirement TEXT NOT NULL CHECK(requirement IN ('required', 'best_effort', 'unknown')),
    state TEXT NOT NULL CHECK(state IN ('committed', 'degraded', 'missing', 'corrupt', 'quarantined')),
    immutability_capability TEXT NOT NULL CHECK(immutability_capability IN (
        'none', 'governance', 'compliance', 'server_append_only', 'unknown'
    )),
    retention_until_ms INTEGER CHECK(retention_until_ms IS NULL OR retention_until_ms >= 0),
    reader_status TEXT NOT NULL CHECK(reader_status IN ('eligible', 'unavailable', 'unverified')),
    committed_at_ms INTEGER CHECK(committed_at_ms IS NULL OR committed_at_ms >= 0),
    last_observed_at_ms INTEGER NOT NULL CHECK(last_observed_at_ms >= 0),
    metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
    UNIQUE(recovery_point_id, destination_fingerprint),
    UNIQUE(id, recovery_point_id)
) STRICT;

CREATE INDEX idx_catalog_replicas_recovery_state
    ON catalog_artifact_replicas(recovery_point_id, state);

CREATE INDEX idx_catalog_replicas_destination_time
    ON catalog_artifact_replicas(destination_fingerprint, committed_at_ms DESC);

CREATE TABLE catalog_stored_objects (
    id TEXT PRIMARY KEY,
    recovery_point_id TEXT NOT NULL REFERENCES catalog_recovery_points(id) ON DELETE CASCADE,
    artifact_replica_id TEXT NOT NULL,
    artifact_part_id TEXT,
    object_role TEXT NOT NULL CHECK(object_role IN (
        'payload', 'part_manifest', 'set_manifest', 'verification_evidence'
    )),
    object_key TEXT NOT NULL,
    object_version TEXT,
    checksum_blake3 TEXT CHECK(checksum_blake3 IS NULL OR length(checksum_blake3) = 64),
    size_bytes INTEGER CHECK(size_bytes IS NULL OR size_bytes >= 0),
    state TEXT NOT NULL CHECK(state IN ('present', 'missing', 'corrupt', 'quarantined', 'deleted')),
    observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms >= 0),
    metadata_json TEXT NOT NULL CHECK(json_valid(metadata_json)),
    UNIQUE(artifact_replica_id, object_key, object_version),
    CHECK(object_role IN ('set_manifest', 'verification_evidence') OR artifact_part_id IS NOT NULL),
    FOREIGN KEY (artifact_replica_id, recovery_point_id)
        REFERENCES catalog_artifact_replicas(id, recovery_point_id) ON DELETE CASCADE,
    FOREIGN KEY (artifact_part_id, recovery_point_id)
        REFERENCES catalog_artifact_parts(id, recovery_point_id) ON DELETE CASCADE
) STRICT;

CREATE INDEX idx_catalog_objects_replica_role
    ON catalog_stored_objects(artifact_replica_id, object_role, state);

CREATE UNIQUE INDEX idx_catalog_objects_unversioned_key
    ON catalog_stored_objects(artifact_replica_id, object_key)
    WHERE object_version IS NULL;

CREATE INDEX idx_catalog_objects_part
    ON catalog_stored_objects(artifact_part_id)
    WHERE artifact_part_id IS NOT NULL;

CREATE TABLE catalog_snapshot_projection (
    recovery_point_id TEXT PRIMARY KEY REFERENCES catalog_recovery_points(id) ON DELETE CASCADE,
    source_target_identity TEXT NOT NULL,
    source_display_name TEXT NOT NULL,
    engine TEXT NOT NULL,
    committed_at_ms INTEGER NOT NULL CHECK(committed_at_ms >= 0),
    artifact_completeness TEXT NOT NULL CHECK(artifact_completeness IN ('complete', 'incomplete')),
    replica_durability TEXT NOT NULL CHECK(replica_durability IN ('healthy', 'degraded', 'unavailable')),
    protection_compliance TEXT NOT NULL CHECK(protection_compliance IN (
        'compliant', 'non_compliant', 'unknown'
    )),
    restore_eligibility TEXT NOT NULL CHECK(restore_eligibility IN ('eligible', 'conditional', 'blocked')),
    highest_verified_level INTEGER CHECK(highest_verified_level IS NULL OR highest_verified_level BETWEEN 1 AND 3),
    last_verified_at_ms INTEGER CHECK(last_verified_at_ms IS NULL OR last_verified_at_ms >= 0),
    search_text TEXT NOT NULL,
    projection_version INTEGER NOT NULL CHECK(projection_version > 0),
    projected_at_ms INTEGER NOT NULL CHECK(projected_at_ms >= 0)
) STRICT;

CREATE INDEX idx_catalog_snapshots_target_time
    ON catalog_snapshot_projection(source_target_identity, committed_at_ms DESC);

CREATE INDEX idx_catalog_snapshots_engine_time
    ON catalog_snapshot_projection(engine, committed_at_ms DESC);

CREATE INDEX idx_catalog_snapshots_restore
    ON catalog_snapshot_projection(restore_eligibility, committed_at_ms DESC);

CREATE TABLE catalog_target_protection_projection (
    source_target_identity TEXT PRIMARY KEY,
    local_database_target_id TEXT,
    latest_recovery_point_id TEXT REFERENCES catalog_recovery_points(id) ON DELETE SET NULL,
    latest_success_at_ms INTEGER CHECK(latest_success_at_ms IS NULL OR latest_success_at_ms >= 0),
    latest_failure_at_ms INTEGER CHECK(latest_failure_at_ms IS NULL OR latest_failure_at_ms >= 0),
    artifact_completeness TEXT NOT NULL CHECK(artifact_completeness IN ('complete', 'incomplete', 'unknown')),
    replica_durability TEXT NOT NULL CHECK(replica_durability IN ('healthy', 'degraded', 'unavailable', 'unknown')),
    protection_compliance TEXT NOT NULL CHECK(protection_compliance IN (
        'compliant', 'non_compliant', 'not_configured', 'unknown'
    )),
    rpo_violation INTEGER NOT NULL CHECK(rpo_violation IN (0, 1)),
    verification_overdue INTEGER NOT NULL CHECK(verification_overdue IN (0, 1)),
    reasons_json TEXT NOT NULL CHECK(json_valid(reasons_json)),
    projection_version INTEGER NOT NULL CHECK(projection_version > 0),
    projected_at_ms INTEGER NOT NULL CHECK(projected_at_ms >= 0)
) STRICT;

CREATE INDEX idx_catalog_target_protection_attention
    ON catalog_target_protection_projection(protection_compliance, rpo_violation, verification_overdue);

CREATE TABLE catalog_verification_projection (
    id TEXT PRIMARY KEY,
    recovery_point_id TEXT NOT NULL REFERENCES catalog_recovery_points(id) ON DELETE CASCADE,
    origin_verification_run_id TEXT,
    verify_level INTEGER NOT NULL CHECK(verify_level BETWEEN 1 AND 3),
    result TEXT NOT NULL CHECK(result IN ('passed', 'failed', 'unsupported', 'cancelled')),
    verified_at_ms INTEGER NOT NULL CHECK(verified_at_ms >= 0),
    tool_name TEXT,
    tool_version TEXT,
    evidence_checksum_blake3 TEXT CHECK(
        evidence_checksum_blake3 IS NULL OR length(evidence_checksum_blake3) = 64
    ),
    evidence_locator TEXT,
    evidence_json TEXT NOT NULL CHECK(json_valid(evidence_json)),
    ingested_at_ms INTEGER NOT NULL CHECK(ingested_at_ms >= 0)
) STRICT;

CREATE INDEX idx_catalog_verification_recovery_level
    ON catalog_verification_projection(recovery_point_id, verify_level DESC, verified_at_ms DESC);

CREATE TABLE catalog_manifest_ingest (
    id TEXT PRIMARY KEY,
    catalog_rebuild_run_id TEXT REFERENCES catalog_rebuild_runs(id) ON DELETE SET NULL,
    source_kind TEXT NOT NULL,
    source_locator TEXT NOT NULL,
    manifest_object_key TEXT NOT NULL,
    manifest_checksum_blake3 TEXT CHECK(
        manifest_checksum_blake3 IS NULL OR length(manifest_checksum_blake3) = 64
    ),
    result TEXT NOT NULL CHECK(result IN (
        'accepted', 'duplicate', 'invalid_signature', 'unknown_key',
        'invalid_schema', 'malformed', 'read_failed'
    )),
    recovery_point_id TEXT REFERENCES catalog_recovery_points(id) ON DELETE SET NULL,
    diagnostic_code TEXT,
    observed_at_ms INTEGER NOT NULL CHECK(observed_at_ms >= 0),
    created_at_ms INTEGER NOT NULL CHECK(created_at_ms >= 0),
    UNIQUE(source_kind, source_locator, manifest_object_key, manifest_checksum_blake3)
) STRICT;

CREATE INDEX idx_catalog_manifest_ingest_rebuild
    ON catalog_manifest_ingest(catalog_rebuild_run_id, result)
    WHERE catalog_rebuild_run_id IS NOT NULL;

CREATE INDEX idx_catalog_manifest_ingest_result_time
    ON catalog_manifest_ingest(result, observed_at_ms DESC);

-- Cutover recovery is monotonic. The cutover transaction creates
-- app_meta('production_epoch.v1') as rollback_eligible. The first durable
-- production mutation changes it once to forward_fix_required:* in the same
-- transaction as the accepted state change. It never transitions back.
CREATE TRIGGER trg_production_epoch_ipc_completion
AFTER UPDATE OF state ON ipc_request_ledger
WHEN NEW.state = 'succeeded'
 AND NEW.command_name NOT IN ('confirmation.prepare', 'config.cutover')
 AND EXISTS (
     SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1'
 )
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:ipc:' || NEW.command_name,
        updated_at_ms = COALESCE(NEW.completed_at_ms, NEW.accepted_at_ms)
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

CREATE TRIGGER trg_production_epoch_forward_fix_terminal
BEFORE UPDATE ON app_meta
WHEN OLD.key = 'production_epoch.v1'
 AND OLD.value_text LIKE 'forward_fix_required:%'
 AND (
     NEW.key <> OLD.key
     OR NEW.value_text <> OLD.value_text
     OR NEW.updated_at_ms <> OLD.updated_at_ms
 )
BEGIN
    SELECT RAISE(ABORT, 'production epoch forward-fix state is terminal');
END;

CREATE TRIGGER trg_production_epoch_delete_requires_authority_removal
BEFORE DELETE ON app_meta
WHEN OLD.key = 'production_epoch.v1'
 AND EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    SELECT RAISE(ABORT, 'production epoch cannot be deleted while authority exists');
END;

CREATE TRIGGER trg_production_epoch_plan_occurrence
AFTER INSERT ON plan_occurrences
WHEN EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:table:plan_occurrences',
        updated_at_ms = NEW.created_at_ms
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

CREATE TRIGGER trg_production_epoch_operation_run
AFTER INSERT ON operation_runs
WHEN EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:table:operation_runs',
        updated_at_ms = NEW.created_at_ms
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

CREATE TRIGGER trg_production_epoch_backup_run
AFTER INSERT ON backup_runs
WHEN EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:table:backup_runs',
        updated_at_ms = NEW.created_at_ms
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

CREATE TRIGGER trg_production_epoch_restore_run
AFTER INSERT ON restore_runs
WHEN EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:table:restore_runs',
        updated_at_ms = NEW.created_at_ms
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

CREATE TRIGGER trg_production_epoch_verification_run
AFTER INSERT ON verification_runs
WHEN EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:table:verification_runs',
        updated_at_ms = NEW.created_at_ms
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

CREATE TRIGGER trg_production_epoch_work_item
AFTER INSERT ON work_items
WHEN EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:table:work_items',
        updated_at_ms = NEW.created_at_ms
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

CREATE TRIGGER trg_production_epoch_catalog_rebuild
AFTER INSERT ON catalog_rebuild_runs
WHEN EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:table:catalog_rebuild_runs',
        updated_at_ms = NEW.created_at_ms
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

CREATE TRIGGER trg_production_epoch_control_backup
AFTER INSERT ON control_store_backups
WHEN EXISTS (SELECT 1 FROM app_meta WHERE key = 'execution_authority.v1')
BEGIN
    UPDATE app_meta
    SET value_text = 'forward_fix_required:table:control_store_backups',
        updated_at_ms = NEW.created_at_ms
    WHERE key = 'production_epoch.v1'
      AND value_text = 'rollback_eligible';
END;

PRAGMA user_version = 2;

COMMIT;
