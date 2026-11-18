# Architecture Expert Analysis — Database Management Workspace

Date: 2026-11-17. Inputs: current `spec.md`, `interview.md`, `research.md`, base and feature-v1 dictionaries.
Baseline: `08ba926`. Static source review only; no runtime/benchmark claims. Owner: architecture artifact-writer.
Disposition: **CONDITIONAL GO** for the user-owned Tauri Rust Workbench; resolve Critical findings in the API/domain contracts before implementation.

## Selected boundary and rejected alternative

The selected composition keeps arbitrary SQL in the current user's Tauri Rust process and existing backup/protection commands in the Windows service. This reduces the privileged service's SQL execution surface and preserves backup certification.
The earlier service-hosted adapter recommendation in `research/codebase.md` is an alternative, not the selected design; do not silently implement its service SQL path.
Existing Tauri Builder has one `service_request` handler and no management state: `apps/console/src-tauri/src/lib.rs:5`. Add Workbench managed state and a typed dispatcher at this composition point, not a second application shell.
Existing `service_request` remains the protection bridge: `apps/console/src-tauri/src/ipc_client.rs:6`. The Workbench command catalog has a distinct privilege/lifecycle contract.
Service authentication uses Windows SID/logon/session and owner/admin policy: `crates/restoreplane-service/src/ipc/auth.rs:35`, `:125`. Do not replace it with JS-supplied identity.

## Critical findings

### C1 — The metadata authorization lease is a new missing service contract
- **Evidence:** existing ownership checks bind target to Workspace: `crates/restoreplane-catalog/src/repositories/workspace.rs:213`; active Workspace validation is a separate method at `:180`.
- **Evidence:** `ConsoleConnection` is a display projection, not a complete connect descriptor: `crates/restoreplane-catalog/src/repositories/console_query.rs:29`. `SelectedDatabaseTarget` has a source fingerprint but no explicit profile revision: `crates/restoreplane-catalog/src/repositories/configuration.rs:300`.
- **Impact:** a frontend-generated endpoint/revision or membership-only check could open an archived/moved/changed resource under a stale authorization lease.
- **Recommendation:** add a service `workbench.access_prepare`-style typed read operation returning canonical nonsecret connection metadata, active-state proof, revision/epoch, target identity and expiry. Derive owner/session from the pipe principal; bind the lease to the issuing Tauri instance and requested target. Treat it as in-memory validated authority, not a durable bearer token.
- **Acceptance:** archived Workspace, revised endpoint/TLS/root certificate, target reassignment, stale epoch and cross-window reuse all refuse before opening/dispatching. A timestamp alone is not a revocation/version mechanism.

### C2 — “User-owned” must be enforced against actual process privilege
- **Evidence:** service encryption explicitly uses `CRYPTPROTECT_LOCAL_MACHINE`: `crates/restoreplane-service/src/secrets/dpapi.rs:8`, `:23`; it cannot be reused as current-user protection by renaming a store.
- **Impact:** launching the Console elevated or under another account changes DB/file rights; a JS `ownerSid` cannot prove current-user confinement. SQLite/file access and extension-capable SQL also carry local process authority.
- **Recommendation:** derive OS user/logon/integrity identity in Rust; bind credentials, journal and handles to that identity. Define whether elevated execution is refused for Workbench or explicitly supported as a different identity. Implement a separate current-user DPAPI provider and user ACL path; never export/reprotect service secrets automatically.
- **Acceptance:** normal/elevated/different-user launches, credential store relocation, revoked secret and cross-user handle requests do not reuse authority. Database roles/file ACLs remain the real privilege boundary; mode classification is only accident prevention.

### C3 — Lease expiry, transaction lifetime and cancellation need a single state contract
- **Evidence:** F1 proposes 30-second authority leases, while F3 allows live manual transactions. Local work fencing is conditional on a local lease: `crates/restoreplane-catalog/src/repositories/runtime.rs:2526`; it cannot fence a remote DB transaction.
- **Impact:** blanket expiry cleanup can claim rollback without confirmation, strand a manual transaction, or send a late Cancel to a newer query on the same socket.
- **Recommendation:** distinguish authorization lease, connection generation and transaction state. Check fresh authority before each new dispatch; expiry blocks new work but always permits cancel/rollback/disconnect/status. Specify commit policy on expired authority, renewal failure and profile invalidation. Never migrate transactions on reconnect. Bind cancellation to the exact executing query/generation under the same session ownership lock; keep the cancel lane independent.
- **Acceptance:** expiration while idle/running/in-transaction/commit_requested, cancel-after-completion, queued cancellation, reconnect and service loss have explicit outcomes. Cancellation intent, socket closure and lease expiry never imply rollback evidence.

### C4 — The new local journal cannot inherit remote-write atomicity
- **Evidence:** generic IPC dispatch reserves locally, calls the operation, then records completion: `crates/restoreplane-service/src/ipc/dispatch.rs:165`, `:185`, `:211`. Typed backup enqueue's local atomic transaction is specific to its local records: `crates/restoreplane-catalog/src/repositories/ipc.rs:2189`.
- **Impact:** reusing the dispatcher pattern without a remote commit state machine can replay an already committed edit after a crash. An interrupted manual transaction may have earlier confirmed statement results but an unknown final commit.
- **Recommendation:** persist operation admission/digest and dispatch intent before wire, use one journal writer actor and guarded version transitions, and recover sent writes as `outcome_unknown` absent decisive evidence. Keep statement outcomes separate from containing transaction outcome. Block dependent apply operations and prohibit automatic mutation replay.
- **Acceptance:** crash before send/after send/around COMMIT/after commit before local completion, disk-full and journal corruption preserve honest uncertainty. Same visible values are not proof against duplicated triggers or sequence effects.

