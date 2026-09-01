# Connected Backup Platform v2 Business Processes

Scope: based only on the requested planning documents and the local codebase. The current codebase already has strong `BackupSource`, `Destination`, `Manifest`, `Catalog`, scheduler, verify, restore gate, and catalog rebuild assets, but the v2 business model requires separating existing overused `Job` meaning into `Backup Plan` and `Backup Run`.

## 1. Connection Onboarding

### Purpose
Register a `Connection Profile` that represents one DB server or SQLite file, prove it can be reached, and record its verified `Capability` without mixing in DB selection or scheduling.

### Actor
Windows 운영자 through `Control Plane`; `Engine Adapter` performs probe in `Data Plane`.

### CRUD/execute Authority
- Create: Windows 운영자 creates `Connection Profile`.
- Read: Windows 운영자 and diagnostic views read probe status.
- Update: Windows 운영자 updates connection attributes and reruns probe.
- Delete: Windows 운영자 may delete only after impact review.
- Execute: `Engine Adapter` executes probe; UI must not execute native tool directly.

### Inputs and Preconditions
- Engine, name, host, port, credentials, TLS, optional `Tunnel`, or SQLite file path.
- Existing secret store available for credentials.
- Native tool path/version can be resolved.
- No DB selection, `Destination`, schedule, `Retention`, or `Verify Level` stored in the `Connection Profile`.

### Processing Steps
1. Operator enters `Connection Profile` fields in `Control Plane`.
2. `Control Plane` validates required fields and routes credential storage to secret store.
3. `Engine Adapter` runs `probe`: network reachability, authentication, server version, permission, TLS, optional `Tunnel`, and native tool version.
4. Probe result records verified `Capability` and failure category.
5. On success, `Connection Profile` transitions to probe-passed and becomes eligible for discovery.
6. On failure, `Activity Event` records phase, engine, retryability, and redacted diagnostic message.

### Outputs and State Transitions
- Draft `Connection Profile` -> probe-passed `Connection Profile`.
- Failed probe -> attention state with DNS/network/TLS/auth/permission/tool-missing classification.
- Verified `Capability` becomes input to discovery and `Support Contract`.

### Exception/Cancel/Retry Path
- Cancel: user cancels before save; no credential or `Connection Profile` becomes active.
- Retry: rerun probe after credential, network, TLS, or tool fix.
- Delete exception: deletion is blocked if related `Database Target` or `Backup Plan` exists; impact count must be shown first.

## 2. Database Discovery/Selection

### Purpose
Discover backup-eligible databases from a `Connection Profile`, separate discovery results from protected `Database Target`, and handle drift without destructive automatic deletion.

### Actor
Windows 운영자; `Engine Adapter` performs discover.

### CRUD/execute Authority
- Create: Windows 운영자 creates `Database Target` by selecting discovered DBs.
- Read: Windows 운영자 reads discovered DB list and existing `Database Target` list.
- Update: Windows 운영자 can rename display metadata or mark attention state.
- Delete: Windows 운영자 can remove protection, but existing `Snapshot` and `Backup Plan` are not auto-deleted.
- Execute: `Engine Adapter` executes discover.

### Inputs and Preconditions
- Probe-passed `Connection Profile`.
- `Engine Adapter` has `discover` `Capability`.
- For SQLite, file path, lock, and WAL precheck replace server discovery.

### Processing Steps
1. Operator starts discovery from a probe-passed `Connection Profile`.
2. `Engine Adapter` lists databases and marks system DB, template DB, offline DB, and engine-specific exclusions.
3. `Control Plane` shows discovered DBs with eligibility and exclusion reasons.
4. Operator selects one or more DBs.
5. System creates one `Database Target` per selected database.
6. On rediscovery, system calculates diff for added, deleted, and renamed DBs.
7. Deleted DBs move related `Database Target` or `Backup Plan` to attention state, not automatic deletion.

### Outputs and State Transitions
- Discovered DB row -> selected `Database Target`.
- Existing `Database Target` -> attention state if missing on rediscovery.
- Discovery diff becomes an `Activity Event`.

### Exception/Cancel/Retry Path
- Cancel: selection cancel leaves discovery results unchanged.
- Retry: discovery can be rerun after permission/tool fixes.
- Exception: engine lacking restore `Capability` may still create backup-only `Database Target`, but restore actions must be hidden or shown as unsupported.

