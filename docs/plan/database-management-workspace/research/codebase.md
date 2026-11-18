# Codebase Research — Database Management Workspace

Date: 2026-11-17. Source baseline: `main` / `08ba92615a2585765fa8cb509df37a0671f10454`.
Role: artifact-writer. Scope: source inspection; no product source, configuration or runtime changed.
Inputs: `spec.md`, `research-decision.md`, `index.md`, then `codemap/index.md` and relevant categories.
The task's `initial-spec.md` does not exist; the initial requirements are in `spec.md`.
Status language: **Observed** is implemented source; **Proposed** is an integration recommendation.

## 1. Existing functionality and missing product boundary

- **Observed:** RestorePlane already has connection profiles, Workspace ownership, database discovery, selected targets, Backup Scopes, scheduled backups, verification and restore.
- **Observed:** the application command catalog exposes explicit operations, not an arbitrary database SQL endpoint: `crates/restoreplane-application/src/commands.rs:63`.
- **Observed:** frontend API methods list connections, targets, discovery results and Backup Scopes: `apps/console/src/lib/api.ts:283`.
- **Observed:** `console_query` is the operational catalog's read/projection repository. It returns `ConsoleConnection`, `ConsoleDatabaseTarget` and health summaries, not user SQL results: `crates/restoreplane-catalog/src/repositories/console_query.rs:20`.
- **Observed:** `ServiceApplication::console_query()` obtains that repository from `OperationalStore`: `crates/restoreplane-service/src/application.rs:1878`.
- **Observed:** `NativeConnectionInspector` implements fixed engine probes and metadata discovery: `crates/restoreplane-service/src/operations/native.rs:329`.
- **Finding:** the requested schema browser, general SQL editor, interactive sessions, general result grid and safe arbitrary-table editing are new capabilities.
- **Boundary:** do not rename or expose the existing `console_query` repository as a general SQL executor; it owns control-plane inventory.

## 2. Existing composition points to reuse

### 2.1 Tauri and application command routing

- **Observed:** the frontend calls one Tauri `service_request(command, payload, idempotency_key)` adapter: `apps/console/src-tauri/src/ipc_client.rs:6`.
- **Observed:** the adapter validates names with `ApplicationCommand::try_from` and uses `IpcClient::production().execute`: `apps/console/src-tauri/src/ipc_client.rs:12`.
- **Observed:** the shared client owns request construction and IPC execution: `crates/restoreplane-ipc-client/src/lib.rs:222`.
- **Observed:** the service dispatcher applies authority and distinguishes read, ordinary mutation and typed enqueue/destructive paths: `crates/restoreplane-service/src/ipc/dispatch.rs:122`.
- **Proposed:** add explicit management commands to the application catalog and consume them through this adapter; do not create a second ad hoc frontend-to-driver channel.
- **Proposed:** the session/result data plane may extend the transport, but its ownership, principal and resource validation must remain in the service composition.

### 2.2 Workspace and target identity

- **Observed:** Workspace is persisted and has paged inventory and active-state validation: `crates/restoreplane-catalog/src/repositories/workspace.rs:42`, `:180`.
- **Observed:** `require_connection` and `require_database_target` enforce Workspace resource membership: `crates/restoreplane-catalog/src/repositories/workspace.rs:197`, `:213`.
- **Observed:** connections are inserted with Workspace-bound database authentication: `crates/restoreplane-catalog/src/repositories/configuration.rs:1097`.
- **Observed:** the scope object API verifies the target belongs to the requested Workspace before querying: `crates/restoreplane-service/src/application.rs:1607`.
- **Proposed:** management sessions reference existing `workspaceId`, `connectionProfileId` and `databaseTargetId`; session/result/edit handles must also carry a principal and generation.
- **Proposed:** reuse target identity and backup linkage without treating a target fingerprint as a transaction snapshot or a row concurrency token.

### 2.3 Credential ownership

