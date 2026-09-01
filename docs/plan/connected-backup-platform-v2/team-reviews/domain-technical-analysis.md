# Connected Backup Platform v2 Technical Analysis

## Scope and Grounding

This analysis uses only the requested planning documents and the local codebase. The current product already has a strong Data Plane core: PostgreSQL/MySQL/MariaDB Logical Dump paths, Destination fan-out, backup별 DEK, signed Manifest, Object Lock assertions, Catalog rebuild, Restore gate, L1/L2/L3 verification modules, Windows Task Scheduler, and Tauri desktop UI. The missing layer is the Control Plane operating model: Connection Profile, Database Target, Backup Plan, Backup Run, Snapshot inventory, Restore Run, Dispatcher, persistent queue, Lease, Resource Permit, and Activity Event as first-class contracts.

## 1. Required Technologies/Contracts By Feature

| 기술 영역 | 관련 Feature | Required technologies/contracts | Local reuse | Required gap |
|---|---|---|---|---|
| 1. Control Plane application service | Connections, Database Discovery, Backup Plans, Snapshots, Recovery | Shared application service used by Tauri, CLI, future HTTP API; UI must not run native tool directly | Tauri command surface, CLI command structure | Service boundary that owns Connection Profile, Database Target, Backup Plan, Backup Run, Snapshot, Restore Run |
| 2. Operational SQLite and persistent queue | Queue and Dispatcher, Activity, Catalog | SQLite WAL, single writer actor, transactional Work Item state transitions, Lease heartbeat/expiry, idempotency key | `dbbackup-catalog` single writer, schema, overlap guard | Work Item table, queue lanes, atomic lease acquisition, retry/delay/DLQ model |
| 3. Dispatcher and Resource Permit model | Multiple DB load control, cancellation, backpressure | Central Dispatcher, Resource Permit by global/host/CPU/Destination/restore/database, bounded workers | `run_scheduled_job`, `RunGuard`, scheduler notifications | Actual semaphore enforcement; `max_parallel_dumps` is currently only configuration, not execution control |
| 4. Engine Adapter and Support Contract | Connections, Discovery, Backup Execution, Verification, Restore, Engine Expansion | `probe`, `discover`, `preflight`, `backup`, `verify`, `restore`, `validate`; Capability and Engine Matrix | `BackupSource::plan/spawn`, PostgreSQL/MySQL/MariaDB version/tool logic | Wider Engine Adapter contract and first-class Support Contract gating |
| 5. Native child-process lifecycle | Backup Execution, Verification, cancellation | `tokio::process::Command`, `kill_on_drop(true)`, stdout/stderr drain, bounded stderr tail, timeout, process tree cancellation | PostgreSQL/MySQL dump process wrappers already use env secrets and stderr drain | Cross-platform child process tree termination, cancellation token, orphan cleanup after Lease expiry |
| 6. Secure Pipeline and Destination Adapter | Destinations, Backup Execution, Commit Protocol | Spool-once fan-out, compression, encryption, multipart/rename Commit Protocol, Destination Capability | `dbbackup-pipeline`, Local/S3/SSH Destination, Object Lock assertion | Destination-level Resource Permit, preflight capacity/write/read/delete/list/object lock health |
| 7. Manifest, Catalog, Snapshot truth | Snapshots, Catalog rebuild, Restore gate | Manifest is backup truth source; Catalog is SQLite cache/read model; rebuild from signed Manifest | signed Manifest, `catalog_rebuild`, `load_and_verify_manifest_for_restore` | Snapshot list/detail/search, restore eligibility read model, Manifest/Catalog reconciliation status |
| 8. Verification and Restore Run | Verification, Restore, restore drill | Verify Level L1/L2/L3, Restore Eligibility, Test-restore, disposable restore target, smoke query evidence | `dbbackup-verify`, sandbox restore, restore signature gate | Automatic verification cadence wiring, Restore Run lifecycle, managed restore testbeds |
| 9. Security and operational controls | Secrets, native tools, support bundle, destructive restore | existing secret store, writer/reader/admin separation, Origin Authentication, redaction, native tool hash/signature policy | `dbbackup-secrets`, scheduler redaction, S3 reader/writer split | native tool binary trust policy, support bundle redaction contract, restore approval boundary |
| 10. Future Agent boundary | Agent, future HTTP API, SQL Server `.bak` | Agent registration/approval/revoke, heartbeat, capabilities, leased Work Item execution, signed Activity Event/result | No current agent/API crate | Trust boundary, protocol compatibility, secret ownership, Catalog aggregation model |

## 2. External Integrations and Protocols