## 3. Destination Onboarding

### Purpose
Register a `Destination`, prove writer/reader access and `Immutability Capability`, and separate backup writer credentials from restore reader credentials.

### Actor
Windows 운영자; `Destination Adapter` performs probe/preflight.

### CRUD/execute Authority
- Create: Windows 운영자 creates `Destination`.
- Read: Windows 운영자 reads capability and health.
- Update: Windows 운영자 changes endpoint, path, reader credentials, Object Lock, or retention.
- Delete: Windows 운영자 deletes only after `Backup Plan` and `Snapshot` impact review.
- Execute: `Destination Adapter` probes write/read/delete/list/object lock capability.

### Inputs and Preconditions
- Local, S3, or SSH/SFTP fields.
- Backup writer credential and optional restore reader credential.
- Object Lock or append-only settings if required.
- Secret store available for sensitive fields.

### Processing Steps
1. Operator enters `Destination` connection and path/bucket data.
2. System stores secrets outside operational DB.
3. `Destination Adapter` probes write, read, delete, list, and Object Lock behavior.
4. System records `Immutability Capability`.
5. If reader credentials are provided, system verifies read-only restore access without writer fallback.
6. `Destination` becomes eligible for `Backup Plan`.

### Outputs and State Transitions
- Draft `Destination` -> probe-passed `Destination`.
- Capability report includes write/read/delete/list/Object Lock.
- Failed probe creates `Activity Event` with phase and retryability.

### Exception/Cancel/Retry Path
- Cancel: incomplete `Destination` is not usable by `Backup Plan`.
- Retry: rerun probe after IAM/path/network fix.
- Exception: if required `Immutability Capability` is missing, `Backup Plan` creation or execution preflight must block loudly.

## 4. Backup Plan Creation

### Purpose
Create a `Backup Plan` that declares which `Database Target` is protected, where it is sent, when it runs, how long it is retained, and what `Verify Level` is required.

### Actor
Windows 운영자 through `Control Plane`.

### CRUD/execute Authority
- Create: Windows 운영자 creates `Backup Plan`.
- Read: Windows 운영자 reads plan and health.
- Update: Windows 운영자 updates schedule, `Destination`, `Retention`, `Verify Level`, and enabled state.
- Delete: Windows 운영자 deletes plan declaration, not historical `Backup Run` or `Snapshot`.
- Execute: manual and scheduled execution only enqueue `Work Item`; `Dispatcher` starts actual work.

### Inputs and Preconditions
- At least one `Database Target`.
- At least one probe-passed `Destination`.
- Schedule.
- `Retention`.
- Verification policy expressed with `Verify Level`.
- Engine and destination `Capability` visible before save.

### Processing Steps
1. Operator selects one or more `Database Target`.
2. Operator selects one or more `Destination`.
3. System checks engine `Capability`, destination `Immutability Capability`, and restore limitations.
4. Operator sets schedule, `Retention`, and verification policy.
5. System validates schedule conflicts and applies default `coalesce-one` duplicate policy.
6. System validates destination-specific `Retention`.
7. System saves `Backup Plan` as declaration only.

### Outputs and State Transitions
- Valid configuration -> enabled `Backup Plan`.
- Unsupported combination -> blocked or degraded plan with explicit unsupported reason.
- Existing codebase `Job` configuration maps conceptually to `Backup Plan`, but v2 must not use `Job` for execution state.

### Exception/Cancel/Retry Path
- Cancel: no `Backup Plan` created.
- Retry: fix missing capability, schedule, or destination preflight issue and resubmit.
- Exception: if schedule delay exceeds RPO budget later, `RPO Violation` must be recorded independent of run success.

## 5. Scheduled Dispatch

### Purpose
Turn due `Backup Plan` occurrences into durable `Work Item` records and let only `Dispatcher` acquire `Lease` and `Resource Permit`.

### Actor
Windows Scheduler wakes service/dispatcher; `Dispatcher` owns execution start.

