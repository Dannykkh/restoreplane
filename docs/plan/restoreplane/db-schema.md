# DB Schema — Local Catalog (SQLite) + Manifest (동봉)

> Catalog는 로컬 조회 캐시, Manifest는 목적지 동봉 진실 소스. Catalog는 Manifest로 재구성 가능해야 함.
> SQLite: `PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;` 단일 writer 커넥션.

## ERD (개념)
```
backups 1 ── N artifacts        (backup × destination)
backups 1 ── N verifications
backups N ── 1 backups(globals) (PG: pairs_with_globals_backup_id 자기참조)
runs (overlap guard/미스런 로그, 독립)
meta (schema_version, argon params ref, sign pubkey ref)
```

## DDL (SQLite)
```sql
CREATE TABLE backups (
  id TEXT PRIMARY KEY,                 -- UUID
  job TEXT NOT NULL,
  source TEXT NOT NULL,                -- db alias (add-db 이름)
  engine TEXT NOT NULL,                -- postgresql | mysql | mariadb
  kind TEXT NOT NULL,                  -- db | globals
  pairs_with_globals_backup_id TEXT,   -- PG DB덤프↔globals 짝 (NULL 가능)
  started_at TEXT NOT NULL,            -- RFC3339 UTC
  finished_at TEXT,
  status TEXT NOT NULL,                -- running | success | failed | partial
  server_version TEXT,
  dump_tool TEXT,                      -- pg_dump | pg_dumpall | mysqldump | mydumper
  dump_tool_version TEXT,
  dump_format TEXT,                    -- custom | plain | directory
  charset TEXT,
  plaintext_blake3 TEXT,               -- 압축/암호화 전 평문 해시
  plaintext_bytes INTEGER,
  cipher_bytes INTEGER,
  dek_wrap_gen INTEGER,                -- 백업별 DEK 세대 (rotate 추적)
  manifest_signed INTEGER,            -- 0/1 서명 여부
  schema_version INTEGER NOT NULL
);

CREATE TABLE artifacts (              -- backup × destination (부분성공 1급)
  backup_id TEXT NOT NULL REFERENCES backups(id),
  dest_name TEXT NOT NULL,
  dest_kind TEXT NOT NULL,            -- s3 | ssh | local
  object_key TEXT NOT NULL,
  object_checksum TEXT,              -- 목적지측 ETag/checksum
  immutability TEXT,                 -- none | governance | compliance | server_append_only
  retention_until TEXT,
  status TEXT NOT NULL,              -- pending | committed | failed
  uploaded_at TEXT,
  PRIMARY KEY (backup_id, dest_name)
);

CREATE TABLE verifications (
  id TEXT PRIMARY KEY,
  backup_id TEXT NOT NULL REFERENCES backups(id),
  verified_at TEXT NOT NULL,
  level INTEGER NOT NULL,            -- 1 파일무결성 | 2 포맷유효성 | 3 실제복원
  result TEXT NOT NULL,             -- pass | fail
  restore_target_version TEXT,      -- L3: 임시 인스턴스 버전 (원본과 갭 투명화)
  restored_rows INTEGER,
  schema_hash TEXT,
  notes TEXT
);

CREATE TABLE runs (                  -- overlap guard + 미스런 로그
  id TEXT PRIMARY KEY,
  job TEXT NOT NULL,
  started_at TEXT NOT NULL,
  status TEXT NOT NULL              -- running | done | skipped(overlap) | coalesced
);
CREATE UNIQUE INDEX idx_runs_single_active ON runs(job) WHERE status='running';

CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
-- meta: schema_version, argon2_params, sign_pubkey_fpr, dpapi_entropy_ref
```

## Manifest (목적지 동봉, 서명 대상 JSON)
```jsonc
{
  "manifest_version": 1,
  "backup_id": "uuid",
  "job_name": "prod-pg",
  "db_alias": "app",
  "created_at": "2026-07-16T03:00:00Z",
  "db_engine": "postgresql",
  "server_version": "PostgreSQL 16.3 ...",           // 원문 + parsed semver
  "server_version_detected_at": "2026-07-16T03:00:01Z",
  "database_name": "app",                             // globals면 "cluster"
  "charset": "UTF8", "collation": "...",
  "extensions": [{"name":"postgis","version":"3.4"}], // PG
  "non_transactional_tables": [],                     // MySQL MyISAM 경고
  "dump_tool": "pg_dump", "dump_tool_version": "16.3",
  "dump_format": "custom",
  "dump_flags": "pg_dump -Fc -f - --no-password ...", // secret 제외 원문
  "pairs_with_globals_backup_id": "uuid-globals",     // PG
  "binlog_coordinates": null, "gtid_set": null,       // MySQL --source-data=2
  "compression": {"algo":"zstd","level":3},
  "encryption": {"algo":"age/chacha20poly1305","dek_gen":42,
                 "unwrap_paths":["password","recovery","dpapi"]},
  "plaintext_blake3": "...", "plaintext_bytes": 0,
  "compressed_bytes": 0, "encrypted_bytes": 0,
  "checksum_final": "...",
  "schema_hash": "...",                                // S03/S04가 백업 시점 스키마 해시 캡처 → S09 L3 대조
  "source_row_counts": {"public.users": 0},            // 백업 시점 테이블별 row count → S09 L3 대조(라이브 DB 아님)
  "wrapset_blake3": "...",                             // wrap 스탠자 집합 해시 → S09 복원 시 무결성 확인
  "destination": {"type":"s3","bucket":"...","object_key":"...",
                  "object_lock_mode":"governance","retain_until":"2026-08-15T..."},
  "verify": {"level":0,"verified_at":null,"method":null,
             "row_counts":{},"restore_target_version":null},
  "signature": {"alg":"ed25519","pubkey_fpr":"...","sig":"base64"}  // 본체 서명
}
```

## 설계 근거
- **artifacts를 backups 자식으로**: 목적지별 상태 분리 → 부분 성공/재시도가 1급.
- **plaintext_blake3 vs checksum_final**: 전자는 "복원한 게 원본과 동일" 증명(verify L1), 후자는 전송 손상 탐지.
- **runs 부분 UNIQUE 인덱스**: 스케줄 catch-up 중복 발화 방지.
- **manifest 서명 + 동봉**: catalog 소실/변조 시에도 오프사이트 단독 복원 + 위조 차단(C3/H5/P0-6).
- **dek_wrap_gen**: rotate-key가 재암호화 없이 wrap만 갱신, 백업별 DEK 세대 추적.

## 인덱스 전략
- `backups(job, started_at)` — 최신 백업 조회.
- `artifacts(status)` — 미완료 업로드 재개.
- `verifications(backup_id, level)` — 검증 레벨 조회.
- runs 단일활성 부분 인덱스(위).