- **Observed:** `ServiceSecretStore` owns encrypted object storage and ACL application: `crates/restoreplane-service/src/secrets/store.rs:88`, `:98`.
- **Observed:** `resolve(id, expected_purpose)` validates purpose/state, decrypts through DPAPI and checks the plaintext fingerprint: `crates/restoreplane-service/src/secrets/store.rs:219`.
- **Observed:** the public secret representation contains reference metadata rather than plaintext: `crates/restoreplane-service/src/secrets/store.rs:24`.
- **Proposed:** interactive adapters resolve credentials inside the service and return only capability/error metadata to Tauri.
- **Proposed:** reuse the provider and reference mechanism; explicitly decide whether interactive credentials need a separate purpose and least-privilege role rather than silently granting backup credentials to editing.

## 3. Native backup pipeline versus interactive drivers

- **Observed:** PostgreSQL/MySQL/MariaDB backup execution verifies the native tool and dependencies, spawns a source artifact, then encrypts it into spool: `crates/restoreplane-service/src/backup.rs:757`.
- **Observed:** the service invokes `run_artifact_until_deadline` on that artifact, with pipeline cancellation and an execution deadline: `crates/restoreplane-service/src/backup.rs:797`.
- **Observed:** real table rows do not pass through the React UI during backup; the UI requests work and observes metadata/progress.
- **Observed:** PostgreSQL table discovery uses a database connection but returns metadata observations: `crates/restoreplane-source-pg/src/lib.rs:145`.
- **Observed:** MySQL metadata discovery runs a fixed `information_schema.tables` statement and parses a complete text result: `crates/restoreplane-source-mysql/src/meta.rs:211`.
- **Finding:** these source adapters are not reusable interactive session APIs. A short probe or native dump cannot supply a reusable cursor, typed cells, cancellation handle or manual transaction.
- **Proposed:** introduce a management driver boundary, with per-engine adapters and explicit capabilities, beside existing backup source adapters.
- **Proposed:** keep native dump/restore certification and artifact evidence intact; a successful interactive SELECT does not certify backup or restore.
- **Proposed:** separate dialect spelling from transport/driver capabilities; model typed NULL, exact numerics, bytes, temporal values and origin/key metadata before adding editable cells.
- **Proposed:** use bounded blocking/async execution lanes and cancellation independent of the saturated query lane; keep native backup resource permits separately accounted.

## 4. Durable consistency infrastructure: reusable pieces and limits

### 4.1 Idempotency and duplicate requests

- **Observed:** the UI has a mutation attempt registry that retains an idempotency key for the same attempt/signature: `apps/console/src/lib/mutationAttempts.ts:15`.
- **Observed:** generic mutation dispatch canonicalizes payloads and reserves the request before execution, then completes the stored outcome: `crates/restoreplane-service/src/ipc/dispatch.rs:165`.
- **Observed:** matching completed requests reuse the stored response and expose deduplication: `crates/restoreplane-service/src/ipc/dispatch.rs:216`.
- **Observed:** IPC request identity includes principal hash, command name, idempotency key and payload digest: `crates/restoreplane-catalog/src/repositories/ipc.rs:51`.
- **Observed:** manual backup enqueue uses one immediate SQLite transaction for the ledger/workflow transition and rejects a reused key with another payload: `crates/restoreplane-catalog/src/repositories/ipc.rs:2189`.
- **Limit:** generic reserve → external execution → completion spans a crash window; it does not atomically commit a remote database write with the local SQLite ledger.
- **Proposed:** reuse keys/digests/outcome states, but create a typed edit operation ledger with `prepared`, `executing`, `committed`, `failed` and `outcome_unknown` semantics.
- **Proposed:** after a lost commit acknowledgment, do not automatically replay a mutation; reconcile using server evidence or a target-side idempotency mechanism explicitly supported for that operation.

### 4.2 Worker ownership and fencing

