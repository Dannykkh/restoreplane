-- db-schema.md DDL 정본과 1:1. 이 파일이 유일한 스키마 소스(SCHEMA_SQL).
-- 전부 IF NOT EXISTS라 멱등(같은 DB에 두 번 실행해도 안전).

CREATE TABLE IF NOT EXISTS backups (
  id TEXT PRIMARY KEY,
  job TEXT NOT NULL,
  source TEXT NOT NULL,
  engine TEXT NOT NULL,
  kind TEXT NOT NULL,
  pairs_with_globals_backup_id TEXT,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  status TEXT NOT NULL,
  server_version TEXT,
  dump_tool TEXT,
  dump_tool_version TEXT,
  dump_format TEXT,
  charset TEXT,
  plaintext_blake3 TEXT,
  plaintext_bytes INTEGER,
  cipher_bytes INTEGER,
  dek_wrap_gen INTEGER,
  manifest_signed INTEGER,
  schema_version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS artifacts (
  backup_id TEXT NOT NULL REFERENCES backups(id),
  dest_name TEXT NOT NULL,
  dest_kind TEXT NOT NULL,
  object_key TEXT NOT NULL,
  object_checksum TEXT,
  immutability TEXT,
  retention_until TEXT,
  status TEXT NOT NULL,
  uploaded_at TEXT,
  PRIMARY KEY (backup_id, dest_name)
);

CREATE TABLE IF NOT EXISTS verifications (
  id TEXT PRIMARY KEY,
  backup_id TEXT NOT NULL REFERENCES backups(id),
  verified_at TEXT NOT NULL,
  level INTEGER NOT NULL,
  result TEXT NOT NULL,
  restore_target_version TEXT,
  restored_rows INTEGER,
  schema_hash TEXT,
  notes TEXT
);

CREATE TABLE IF NOT EXISTS runs (
  id TEXT PRIMARY KEY,
  job TEXT NOT NULL,
  started_at TEXT NOT NULL,
  status TEXT NOT NULL
);
-- overlap guard 2차 방어: 같은 job에 running row는 최대 1개(부분 UNIQUE).
CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_single_active ON runs(job) WHERE status='running';

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);

CREATE INDEX IF NOT EXISTS idx_backups_job_started ON backups(job, started_at);
CREATE INDEX IF NOT EXISTS idx_artifacts_status ON artifacts(status);
CREATE INDEX IF NOT EXISTS idx_verifications_backup_level ON verifications(backup_id, level);
