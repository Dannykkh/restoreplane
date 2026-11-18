# Section 03: Durable operation outcomes and message delivery

## Background

Additive upgrade to Windows RestorePlane at08ba926. P5,P6 are the owning problems. Existing Console inventories/native backup adapters are not general SQL or a virtual query grid.

## Requirements

- Tauri 유지; 현재 Workspace/Target 소유권과 기존 service_request 보호 계약을 보존한다. 임의 SQL은 현재 사용자 Workbench에서만 실행한다.
- Canonical handle binds native owner/window/workspace/target/profile revision/generation. JS identity claims are not authority.
- Generated values bind; scoped identifiers quote per dialect; exact TypedCell is independent from display text.
- No target exactly-once claim, no automatic write replay, no rollback inference from cancel/disconnect.
- Local operation state/version is authoritative; duplicated hints resync by pull. No new durable UI broker.
- All new limits are proposed/unmeasured until actual tests; never report parser/type/static-file presence as runtime proof.
- Section behavior: canonical request digest+dispatch intent, monotonic guarded state; operation versions are authoritative and UI hints may duplicate; gap/status pulls journal; no new persistent UI outbox/inbox broker; process/journal epochs, retention, disk faults and unknown quarantine.

## Dependencies

section-02
Consumed artifacts/contracts must be present before the implementation wave starts; dependencies are runtime responsibilities, not permission to modify their owned files.

## Module Contract

- Provides: operation admission/idempotency/state versions/restart reconciliation; no durable UI message broker
- Consumes: 02 user state SQLite + typed owner/target handles
- Owns: crates/restoreplane-workbench/src/journal.rs; events.rs; recovery.rs; tests/journal_recovery.rs
- Composition Point: WorkbenchState operation boundary before driver dispatch
- Shared composition edits: implementation lead owns workspace Cargo.toml/Cargo.lock, Tauri lib.rs/capability registrations, App.tsx/route/types, existing service command routing. Feature workers return integration patch requests; do not edit them concurrently.

## Reference Libraries

Existing Rust std/Tokio, application/domain IDs and error vocabulary first; existing tokio-postgres/rusqlite/secrets interfaces reused only with scope and current advisory checks. New mysql_async and TanStack Virtual are candidates with verified upstream docs, not installed by planning.
- https://github.com/blackbeam/mysql_async (streaming/prepared/TLS)
- https://github.com/tanstack/virtual (row+column virtualization, stable keys)
- https://v2.tauri.app/develop/calling-frontend/ (Channels are not app credit/ACK)
No DBDelve GPL code or dependency pins are copied. Exact new versions/licenses/advisories are implementation gates.

## Implementation

Internal IPC: workbench_request(request,onResult?) with the closed Request envelope {protocolVersion:1,requestId,command,payload,attemptToken?} dispatched in Rust. No HTTP API/MCP. Errors use WBError {code,messageKey,retryable,operationId,field,details} in {ok:false,requestId,error}; no raw SQL/parameters/driver secrets in default diagnostics.
Lists use opaque cursor, limit100/default200/max and512KiB/page (metadata legacy service pages max100); allowed sort/filter are explicit enums/catalog IDs. No N+1 catalog fetch per displayed row. Invalid/foreign/expired cursor returns a typed error.
Data batches max256KiB, credit8MiB, UI retained32MiB and native buffers64MiB proposed; sequence and generation bind all ACKs. Rows/bytes/cells and queued requests are independently bounded.
canonical request digest+dispatch intent, monotonic guarded state; operation versions are authoritative and UI hints may duplicate; gap/status pulls journal; no new persistent UI outbox/inbox broker; process/journal epochs, retention, disk faults and unknown quarantine.