### CRUD/execute Authority
- Create: Windows Scheduler or manual run request creates/enqueues `Work Item`.
- Read: `Dispatcher` reads queue, lease state, and permit state.
- Update: `Dispatcher` updates `Work Item` state.
- Delete: queue records are not deleted as control evidence; terminal states are retained.
- Execute: only `Dispatcher` starts executor; UI and Windows Scheduler must not directly run native backup tools.

### Inputs and Preconditions
- Enabled `Backup Plan`.
- Due scheduled occurrence or manual run request.
- Persistent queue in SQLite.
- Idempotency key for scheduled occurrence.
- Configured `Resource Permit` limits.

### Processing Steps
1. Scheduler trigger wakes dispatcher or ensures service availability.
2. System computes due `Backup Plan` occurrences.
3. For each occurrence, system creates or reuses idempotent `Work Item`.
4. `Work Item` enters `queued`.
5. `Dispatcher` attempts to acquire `Lease`.
6. `Dispatcher` attempts required `Resource Permit`: global, host, CPU, destination, restore.
7. If permits are acquired, `Work Item` becomes `leased`, then `running`.
8. If not, `Work Item` remains `queued` with expected wait reason.

### Outputs and State Transitions
- due occurrence -> `Work Item(queued)`.
- `queued` -> `leased` -> `running`.
- Stale lease -> `expired` or returned to `queued` after recovery check.
- Activity view shows queued/running/succeeded/failed/cancelled/expired.

### Exception/Cancel/Retry Path
- Cancel: cancelling queued item sets `cancelled`; cancelling running item propagates to child process tree.
- Retry: failed retryable item gets next retry time with exponential backoff and max attempts.
- Exception: heartbeat loss triggers orphan child process check before requeue or failure.

## 6. Backup Execution/Commit

### Purpose
Execute a `Backup Run` using one `Logical Dump` per `Database Target`, protect it through encryption and signing, fan out to `Destination`, and create a committed `Snapshot` only after `Commit Protocol`.

### Actor
`Dispatcher` starts executor; `Engine Adapter`, `BackupSource`, secure pipeline, and `Destination` perform work.

### CRUD/execute Authority
- Create: `Dispatcher` creates `Backup Run` from `Work Item`.
- Read: operator and diagnostics read status and `Activity Event`.
- Update: executor updates phase, metrics, and result.
- Delete: committed `Snapshot` is governed by `Retention`, not run cleanup.
- Execute: `Engine Adapter` and pipeline execute; `Destination` writes artifacts.

### Inputs and Preconditions
- `Work Item(running)` with valid `Lease`.
- `Backup Plan` still enabled or allowed to finish once started.
- `Resource Permit` acquired.
- Preflight passed for `Database Target`, `Destination`, spool disk, native tool, and Object Lock.
- Secret store unlock available.
- Existing security core available: per-backup `DEK`, `KEK` wrapping, `Origin Authentication`, signed `Manifest`.

### Processing Steps
1. Create `Backup Run` and first `Activity Event`.
2. Run engine preflight: server/tool version, permission, dump options, consistency mode.
3. Produce `DumpArtifact` from `BackupSource`.
4. Drain stdout/stderr and redact sensitive data.
5. Apply compression and encryption using per-backup `DEK`.
6. Write local spool once.
7. Write signed `Manifest` with wrap set.
8. Fan out encrypted artifact and signed `Manifest` to each `Destination`.
9. Apply `Commit Protocol`: exit0, pipeline finish, multipart close, then manifest commit.
10. Only fully committed destination outcomes update `Catalog`.
11. Mark `Backup Run` `succeeded`, `failed`, or partial-success equivalent if policy allows partial destination success.
12. Expose committed `Snapshot` only for artifact plus signed `Manifest` that passed commit.

### Outputs and State Transitions
- `Backup Run(running)` -> `succeeded` or `failed`.
- Successful committed artifact + signed `Manifest` -> `Snapshot(committed)`.
- `Catalog` receives cache rows for successful committed destinations.
- `Activity Event` records engine/tool/version/options, source identity, bytes, duration, phases, retryability.

### Exception/Cancel/Retry Path
- Failed dump: no committed `Snapshot`; no committed `Manifest` should appear in any destination.
- Failed destination: failed destination recorded; successful destinations may remain committed if commit completed.
- Cancel: child process tree terminated; partial artifact cleanup; no pre-commit artifact exposed as `Snapshot`.
- Retry: retry only retryable phases with bounded attempts and backoff; do not amplify backlog indefinitely.