## Important findings

### I1 — Bounded transport needs two explicit contracts, not just a Tauri Channel
- Existing service payload cap is 1MiB (`crates/restoreplane-application/src/payload.rs:5`); its 64KiB stream chunk is a frame decoder limit, not result backpressure (`crates/restoreplane-service/src/ipc/stream.rs:6`).
- Keep service metadata/protection IPC unchanged; Workbench uses typed commands plus a result producer/consumer contract. Define reserved credit before serialization, queue capacities, terminal delivery, consumer disappearance and ACK ownership.
- Byte credit bounds encoded transit, not decoded JS/Rust heap. Define process-global versus per-result budgets and account for strings, offsets, object overhead, retained drafts, selection, sorting and serialization copies. Bound queued requests as well as running queries.
- The proposed 256KiB batch/8MiB outstanding/32MiB UI/64MiB native limits are hypotheses. Add named hardware/WebView fixtures and slow/no-ACK, wide/LOB, tab-switch and many-result measurements; do not mark them achieved by type definitions.

### I2 — Current UI paging is reusable; the result grid and lazy catalog are new
- Scope pages are limited to 100 (`crates/restoreplane-catalog/src/repositories/backup_scope.rs:569`), but the editor appends pages (`apps/console/src/components/BackupScopeEditor.tsx:158`) and maps every loaded object (`:361`).
- `InventoryTable` uses ordinary DOM rows; `VirtualActivityList` also maps all supplied events (`apps/console/src/components/InventoryTable.tsx:70`; `apps/console/src/components/VirtualActivityList.tsx:23`). Reusing them does not provide row/column virtualization.
- Metadata discovery collects the original result under a 15-second timeout; MySQL's trusted metadata stdout cap is 1MiB (`crates/restoreplane-service/src/operations/native.rs:19`; `crates/restoreplane-source-mysql/src/meta.rs:113`). A new lazy catalog API must bypass these full-inventory assumptions without changing backup discovery evidence.
- Use bounded metadata/result LRUs, stable ordering and cursor-bound identity. No key means bounded preview, not invented exhaustive keyset pagination; live pages do not form a snapshot.

### I3 — Type and edit capabilities need per-engine evidence
- Existing backup metadata and result projections do not supply lossless typed arbitrary cells or reliable column origin. Native dump processing (`crates/restoreplane-service/src/backup.rs:757`) is not an interactive driver API.
- Put dialect quoting/bound parameters, lossless codecs, key/origin metadata, matched-row semantics, transaction/isolation support and cancel behavior behind explicit certified capabilities. Frontend consumes discriminated tagged values, not JS `number` for arbitrary integers/decimal values.
- Record original typed values and schema identity for drafts. Refuse unsupported generated/default/LOB/key types; require transactional storage and exactly-one matched row. Original-value comparison detects current mismatch, not ABA/history.
- User SQL stays unchanged. Multiple statements, procedures, implicit commits, server-side files/extensions and Explain Analyze need explicit classifications/capability restrictions; no parser-only “safe execution” claim.

### I4 — Persistence and multi-window ownership require one local coordinator
- Current Tauri backend has no managed state or local Workbench journal dependencies (`apps/console/src-tauri/src/lib.rs:5`; `apps/console/src-tauri/Cargo.toml:21`). Add one coordinator for sessions, permits, journal writes and window ownership.
- Current capability config names only `main` (`apps/console/src-tauri/capabilities/default.json:5`); explicitly scope and verify new custom commands. Derive invoking window in Rust and never trust an input window label as authority.
- Store only metadata/digests by default. Encrypt named saved-query content before SQLite insertion so WAL/journal/temp/backup paths never receive raw query bodies; DPAPI is not transparent whole-database encryption. Redact bound values, server errors and credential-shaped strings from diagnostics.
- Window close removes ownership and drains/cancels associated streams without holding a global mutex over driver I/O. App restart invalidates live handles; durable journal outcomes remain queryable. Define multiple app-instance coordination or enforce a single writer instance per user journal.

## Integration recommendation

Keep the selected split: Tauri managed Workbench coordinator -> bounded driver sessions/user credential provider/local journal; existing service bridge -> canonical identity leases and backup/restore actions only. Share stable domain IDs and pure codecs/contracts where appropriate, not service secret/runtime ownership.
Gate writable release on C1–C4 plus engine transaction/type/cancel certification. A read-only slice may precede editing only with the same authority, transport and resource bounds; it must not bypass the state contracts later required by editing.

## Dictionary Updates

| Action | Term | Proposed definition / reason |
|---|---|---|
| ADD | Workbench Access Lease | Short-lived service-issued validation of principal/Workspace/target/canonical metadata revision; separate from worker Lease and never proof a remote transaction stopped. |
| ADD | Workbench Transaction | One target connection's manual/generated transaction identity, isolation, state and outcome; survives only in the journal as evidence after disconnect, never as a resumable session. |
| REFINE | Connection Generation | Changes on reconnect/invalidation; cancellation and result application require exact generation and query identity checked atomically with session ownership. |
| REFINE | Credit Window | Encoded in-flight producer allowance reserved before enqueue; decoded retained-memory budgets and terminal/control delivery are separate contracts. |
| REFINE | Operation Journal | Current-user durable intent/version/outcome and local delivery metadata; no raw SQL/result history by default and no cross-store atomicity claim. |
| CONFLICT | Lease / Transaction Owner | Existing terms describe service Work Item/local Operational Store ownership. Qualify Workbench access/session/transaction terms rather than expanding those existing definitions silently. |
| MERGE | none | No duplicate feature term requires merging in this review. |

Validation: source/document inspection complete; application tests, new drivers, fault injection and performance benchmarks **NOT RUN** in this design review.