| Integration/protocol | Required role | Contract requirement |
|---|---|---|
| Windows Task Scheduler | Wake Dispatcher or ensure service state | Must not directly run individual Backup Plan execution in v2; direct per-job execution bypasses global Resource Permit |
| Tauri desktop | Primary Control Plane | Uses application service; must not call native dump tools directly |
| SQLite WAL | Operational SQLite, Catalog, persistent queue | Single writer actor owns write connection; readers use WAL read-only connections |
| Native OS child process | Data Plane execution | `pg_dump`, `pg_restore`, `pg_dumpall`, `mysqldump`/`mariadb-dump`, later `sqlpackage`, `sqlite3`, `mongodump`, `gbak`, `redis-cli` |
| S3 API | Destination | write/read/delete/list/Object Lock Capability, append-only writer, separate restore reader |
| SSH/SFTP | Destination and optional Tunnel | host-key pinning required; SSH Destination is not Agent |
| russh direct-tcpip Tunnel | Connection Profile connectivity | Shared resolver for probe/discover/backup/restore |
| Docker or managed disposable target | L3 Test-restore | Required for actual restore evidence; external runtime must be explicit |
| Future HTTP API | Later Control Plane surface | Same application service contract; not initial v2 public API |
| Future Agent protocol | Remote Data Plane | Leased Work Item execution, heartbeat, signed progress/result, cancellation, capability inventory |

## 3. Data Formats and Compatibility Matrices

### Core Data Formats

| Format | Truth role | Compatibility requirement |
|---|---|---|
| signed Manifest JSON | Backup truth source | Must include engine, source server version, tool name/version/platform, dump format, compression/encryption format, object scope, restore support, checksum, signature, Verify Level evidence |
| Catalog SQLite | Cache/read model | Rebuildable from signed Manifest; must not override Manifest truth |
| Operational SQLite queue | Control state truth | Owns Work Item, Lease, Backup Run, Restore Run, Activity Event; transitions must be transactional |
| TOML config/export | Import/export and migration | Existing TOML should migrate from old `Database`/`Job` into Connection Profile, Database Target, Backup Plan |
| encrypted compressed spool | Data artifact | Only becomes Snapshot after Commit Protocol and signed Manifest publication |
| Activity Event records | Operator evidence | Structured, immutable, paginated/streamed; debug log is separate |

### Engine Matrix

| Engine | Native tool/format | Initial restore Capability | Required compatibility boundary |
|---|---|---|---|
| PostgreSQL | `pg_dump`/`pg_restore`, `pg_dumpall --globals-only` | Full logical restore after L3 | client/server major pairing, plain/archive path, globals pairing |
| MySQL | `mysqldump` or compatible | Full logical restore after L3 | MySQL major, client kind, routine/event/trigger/view/binary data |
| MariaDB | `mariadb-dump` | Full logical restore after L3 | separate MySQL/MariaDB tool/version matrix |
| SQL Server | `sqlpackage` `.dacpac` | Scoped logical restore | not equivalent to `.bak`; server-bound object exclusions must be visible |
| SQLite | online `.backup` or Backup API | Restore supported after WAL/lock validation | raw file copy only with quiesce+WAL conditions |
| MongoDB | `mongodump`/`mongorestore` archive | Later wave | namespace/consistency and source/tool/target versions |
| Firebird | `gbak` `.fbk` | Later wave | dialect/version and destructive replace semantics |
| Redis | `redis-cli --rdb` | backup-only initially | restore requires service stop/data directory control, likely Agent |
| Valkey | `redis-cli --rdb` | backup-only initially | separate Redis/Valkey Capability and matrix |
| SQL Server native | `.bak` | later advanced mode | requires DB-adjacent Agent and server file/permission model |

### Destination Capability Matrix

| Destination | write | read | delete/list | Immutability Capability | Required handling |
|---|---:|---:|---:|---|---|
| Local | yes | yes | yes | None | health, free space, rename Commit Protocol |
| S3 | yes | yes | yes | Governance/Compliance when verified | Object Lock self-test, separate writer/reader |
| SSH/SFTP | yes | yes | server-dependent | ServerAppendOnly only when proven | host-key pinning, append-only must not be assumed |
| Azure Blob/SMB/rclone | future | future | future | adapter-specific | do not expose until Destination Adapter contract and tests exist |

### Verify Level Matrix

| Verify Level | Meaning | Required evidence |
|---|---|---|
| L1 | file integrity and Manifest signature | checksum, Origin Authentication, decrypt/decompress ability |
| L2 | native tool format validity | tool/version, format validation output |
| L3 | actual restore into disposable target | Restore Run, smoke query, schema/object checks, measured RTO/RPO |