- **Observed:** worker claiming increments attempt and fence and updates only the observed previous state/fence: `crates/restoreplane-catalog/src/repositories/runtime.rs:2522`.
- **Observed:** resource heartbeat updates require matching lease owner/boot/fence and an unexpired lease: `crates/restoreplane-catalog/src/repositories/runtime.rs:3210`.
- **Reusable:** local ownership checks, retry classification, cancellation requests, persisted work and startup reconciliation patterns.
- **Limit:** a local fence does not revoke SQL already sent to another database or prevent that server from committing; native-process containment is not database transaction fencing.
- **Proposed:** interactive session generations prevent stale messages/results from attaching to a newly connected session, while server cancellation/rollback is tracked separately.
- **Proposed:** session-bound manual transactions must not migrate to a different connection or silently resume after service restart.

### 4.3 Outbox, inbox and cross-database transactions

- **Observed:** notification enqueue deduplicates `(notification_rule_id, deduplication_key)` and conflicts on differing payloads in one local transaction: `crates/restoreplane-catalog/src/repositories/notification.rs:353`.
- **Observed:** notification delivery claims use persisted attempts/fences and retryable state: `crates/restoreplane-catalog/src/repositories/notification.rs:400`.
- **Finding:** this is a notification outbox. It is not an existing general interactive-write outbox or a distributed transaction coordinator.
- **Finding:** the inspected command/IPC/catalog path has no general result-message inbox for query batches with sequence/acknowledgment semantics; the request ledger deduplicates commands, not streaming batches.
- **Proposed:** add stream identity, monotonically increasing batch sequence, ACK/credit and bounded replay/resume policy; duplicate or old-generation batches must not append rows twice.
- **Proposed:** do not imply XA/2PC or all-or-nothing edits across databases. First scope is a transaction on one target session; multi-target operations expose per-target outcomes and partial completion.
- **Proposed:** local outbox/inbox may coordinate durable workflow messages, but cannot manufacture atomicity between SQLite and an unrelated database.

## 5. Paging, discovery and current UI gaps

- **Observed:** `backup_scope.object_list` accepts explicit discovery ID, cursor, limit, search, schema and eligibility: `crates/restoreplane-service/src/application.rs:2422`.
- **Observed:** the filtered repository validates page size 1–100, uses ordinal keyset pagination and fetches limit+1: `crates/restoreplane-catalog/src/repositories/backup_scope.rs:569`.
- **Observed:** filtered count and search concatenate names through `lower(...) LIKE ...`; each page performs a count query: `crates/restoreplane-catalog/src/repositories/backup_scope.rs:619`.
- **Observed:** discovery admission caps objects at 100,000 and explicit selections at 1,000: `crates/restoreplane-catalog/src/repositories/backup_scope.rs:16`.
- **Observed:** discovery itself collects metadata before persisting it; the default native inspector timeout is 15 seconds: `crates/restoreplane-service/src/operations/native.rs:19`, `:370`.
- **Observed:** trusted MySQL metadata output is capped at 1MiB and truncated output fails: `crates/restoreplane-source-mysql/src/meta.rs:113`, `:165`.
- **Limit:** these are validation/resource limits, not proof of acceptable performance at 100,000 objects. Discovery and substring-count latency can be the bottleneck before rendering.
- **Observed:** BackupScopeEditor fetches another 100 objects and merges them into retained state: `apps/console/src/components/BackupScopeEditor.tsx:158`.
- **Observed:** object rows are mapped directly to DOM: `apps/console/src/components/BackupScopeEditor.tsx:361`.
- **Observed:** InventoryTable maps supplied rows/columns into a regular table; it has keyboard selection but no viewport virtualization: `apps/console/src/components/InventoryTable.tsx:70`.
- **Observed:** despite its name, VirtualActivityList maps all supplied items; it does not calculate a viewport window: `apps/console/src/components/VirtualActivityList.tsx:23`.
- **Reusable:** request sequence checks already discard obsolete scope-page responses: `apps/console/src/components/BackupScopeEditor.tsx:160`.
- **Proposed:** bounded page caches and row+column virtualization are new result-grid work; the current inventory components are not a million-row spreadsheet.
- **Proposed:** distinguish stable keyset browsing, unordered ad hoc result streams, client-side sorting and server sorting. A cursor requires deterministic ordering and snapshot/consistency disclosure.