## 7. Verification

### Purpose
Prove a `Snapshot` at the required `Verify Level`, distinguish L1/L2/L3, and store evidence rather than a vague “verified” flag.

### Actor
`Dispatcher` for scheduled verification or operator for manual verification; `Verifier` and `Restorer` perform work.

### CRUD/execute Authority
- Create: verification request creates verification `Work Item` or verification record.
- Read: operator reads `Verify Level`, timestamp, result, and evidence.
- Update: system updates `Catalog` verification state.
- Delete: verification evidence should remain as operational history.
- Execute: `Verifier` executes L1/L2/L3; restore drill uses separate restore `Resource Permit`.

### Inputs and Preconditions
- Committed `Snapshot`.
- Signed `Manifest`.
- Reader credential if remote `Destination`.
- Pinned public key for `Origin Authentication`.
- KEK unlock path for DEK recovery.
- Native verification tool for L2.
- Disposable target for L3.

### Processing Steps
1. Select `Snapshot` and required `Verify Level`.
2. Load signed `Manifest` from selected `Destination` or local spool.
3. Run signature and integrity gate before DEK unwrap.
4. For L1, decrypt/decompress artifact and compare file integrity against `Manifest`.
5. For L2, use engine-native tool to validate format and structure.
6. For L3, restore into isolated disposable target.
7. Run smoke query and capture evidence.
8. Record result in `Catalog` with level, timestamp, and evidence.
9. Emit `Activity Event` and update attention state for failed, overdue, or missing verification.

### Outputs and State Transitions
- `Snapshot(unverified)` -> `verified(L1)`, `verified(L2)`, or `verified(L3)`.
- Failed verification -> attention state.
- Verification overdue -> attention state without changing committed status.

### Exception/Cancel/Retry Path
- Gate failure: verification stops before decrypt or restore.
- L2/L3 unavailable: record unsupported/missing runtime, do not label as higher `Verify Level`.
- Cancel: running verification stops and records cancelled.
- Retry: permitted after fixing reader credentials, tool path, Docker/disposable target, or corrupted local cache.

## 8. Snapshot Inventory/Rebuild

### Purpose
Rebuild searchable `Snapshot` inventory when local `Catalog` is lost or inconsistent, using destination-side signed `Manifest` as truth source.

### Actor
Windows 운영자 starts rebuild; system reads `ManifestSource`; `Catalog` writer updates cache.

### CRUD/execute Authority
- Create: rebuild creates `Catalog` rows from verified `Manifest`.
- Read: rebuild reads destination inventory and signed `Manifest`.
- Update: rebuild upserts cache rows.
- Delete: rebuild should not delete destination artifacts; local cache reconciliation must be explicit.
- Execute: catalog rebuild process executes; restore reader credential is used for remote reads.

### Inputs and Preconditions
- Selected `Destination`.
- `ManifestSource` can list and read manifests.
- Pinned public key available.
- Reader credential available for S3/SSH if needed.
- Local operational SQLite path available.

### Processing Steps
1. Operator chooses source `Destination` for rebuild.
2. System builds local/S3/SSH `ManifestSource`.
3. For remote source, system unlocks required reader secrets.
4. System lists candidate `Manifest` objects.
5. Each object is parsed as signed `Manifest`.
6. Signature verification is performed with pinned key.
7. Valid manifests are converted to `Catalog` cache rows.
8. Unsigned or tampered manifests are rejected and reported.
9. Rebuild report shows accepted and rejected counts.
10. Snapshot inventory becomes searchable again.

### Outputs and State Transitions
- Missing/corrupt `Catalog` -> rebuilt `Catalog`.
- Valid signed `Manifest` -> searchable `Snapshot`.
- Unsigned/tampered manifest -> rejected evidence, no `Snapshot`.

### Exception/Cancel/Retry Path
- Cancel: stop listing/upserting; preserve already accepted rows if committed transaction model allows, otherwise rollback batch.
- Retry: rerun rebuild after reader credential, network, or key problem is fixed.
- Exception: `Catalog` conflict must prefer signed `Manifest` truth, while preserving enough evidence for diagnosis.

## 9. Restore Run