## 4. Security and Operational Control Requirements

- Manifest/catalog truth: Manifest is the backup truth source. Catalog is a cache and search index. Restore Eligibility must be computed from freshly read and verified Manifest bytes, not only from Catalog rows.
- SQLite transaction ownership: one writer actor must own SQLite writes. Work Item lease acquisition, heartbeat, status change, retry scheduling, and Activity Event append must be short transactions. A child process must never hold an SQLite transaction while dumping, uploading, or restoring.
- Persistent queue leases: Work Item must include `idempotency_key`, `attempt`, `lease_owner`, `lease_expires_at`, `scheduled_for`, `priority`, `resource_keys`, and `next_retry_at`. Lease expiry must trigger orphan child process inspection before retry or failure.
- Idempotency: the same scheduled occurrence must not produce duplicate committed Snapshot records. If a duplicate tick arrives while queued/running, coalesce it and record missed run/RPO violation when applicable.
- Child-process cancellation: cancellation must propagate to the child process tree, drain/close pipes, wait/reap processes, release Resource Permit, and remove or quarantine partial artifact.
- Secret handling: Connection Profile and Destination credentials must use existing secret store; secrets must not appear in process arguments, Activity Event, structured log, support bundle, or dump flags.
- Native tool trust: Engine Adapter must record and validate native tool path, version, platform, and hash/signature policy before Support Contract pass.
- Restore-as-Exec: Logical Dump restore is high-risk. Restore Run must require signed Manifest, Restore Eligibility, explicit target confirmation, and isolation or approval boundary.
- Writer/reader separation: backup writer and restore reader credentials must remain separable, especially for S3/SSH.
- Future Agent trust boundary: Agent is Data Plane, not Control Plane. Agent must never become an unbounded secret sink or unauthenticated remote executor; every Agent Work Item must be leased, scoped, cancellable, and auditable.

## 5. Measurable Performance/Reliability Targets

| Target | Required measure |
|---|---|
| Connection Profile registration to first successful backup | median <= 5 minutes |
| due scheduled occurrence missed by Dispatcher | 0 |
| duplicate committed Snapshot for same scheduled occurrence | 0 |
| committed Snapshot with L1 evidence | 100% |
| policy-target Snapshot meeting L3 deadline | >= 95% |
| unknown/unclassified failure reason | < 5% |
| supported Engine Matrix entry with managed testbed restore pass | 100% |
| default Resource Permit | global jobs 2, per source host 1, CPU stages 1-2, per Destination 2, restore 1 |
| UI activity browsing | usable at 100 Backup Plan and 100,000 Activity Event records with pagination/streaming |
| cancellation cleanup | no dump process, open pipe, partial spool, or uncommitted object remains after cancel/timeout |
| dispatcher crash recovery | queued/leased/running states recover deterministically after app/host restart |
| backpressure observability | queue depth, oldest age, active permits, process duration, CPU-stage wait time, RPO breach visible |

## 6. Existing Solutions or Native Tools That Should Be Reused

| Reuse item | How to reuse |
|---|---|
| Existing Rust secure core | Keep as Data Plane foundation; do not rewrite around Databasement runtime |
| `BackupSource::plan/spawn` | Expand into Engine Adapter rather than discard |
| PostgreSQL bundled tool matrix | Reuse for `pg_dump`, `pg_restore`, `pg_dumpall` version selection |
| MySQL/MariaDB binary/version/preflight logic | Reuse for MySQL/MariaDB Support Contract and Engine Matrix |
| Destination trait and `DestCaps` | Extend into Destination Adapter Capability model |
| S3 Object Lock self-test | Keep as pre-run gate for Immutability Capability |
| SSH/SFTP Destination and host-key pinning | Reuse for remote Destination and Tunnel foundations |
| signed Manifest and Origin Authentication | Keep as restore/catalog rebuild trust gate |
| Catalog single writer and rebuild | Reuse, then extend schema/read model |
| `dbbackup-verify` L1/L2/L3 modules | Wire into automatic verification cadence and Restore Run |
| Windows Task Scheduler registration | Re-scope to Dispatcher wake/service management |
| Databasement | Reuse as information architecture and engine coverage benchmark only, not code fork |
| Native tools | `pg_dump`, `pg_restore`, `pg_dumpall`, `mysqldump`, `mariadb-dump`, `sqlpackage`, `sqlite3 .backup`, `mongodump`, `mongorestore`, `gbak`, `redis-cli --rdb` |

## 7. Missing Technical Items

### Critical