Key scope owner+workspace+target+kind+attempt; canonical digest and native-issued epoch/expiry token. Duplicate same payload returns existing operation, changed payload conflicts. Dispatch marker persists before wire; no sent operation can be taken over for reexecution.
Guard state_version in one SQLite writer transaction. Metadata-only persisted fields; raw bodies/values/default query history absent. Separate transaction and ordered StatementOutcome records. Child import batches carry parent identity,index,byte/row range,digest and stop after unknown.
After socket/process/commit response loss, preserve unknown absent decisive target proof. Operator acknowledgment does not promote unknown; no automatic resend. Terminal read/export completed differs from committed write.
UI state hints can duplicate/be lost; ignore old versions, operation.get/list on gap/reconnect. Result frame ACK is a separate ephemeral protocol. No local outbox/inbox broker tables; existing service notification outbox untouched.
Proposed retry token24h/terminal metadata30days; old epoch/expired token cannot admit fresh execution. Unknown and referenced active records survive pruning; quota stops new admission. Restore/corruption starts write-disabled/fresh epoch until explicit reconciliation. DPAPI failure has no plaintext fallback. W2 emits deterministic dispatch/COMMIT/local-finalize faults and M3 observations.

## Test Scenarios

Normal: permitted feature action yields typed scoped result. Error: wrong owner/revision/capability fails before driver dispatch. Edge: duplicate/gap/restart/cancel/disk failures preserve evidence and bounds.
1. journal.idempotency, T5 -> 같은 scoped key+payload는 같은 operation을 반환하고 다른 payload는 IDEMPOTENCY_CONFLICT로 거부한다
2. journal.version_resync, T7 -> operation state와 version을 함께 commit하고 duplicate/old hint를 무시하며 gap/reconnect는 journal 상태로 복구한다
3. journal.commit_loss, T6 -> dispatch/COMMIT 후 crash·응답유실을 unknown으로 복구하고 target SQL을 자동 재실행하지 않는다
4. journal.disk_failure, T6 -> journal 손상·disk-full이면 새 쓰기를 거부하고 기존 operation의 불확실성을 숨기지 않는다
5. journal.secret_canary, T3 -> raw SQL·credentials·row values를 default journal/diagnostics에 영속 저장하지 않는다
Tests listed here are to implement and run; planning documents do not count as their evidence.

## Implementation Strategy

Implement typed input/state validation before wire adapters, then feature UI and one vertical live scenario. Add deterministic failure seams at actual risky boundaries. Preserve existing feature behavior. Record AC proof only from executed tests/observations; source presence is weak.

## Quality Gate

Run targeted Rust tests and fmt/Clippy for changed crates, frontend TypeScript/Vitest and relevant Playwright/native WebView checks. New driver changes require live certification on all declared engines. Run cargo-audit/cargo-deny for new/affected dependencies. Broaden only on actual regression risk. Browser fixtures do not prove native IPC, live DB, performance or OS accessibility.

## Risk & Rollback

Disable this capability/route at its typed feature gate without changing old backup formats or deleting unresolved operations. Preserve journal/partial evidence. Never relax an AC because a test is difficult; before implementation, update the design explicitly if a contradiction exists.

## Acceptance Criteria

- [ ] AC-03-1 같은 scoped key+payload는 같은 operation을 반환하고 다른 payload는 IDEMPOTENCY_CONFLICT로 거부한다 — 근거: P5,P6 — 검증: journal.idempotency, T5
- [ ] AC-03-2 operation state와 version을 함께 commit하고 duplicate/old hint를 무시하며 gap/reconnect는 journal 상태로 복구한다 — 근거: P5,P6 — 검증: journal.version_resync, T7
- [ ] AC-03-3 dispatch/COMMIT 후 crash·응답유실을 unknown으로 복구하고 target SQL을 자동 재실행하지 않는다 — 근거: P5,P6 — 검증: journal.commit_loss, T6
- [ ] AC-03-4 journal 손상·disk-full이면 새 쓰기를 거부하고 기존 operation의 불확실성을 숨기지 않는다 — 근거: P5,P6 — 검증: journal.disk_failure, T6
- [ ] AC-03-5 raw SQL·credentials·row values를 default journal/diagnostics에 영속 저장하지 않는다 — 근거: P5,P6 — 검증: journal.secret_canary, T3

## Files

- crates/restoreplane-workbench/src/journal.rs
- events.rs
- recovery.rs
- tests/journal_recovery.rs
Shared composition files are lead-integrated per Module Contract. Each feature worker owns only its declared paths.