## 6. Transport bounds and integration decisions

- **Observed:** canonical request/response payloads cap at 1MiB: `crates/restoreplane-application/src/payload.rs:5`; `crates/restoreplane-service/src/ipc/response.rs:123`.
- **Observed:** `MAX_STREAM_CHUNK_BYTES = 64KiB` belongs to the framed request decoder. It does not implement query-result streaming or consumer backpressure: `crates/restoreplane-service/src/ipc/stream.rs:6`.
- **Observed:** operational inventory pages cap at 100 and response page budget at 768KiB: `crates/restoreplane-catalog/src/repositories/console_query.rs:11`.
- **Proposed:** preserve the command control plane; add bounded result batches or pull pages with byte, row, cell, column and active-result budgets instead of one huge JSON response.
- **Proposed:** result cells larger than the inline budget use explicit bounded value fetch/export handles; row virtualization alone does not bound Rust/JS memory.
- **Proposed:** persistent query history, schema cache and optional result caches need explicit retention, redaction and Workspace deletion handling; never persist passwords or unbounded result bodies by default.

## 7. Concrete integration outline

1. Extend `ApplicationCommand`, the shared IPC client and existing `service_request` contract with typed management session/catalog/query/result/edit operations.
2. Add a Rust management application/domain boundary; reuse Workspace/configuration/secret repositories through existing service composition.
3. Add engine adapters for PostgreSQL, MySQL/MariaDB and SQLite, with dialect-specific quoting, bound values, catalog-origin metadata, cancellation and transaction capability contracts.
4. Add isolated session ownership, generation/lease expiry, result handles, bounded caches and streaming credit; enforce per-Workspace and global budgets independently of backup workers.
5. Add schema browser, SQL editor and virtual result grid in the Tauri/React shell; retain backup screens and link selected database/tables into existing Scope workflows.
6. Add generated-edit preview, primary/unique-key requirements, original-value/version conflict checks and exactly-one-row affected validation within one target transaction.
7. Integrate typed local mutation ledger and outcome reconciliation before enabling writes; no global transaction promise and no automatic replay after unknown remote commit.
8. Reuse backup/restore preflight and evidence APIs as separate actions; a restored test target can be opened in management only after explicit target identity checks.

## 8. Validation evidence and remaining research

- Existing scope tests verify pagination/search/eligibility with a small fixture: `crates/restoreplane-catalog/tests/backup_scope_repository.rs:32`.
- Existing IPC live tests cover real named-pipe concurrency: `crates/restoreplane-ipc-client/tests/diagnostics_roundtrip.rs:90`.
- Existing mutation attempt tests provide a retry/idempotency starting point: `apps/console/src/lib/mutationAttempts.test.ts:1`.
- **NOT RUN:** product tests, live database operations and performance measurements; this artifact is static planning research.
- **Required new evidence:** large metadata inventories, wide results, slow/blocked queries, cancel under saturation, repeated/out-of-order batches, low-memory eviction and frontend frame/interaction latency.
- **Required new evidence:** concurrency conflicts, lost COMMIT acknowledgment, server reconnect, duplicate mutation requests and cross-Workspace handle rejection.
- **Remaining decisions:** driver selection/API documentation, engine isolation guarantees and Tauri result-delivery mechanism are delegated to official-document research and subsequent design sections.
- **Acceptance boundary:** the presence of reusable idempotency, fencing and outbox infrastructure does not make general database editing implemented or exactly-once across stores.