| Item | Why critical |
|---|---|
| Persistent queue with Lease and heartbeat | Without it, restart recovery and safe multi-DB scheduling are not reliable |
| Atomic Work Item idempotency | Required to guarantee duplicate committed Snapshot count is 0 |
| Central Dispatcher with Resource Permit | Current per-job scheduler/process model cannot enforce global backpressure |
| SQLite transaction ownership rules for queue and Catalog | Prevents lock contention, long transactions, and split-brain write paths |
| Child process tree cancellation and orphan cleanup | Required for safe cancel, timeout, and Lease expiry recovery |
| Manifest/Catalog truth contract in restore path | Prevents stale Catalog rows from authorizing restore |
| Engine Adapter Support Contract | Prevents enum/logo-only “support” without restore evidence |
| Managed restore testbeds for supported Engine Matrix entries | Required before any engine/version is shown as supported |
| Native tool binary trust policy | Required because tools become part of the trusted Data Plane |
| Future Agent trust boundary | Must be designed before SQL Server `.bak` or DB-adjacent execution is introduced |

### Important

| Item | Why important |
|---|---|
| Snapshot search/detail/read API | Users need to find recoverable Snapshot quickly |
| Activity Event schema and pagination | Operational diagnosis requires structured evidence, not raw logs |
| Restore Run lifecycle and UI | Backup ID manual entry is not enough for safe restore |
| Verification cadence wiring | Existing Verify Level code must run automatically and record evidence |
| Destination preflight and health | Avoids discovering no space/no read/no Object Lock only after dump |
| Connection Profile and Database Target migration | Prevents credential duplication and enables discovery |
| Tunnel resolver shared across probe/discover/backup/restore | Prevents inconsistent connectivity behavior |
| Retry classification and DLQ/delayed lane | Prevents retry storms on auth/permission/compatibility failures |
| Support bundle redaction contract | Needed for real operations without secret leakage |

### Nice-to-Have

| Item | Why nice-to-have |
|---|---|
| Azure Blob/SMB/rclone Destination Adapter | Useful after Local/S3/SSH are stable |
| HTTP API | Useful after desktop Control Plane contracts are stable |
| Agent dashboard | Valuable once Agent exists, not before |
| Dynamic adapter/plugin loading | Premature for v2; static Rust adapters are safer initially |
| Advanced SQL Server `.bak` | Valuable, but requires Agent and server-side permission model |
| Redis/Valkey restore | Should wait until service-stop/data-directory control is validated |

## 8. Dictionary Updates

| 행위 | 용어 | 정의 (제안) | 근거 |
|---|---|---|---|
| ADD | Persistent Queue | Work Item을 SQLite에 저장하고 재시작 후 Lease와 상태를 복구하는 영속 실행 대기열 | FR-DSP와 AS-02의 재시작 복구, backpressure 제어에 필요 |
| ADD | Idempotency Key | 동일 scheduled occurrence 또는 동일 사용자 요청이 중복 committed Snapshot을 만들지 않게 식별하는 키 | 성공 지표의 중복 committed Snapshot 0건 보장 |
| ADD | Lease Heartbeat | Dispatcher가 Lease를 유지 중임을 주기적으로 갱신하는 신호 | 끊긴 작업의 orphan child process 확인과 재개/실패 처리에 필요 |
| ADD | Engine Matrix | 엔진·서버 버전·native tool·기능별 검증 상태 표 | Support Contract의 실제 증적 단위 |
| ADD | RPO Violation | 마지막 유효 Snapshot이 정책 RPO를 넘긴 운영 상태 | 실행 성공/실패와 독립된 운영 판단 필요 |
| ADD | Restore Testbed | L3 Test-restore를 수행하는 격리된 disposable target 환경 | 지원 표시 전 실제 restore E2E를 증명해야 함 |
| ADD | Trust Boundary | Control Plane, Data Plane, Agent, Destination 사이에서 권한·비밀·실행 책임이 넘어가는 경계 | 향후 Agent가 원격 실행 권한을 가지므로 사전 정의 필요 |
| ADD | Transaction Owner | 특정 SQLite write transaction을 열고 commit/rollback할 수 있는 단일 소유자 | SQLite transaction ownership과 단일 writer actor 규칙 명확화 |
| REFINE | Lease | Work Item 실행 권한뿐 아니라 heartbeat, expiry, orphan cleanup 기준을 포함한다 | persistent queue lease 의미를 운영 복구까지 확장 |
| REFINE | Snapshot | committed artifact와 signed Manifest가 모두 존재하고 Catalog에서 검색 가능한 표현 | partial artifact와 Catalog-only 상태를 배제 |

`#tags: dbbackup, connected-backup-platform-v2, domain-dictionary, dispatcher, restore, sqlite, manifest`