### Purpose
Restore a specific `Snapshot` to a specified DB destination safely, using `Restore Eligibility`, reader credentials, signature/integrity gates, destructive confirmation, and smoke validation.

### Actor
복구 담당자 or Windows 운영자; `Restorer` and `Engine Adapter` execute in `Data Plane`.

### CRUD/execute Authority
- Create: operator creates `Restore Run` from `Snapshot`.
- Read: operator reads `Restore Eligibility`, progress, result, and evidence.
- Update: system updates restore state and evidence.
- Delete: restore history should not be deleted as routine cleanup.
- Execute: `Dispatcher` executes with restore `Resource Permit`; UI cannot directly invoke restore tool.

### Inputs and Preconditions
- Selected committed `Snapshot`.
- `Restore Eligibility` passed: signature, integrity, support matrix, reader condition.
- Target `Connection Profile`.
- Destination DB name.
- Existing DB conflict policy.
- Destructive warning approval.
- Restore reader credential.
- For `Restore-as-Exec`, isolation or explicit approval boundary.

### Processing Steps
1. Operator selects `Snapshot` from Snapshots view.
2. System calculates `Restore Eligibility`.
3. Operator selects target `Connection Profile`, target DB name, and conflict policy.
4. System displays destructive warning and requires explicit confirmation.
5. Restore request is enqueued as `Work Item`.
6. `Dispatcher` acquires restore `Resource Permit`.
7. System loads signed `Manifest` from `Destination` using reader identity.
8. Signature and integrity gate runs before decrypt or restore.
9. DEK is unwrapped through KEK or Recovery Code path.
10. Artifact is decrypted and materialized.
11. `Engine Adapter` restores to target under the selected conflict policy.
12. Engine-specific smoke query runs.
13. `Restore Run` records succeeded/failed/cancelled and evidence.

### Outputs and State Transitions
- `Snapshot` -> `Restore Run(queued)`.
- `Restore Run(queued)` -> `leased` -> `running` -> `succeeded`/`failed`/`cancelled`.
- Successful restore stores smoke query evidence and measured RTO if available.
- Failed restore creates structured `Activity Event`.

### Exception/Cancel/Retry Path
- Gate failure: restore does not start; no unsafe bypass in general settings.
- Collision policy rejection: restore remains blocked until operator changes target name or policy.
- Cancel: stop restore process where engine supports it; record final target cleanup status.
- Retry: allowed after fixing reader credential, target permission, storage availability, or engine-specific restore issue.

## 10. Incident Diagnosis

### Purpose
Give operators a reliable process to diagnose backup, verification, restore, queue, and RPO incidents without hiding recent failures behind later success.

### Actor
Windows 운영자, 복구 담당자, platform 운영자.

### CRUD/execute Authority
- Create: system creates `Activity Event`, support bundle, and incident records if modeled.
- Read: operator reads recent failures, queue depth, stale `Lease`, `RPO Violation`, last L3 time, and redacted logs.
- Update: operator can annotate or mark reviewed if future model supports it.
- Delete: diagnostic evidence should follow retention rules, not ad hoc deletion.
- Execute: diagnostic screen can trigger probe, rebuild, verification, retry, cancel, or support bundle export through allowed application services.

### Inputs and Preconditions
- `Activity Event` stream.
- `Catalog`.
- Queue and `Work Item` states.
- `Backup Run`, `Restore Run`, verification results.
- Redacted logs, version data, config state.
- RPO/RTO policy data.

### Processing Steps
1. Operator opens diagnostic view.
2. System shows recent failure, recent success, queue depth, stale `Lease`, `RPO Violation`, and last L3 time separately.
3. Operator filters by engine, phase, retryability, `Database Target`, `Destination`, or `Backup Plan`.
4. System correlates `Activity Event` with `Backup Run`, `Restore Run`, `Snapshot`, and `Catalog`.
5. System classifies incident: connection/probe, discovery drift, preflight, dump, encryption, upload, commit, verification, restore gate, restore execution, scheduler/dispatcher, RPO.
6. Operator chooses action: retry, cancel, rerun probe, rebuild `Catalog`, run verification, start restore, or export support bundle.
7. System exports support bundle with redacted settings, logs, versions, and state only.
8. Resolution path records final `Activity Event`.

