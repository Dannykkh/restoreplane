-- Extracted canonical DDL from db-schema.md; edit that document then regenerate.
PRAGMA foreign_keys = ON;
PRAGMA trusted_schema = OFF;
PRAGMA journal_mode = WAL;
PRAGMA synchronous = FULL;
BEGIN IMMEDIATE;
CREATE TABLE journal_header (
 singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema_version INTEGER NOT NULL CHECK(schema_version=1),
 owner_id TEXT NOT NULL UNIQUE CHECK(length(owner_id)=64), journal_epoch TEXT NOT NULL UNIQUE CHECK(length(journal_epoch)=36),
 storage_revision INTEGER NOT NULL DEFAULT 0 CHECK(storage_revision>=0),
 created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0), updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=created_at_ms)
) STRICT;
CREATE TABLE target_bindings (
 owner_id TEXT NOT NULL REFERENCES journal_header(owner_id) ON DELETE RESTRICT,
 workspace_id TEXT NOT NULL CHECK(length(workspace_id) BETWEEN 1 AND 128), target_id TEXT NOT NULL CHECK(length(target_id) BETWEEN 1 AND 128),
 binding_id TEXT NOT NULL CHECK(length(binding_id)=36), connection_profile_id TEXT NOT NULL CHECK(length(connection_profile_id) BETWEEN 1 AND 128),
 descriptor_revision TEXT NOT NULL CHECK(length(descriptor_revision)=64), authority_epoch TEXT NOT NULL CHECK(length(authority_epoch) BETWEEN 1 AND 128),
 source_identity_digest TEXT NOT NULL CHECK(length(source_identity_digest)=64), engine TEXT NOT NULL CHECK(engine IN('postgresql','mysql','mariadb','sqlite')),
 credential_ref TEXT CHECK(credential_ref IS NULL OR length(credential_ref)=36), state TEXT NOT NULL CHECK(state IN('active','retired')),
 created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0), updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=created_at_ms),
 PRIMARY KEY(owner_id,workspace_id,target_id,binding_id), UNIQUE(owner_id,workspace_id,target_id,descriptor_revision,authority_epoch)
) STRICT;
CREATE TABLE operations (
 owner_id TEXT NOT NULL, workspace_id TEXT NOT NULL, target_id TEXT NOT NULL, binding_id TEXT NOT NULL,
 operation_id TEXT NOT NULL CHECK(length(operation_id)=36), kind TEXT NOT NULL CHECK(kind IN('query','manual_transaction','verified_apply','import_job','import_batch','export_job','protection_link')),
 idempotency_key TEXT NOT NULL CHECK(length(idempotency_key) BETWEEN 1 AND 256), payload_digest TEXT NOT NULL CHECK(length(payload_digest)=64),
 journal_epoch TEXT NOT NULL CHECK(length(journal_epoch)=36),
 process_boot_epoch TEXT NOT NULL CHECK(length(process_boot_epoch)=36), window_instance_id TEXT NOT NULL CHECK(length(window_instance_id)=36),
 session_id TEXT CHECK(session_id IS NULL OR length(session_id)=36), connection_generation INTEGER NOT NULL DEFAULT 0 CHECK(connection_generation>=0), execution_epoch INTEGER NOT NULL DEFAULT 0 CHECK(execution_epoch>=0),
 transaction_operation_id TEXT CHECK(transaction_operation_id IS NULL OR length(transaction_operation_id)=36), transaction_kind TEXT CHECK(transaction_kind IS NULL OR transaction_kind='manual_transaction'),
 parent_operation_id TEXT, parent_kind TEXT CHECK(parent_kind IS NULL OR parent_kind='import_job'), batch_index INTEGER,
 source_digest TEXT CHECK(source_digest IS NULL OR length(source_digest)=64), batch_digest TEXT CHECK(batch_digest IS NULL OR length(batch_digest)=64),
 source_byte_start INTEGER, source_byte_end INTEGER, source_row_start INTEGER, source_row_end INTEGER,
 state TEXT NOT NULL CHECK(state IN('accepted','dispatching','running','commit_requested','committed','rolled_back','failed','outcome_unknown','completed','cancelled','interrupted','partial')),
 transaction_outcome TEXT NOT NULL DEFAULT 'none' CHECK(transaction_outcome IN('none','open','commit_requested','committed','rolled_back','outcome_unknown')),
 result_completeness TEXT NOT NULL DEFAULT 'none' CHECK(result_completeness IN('none','complete','capped','cancelled','interrupted','partial')),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>=1), dispatch_count INTEGER NOT NULL DEFAULT 0 CHECK(dispatch_count IN(0,1)),
 failure_code TEXT CHECK(failure_code IS NULL OR length(failure_code) BETWEEN 1 AND 64),
 movement_metadata_json TEXT CHECK(movement_metadata_json IS NULL OR (kind IN('import_job','export_job') AND json_valid(movement_metadata_json) AND length(CAST(movement_metadata_json AS BLOB))<=65536)),
 operator_acknowledged_at_ms INTEGER CHECK(operator_acknowledged_at_ms IS NULL OR operator_acknowledged_at_ms>=0),
 token_expires_at_ms INTEGER NOT NULL, dedup_retain_until_ms INTEGER NOT NULL,
 terminal_at_ms INTEGER, tombstoned_at_ms INTEGER,
 created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0), updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=created_at_ms),
 PRIMARY KEY(owner_id,workspace_id,target_id,operation_id), UNIQUE(owner_id,workspace_id,target_id,operation_id,kind),
 UNIQUE(owner_id,workspace_id,target_id,kind,idempotency_key),
 FOREIGN KEY(owner_id,workspace_id,target_id,binding_id) REFERENCES target_bindings(owner_id,workspace_id,target_id,binding_id) ON DELETE RESTRICT,
 FOREIGN KEY(owner_id,workspace_id,target_id,parent_operation_id,parent_kind) REFERENCES operations(owner_id,workspace_id,target_id,operation_id,kind) ON DELETE RESTRICT,
 FOREIGN KEY(owner_id,workspace_id,target_id,transaction_operation_id,transaction_kind) REFERENCES operations(owner_id,workspace_id,target_id,operation_id,kind) ON DELETE RESTRICT,
 CHECK((transaction_operation_id IS NULL AND transaction_kind IS NULL) OR (transaction_operation_id IS NOT NULL AND transaction_kind IS NOT NULL AND transaction_kind='manual_transaction')),
 CHECK(transaction_operation_id IS NULL OR (kind='query' AND transaction_operation_id<>operation_id)),
 UNIQUE(owner_id,workspace_id,target_id,parent_operation_id,batch_index),
 CHECK(token_expires_at_ms>created_at_ms AND token_expires_at_ms-created_at_ms<=86400000 AND dedup_retain_until_ms>=token_expires_at_ms),
 CHECK(terminal_at_ms IS NULL OR terminal_at_ms>=created_at_ms), CHECK(tombstoned_at_ms IS NULL OR (terminal_at_ms IS NOT NULL AND tombstoned_at_ms>=terminal_at_ms)),
 CHECK((kind='import_batch' AND parent_operation_id IS NOT NULL AND parent_kind IS NOT NULL AND parent_kind='import_job' AND parent_operation_id<>operation_id AND batch_index IS NOT NULL AND batch_index>=0 AND source_digest IS NOT NULL AND batch_digest IS NOT NULL AND source_byte_start IS NOT NULL AND source_byte_end IS NOT NULL AND source_row_start IS NOT NULL AND source_row_end IS NOT NULL AND source_byte_start>=0 AND source_byte_end>source_byte_start AND source_row_start>=0 AND source_row_end>source_row_start) OR (kind<>'import_batch' AND parent_operation_id IS NULL AND parent_kind IS NULL AND batch_index IS NULL AND source_digest IS NULL AND batch_digest IS NULL AND source_byte_start IS NULL AND source_byte_end IS NULL AND source_row_start IS NULL AND source_row_end IS NULL)),
 CHECK((state IN('accepted','dispatching','running','commit_requested') AND terminal_at_ms IS NULL) OR (state IN('committed','rolled_back','failed','outcome_unknown','completed','cancelled','interrupted','partial') AND terminal_at_ms IS NOT NULL)),
 CHECK(state<>'committed' OR transaction_outcome='committed'), CHECK(state<>'rolled_back' OR transaction_outcome='rolled_back')
) STRICT;
CREATE TABLE statements (
 owner_id TEXT NOT NULL, workspace_id TEXT NOT NULL, target_id TEXT NOT NULL, operation_id TEXT NOT NULL,
 ordinal INTEGER NOT NULL CHECK(ordinal>=0 AND ordinal<10000), statement_digest TEXT NOT NULL CHECK(length(statement_digest)=64),
 effect_class TEXT NOT NULL CHECK(effect_class IN('read','write','ddl','transaction','unknown')),
 outcome TEXT NOT NULL CHECK(outcome IN('not_sent','sent','succeeded','failed','cancelled','outcome_unknown')),
 effect_evidence TEXT NOT NULL DEFAULT 'none' CHECK(effect_evidence IN('none','read_only','pending_transaction','committed','rolled_back','outcome_unknown')),
 transaction_operation_id TEXT CHECK(transaction_operation_id IS NULL OR length(transaction_operation_id)=36), transaction_kind TEXT CHECK(transaction_kind IS NULL OR transaction_kind='manual_transaction'),
 matched_rows TEXT CHECK(matched_rows IS NULL OR (length(matched_rows) BETWEEN 1 AND 20 AND matched_rows NOT GLOB '*[^0-9]*')),
 changed_rows TEXT CHECK(changed_rows IS NULL OR (length(changed_rows) BETWEEN 1 AND 20 AND changed_rows NOT GLOB '*[^0-9]*')),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>=1), failure_code TEXT CHECK(failure_code IS NULL OR length(failure_code) BETWEEN 1 AND 64),
 created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0), updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=created_at_ms),
 CHECK((transaction_operation_id IS NULL AND transaction_kind IS NULL) OR (transaction_operation_id IS NOT NULL AND transaction_kind IS NOT NULL AND transaction_kind='manual_transaction')),
 PRIMARY KEY(owner_id,workspace_id,target_id,operation_id,ordinal),
 FOREIGN KEY(owner_id,workspace_id,target_id,transaction_operation_id,transaction_kind) REFERENCES operations(owner_id,workspace_id,target_id,operation_id,kind) ON DELETE RESTRICT,
 FOREIGN KEY(owner_id,workspace_id,target_id,operation_id) REFERENCES operations(owner_id,workspace_id,target_id,operation_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE saved_queries (
 owner_id TEXT NOT NULL, workspace_id TEXT NOT NULL, target_id TEXT NOT NULL, binding_id TEXT NOT NULL,
 query_id TEXT NOT NULL CHECK(length(query_id)=36), encryption_format TEXT NOT NULL CHECK(encryption_format='current-user-dpapi-v1'),
 encrypted_content BLOB NOT NULL CHECK(length(encrypted_content) BETWEEN 1 AND 262144),
 version INTEGER NOT NULL DEFAULT 1 CHECK(version>=1), created_at_ms INTEGER NOT NULL CHECK(created_at_ms>=0), updated_at_ms INTEGER NOT NULL CHECK(updated_at_ms>=created_at_ms),
 PRIMARY KEY(owner_id,workspace_id,target_id,query_id),
 FOREIGN KEY(owner_id,workspace_id,target_id,binding_id) REFERENCES target_bindings(owner_id,workspace_id,target_id,binding_id) ON DELETE RESTRICT
) STRICT;
CREATE INDEX bindings_scope_state ON target_bindings(owner_id,workspace_id,target_id,state,updated_at_ms,binding_id);
CREATE INDEX operations_transaction ON operations(owner_id,workspace_id,target_id,transaction_operation_id,transaction_kind);
CREATE INDEX statements_transaction_effect ON statements(owner_id,workspace_id,target_id,transaction_operation_id,transaction_kind,effect_evidence,ordinal);
CREATE INDEX operations_binding ON operations(owner_id,workspace_id,target_id,binding_id);
CREATE INDEX operations_scope_updated ON operations(owner_id,workspace_id,target_id,updated_at_ms,operation_id);
CREATE INDEX operations_scope_state ON operations(owner_id,workspace_id,target_id,state,updated_at_ms,operation_id);
CREATE INDEX operations_retention ON operations(dedup_retain_until_ms,terminal_at_ms) WHERE terminal_at_ms IS NOT NULL;
CREATE INDEX saved_queries_binding ON saved_queries(owner_id,workspace_id,target_id,binding_id);
CREATE INDEX saved_queries_scope_updated ON saved_queries(owner_id,workspace_id,target_id,updated_at_ms,query_id);
PRAGMA user_version = 1;
COMMIT;