### Outputs and State Transitions
- Incident state becomes classified and actionable.
- `RPO Violation` persists as historical event even after later success.
- Stale `Lease` becomes recovered, expired, or failed.
- Support bundle becomes redacted diagnostic output.

### Exception/Cancel/Retry Path
- Cancel: diagnostic operation stops without changing run state unless explicit cancel action was chosen.
- Retry: each retry is routed through `Dispatcher`, not direct execution.
- Exception: if evidence is missing because `Catalog` was lost, operator is directed to Snapshot inventory/rebuild from signed `Manifest`.

# Missing Process Proposals

## Critical
- Engine Support Certification: formal process to move an engine/version/native tool combination from “implemented” to `Support Contract` passed, requiring probe, discover, preflight, backup, verify, restore, and validate evidence.
- Queue Recovery and Lease Reconciliation: explicit operational process for startup recovery, stale `Lease`, orphan child process handling, and idempotent `Work Item` recovery.
- Secret Lifecycle and Reader/Writer Separation: process for creating, rotating, testing, and revoking writer, reader, admin, KEK, DEK wrapping, and Recovery Code paths.

## Important
- Retention Enforcement: process for applying `Retention` per `Destination`, respecting Object Lock, and ensuring deletion eligibility does not conflict with compliance mode.
- RPO/RTO Review: process for reviewing `RPO Violation`, measured RTO, backup windows, and schedule drift by `Backup Plan`.
- Toolchain Health Management: process for native tool discovery, version pinning, hash/signature policy, and operator-facing remediation.
- Export/Import and Migration: process for moving operational SQLite truth and TOML export/import safely across versions.

## Nice-to-Have
- Restore Drill Campaign: scheduled L3 `Test-restore` process across selected `Backup Plan` groups with evidence reporting.
- Destination Capacity Forecasting: trend process for storage growth, object count, retention pressure, and destination throttling.
- Agent Readiness Assessment: pre-adoption process for future `Agent` trust boundary, network topology, and protocol compatibility.

## Dictionary Updates

| 행위 | 용어 | 정의 (제안) | 근거 |
|------|------|-------------|------|
| ADD | Engine Matrix | 엔진·서버 버전·native tool·기능별 검증 상태 표 | `Support Contract`의 실제 증적 단위가 필요하며, 검증되지 않은 조합을 지원으로 표시하면 안 된다. |
| ADD | RPO Violation | 마지막 유효 `Snapshot`이 정책 RPO를 넘긴 운영 상태 | 실행 성공/실패와 독립된 운영 판단이며, 나중 성공으로 사라지면 안 된다. |
| ADD | Scheduled Occurrence | `Backup Plan` 일정에서 계산된 특정 실행 예정 시각과 idempotency key의 단위 | 동일 scheduled occurrence 중복 실행과 중복 committed `Snapshot` 방지가 필요하다. |
| ADD | Verification Policy | `Snapshot`에 요구되는 `Verify Level`, 실행 직후 검증, 주기적 restore drill 기준을 묶은 정책 | 스펙은 검증 정책을 `Backup Plan` 구성요소로 쓰지만 사전에 독립 용어가 없다. |
| ADD | Destination Adapter | Local/S3/SSH/SFTP 등 `Destination`별 저장, 읽기, 삭제, list, Object Lock `Capability`를 구현하는 경계 | 스펙의 context map과 FR-DST가 공통 adapter contract를 요구한다. |
| ADD | Preflight | 실행 직전 엔진·목적지·공간·권한·일관성 옵션이 `Backup Run` 또는 `Restore Run` 조건을 만족하는지 확인하는 단계 | 연결 probe와 실제 실행 전 검사는 목적과 시점이 다르다. |
| ADD | Support Bundle | 비밀을 redaction한 설정·로그·버전·상태만 포함하는 진단 산출물 | Operability 요구사항의 incident diagnosis와 외부 지원 흐름에 필요하다. |
| REFINE | Snapshot | `Commit Protocol`을 통과한 artifact와 서명 `Manifest`의 논리 단위 | partial artifact를 `Snapshot`으로 노출하지 않는 규칙을 더 명확히 해야 한다. |

`#tags: database-backup, business-process, domain-dictionary, backup-plan, restore-run, dispatcher`