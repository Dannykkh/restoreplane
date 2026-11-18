# Section 05: SQL editor, query runs and manual transactions

## Background

Additive upgrade to Windows RestorePlane at08ba926. P2,P3,P5 are the owning problems. Existing Console inventories/native backup adapters are not general SQL or a virtual query grid.

## Requirements

- Tauri 유지; 현재 Workspace/Target 소유권과 기존 service_request 보호 계약을 보존한다. 임의 SQL은 현재 사용자 Workbench에서만 실행한다.
- Canonical handle binds native owner/window/workspace/target/profile revision/generation. JS identity claims are not authority.
- Generated values bind; scoped identifiers quote per dialect; exact TypedCell is independent from display text.
- No target exactly-once claim, no automatic write replay, no rollback inference from cancel/disconnect.
- Local operation state/version is authoritative; duplicated hints resync by pull. No new durable UI broker.
- All new limits are proposed/unmeasured until actual tests; never report parser/type/static-file presence as runtime proof.
- Section behavior: 원문 SQL 보존; parser gate+mode+destructive review; supported server readonly checks; manual txperconnection; separate cancellane/idle txdeadline; parameter/statement byte limits; sequencecredit ACK and stale/gapguard; optin encrypted savedqueries.

## Dependencies

Consume section04 shared result_stream.rs and result.attach/ack; extend through the declared contract without reimplementing credit/ACK.

section-02, section-03, section-04
Consumed artifacts/contracts must be present before the implementation wave starts; dependencies are runtime responsibilities, not permission to modify their owned files.

## Module Contract

- Provides: explicit SQL/Explain, generation-bound runs/cancel, manual transaction, bounded typed result stream
- Consumes: 02 certified sessions;03 journal;04 metadata/TypedCell/grid
- Owns: crates/restoreplane-workbench/src/query.rs; policy.rs; saved_queries.rs; apps/console/src/features/workbench/{QueryEditor,QueryTabs,QueryResults,TransactionBar}.tsx
- Composition Point: WorkbenchState query handlers and Workbench screen editor region
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
원문 SQL 보존; parser gate+mode+destructive review; supported server readonly checks; manual txperconnection; separate cancellane/idle txdeadline; parameter/statement byte limits; sequencecredit ACK and stale/gapguard; optin encrypted savedqueries.

Implement explicit query.run/Explain/analyze distinction, exact approved SQL, server readonly/capability enforcement, and review unknown/DDL/procedure/multiple statements under restrictive mode. No injection of LIMIT/order/wrappers into user buffers. Explicit generated previews have a separate bound generator path.
An explicitly read_only session may establish a native readonly transaction around one unchanged statement and observe the server's actual readonly hold, then perform cleanup; review discloses that control and preserves the server's default isolation. An existing readonly manual transaction supplies its own hold. ReadWrite/Full queries do not receive an implicit whole-script wrapper. Syntactic read candidates are not proof of harmless UDF/external effects, and cleanup/cancellation is not proof that such effects rolled back.
Manual transaction stays on one physical connection; classify implicit commits, aggregate partial scripts via ordered per-statement dispatch where boundaries are proven. Unseparable procedure is one conservative unit. No semicolon regex splitter.
ExecutionEpoch and cancel settlement barrier: complete A+cancel A and next-run B admission serialize; uncertain cancel retires socket. Write-bearing transaction becomes unknown, read-only result interrupted. Never assume rollback from socket close. COMMIT needs fresh authority; stale/expired owner may rollback/status only.
Proposed statement30s/lock2s/snapshot60s/idle transaction30s; certify adapters. Request queue32/global is bounded and cancel/control capacity reserved.
Reserve encoded byte credits before enqueue/serialization; highest contiguous ACK only within sent sequence range, duplicate/stale releases0, wrong-owner/future/overflow rejects. Unretained gap returns expired, never SQL reexecute. Terminal has reserved control capacity. Source completeness/consistency/outcome are separate.
Saved named queries are opt-in and user-DPAPI encrypted before SQLite/WAL; editor drafts and raw run text default memory-only. Completion comes from bounded metadata, saved query pagination from scoped SQLite indexes.

## Test Scenarios

Normal: permitted feature action yields typed scoped result. Error: wrong owner/revision/capability fails before driver dispatch. Edge: duplicate/gap/restart/cancel/disk failures preserve evidence and bounds.
1. query.exact_sql, T3 -> 사용자 SQL을 승인한 원문대로 실행하고 generated preview와 섞거나 LIMIT을 주입하지 않는다
2. query.execution_policy, T3 -> 모드·unknown/DDL/ANALYZE 정책을 Rust에서 검사하고 privileged backup service로 SQL을 넘기지 않는다
3. query.cancel_race, T8 -> 포화된 작업 중에도 정확한 query generation만 취소하고 취소 요청을 rollback 확인으로 표시하지 않는다
4. query.transaction_loss, T6 -> manual transaction을 같은 connection에 고정하며 reconnect/unknown commit 때 격리하고 자동 replay하지 않는다
5. query.stream_protocol, T7 -> duplicate/gap/stale batches를 정확히 처리하고 credit/cache 상한 및 terminal status를 유지한다
6. query.saved_privacy, T3 -> 자동 raw history/result 저장 없이 명시적 named query만 사용자 암호화 저장하고 secret canary를 로그에 남기지 않는다
Tests listed here are to implement and run; planning documents do not count as their evidence.

## Implementation Strategy

Implement typed input/state validation before wire adapters, then feature UI and one vertical live scenario. Add deterministic failure seams at actual risky boundaries. Preserve existing feature behavior. Record AC proof only from executed tests/observations; source presence is weak.

## Quality Gate

Run targeted Rust tests and fmt/Clippy for changed crates, frontend TypeScript/Vitest and relevant Playwright/native WebView checks. New driver changes require live certification on all declared engines. Run cargo-audit/cargo-deny for new/affected dependencies. Broaden only on actual regression risk. Browser fixtures do not prove native IPC, live DB, performance or OS accessibility.

## Risk & Rollback

Disable this capability/route at its typed feature gate without changing old backup formats or deleting unresolved operations. Preserve journal/partial evidence. Never relax an AC because a test is difficult; before implementation, update the design explicitly if a contradiction exists.

## Acceptance Criteria

- [ ] AC-05-1 사용자 SQL을 승인한 원문대로 실행하고 generated preview와 섞거나 LIMIT을 주입하지 않는다 — 근거: P2,P3,P5 — 검증: query.exact_sql, T3
- [ ] AC-05-2 모드·unknown/DDL/ANALYZE 정책을 Rust에서 검사하고 privileged backup service로 SQL을 넘기지 않는다 — 근거: P2,P3,P5 — 검증: query.execution_policy, T3
- [ ] AC-05-3 포화된 작업 중에도 정확한 query generation만 취소하고 취소 요청을 rollback 확인으로 표시하지 않는다 — 근거: P2,P3,P5 — 검증: query.cancel_race, T8
- [ ] AC-05-4 manual transaction을 같은 connection에 고정하며 reconnect/unknown commit 때 격리하고 자동 replay하지 않는다 — 근거: P2,P3,P5 — 검증: query.transaction_loss, T6
- [ ] AC-05-5 duplicate/gap/stale batches를 정확히 처리하고 credit/cache 상한 및 terminal status를 유지한다 — 근거: P2,P3,P5 — 검증: query.stream_protocol, T7
- [ ] AC-05-6 자동 raw history/result 저장 없이 명시적 named query만 사용자 암호화 저장하고 secret canary를 로그에 남기지 않는다 — 근거: P2,P3,P5 — 검증: query.saved_privacy, T3

## Files

- crates/restoreplane-workbench/src/query.rs
- policy.rs
- saved_queries.rs
- apps/console/src/features/workbench/{QueryEditor,QueryTabs,QueryResults,TransactionBar}.tsx
Shared composition files are lead-integrated per Module Contract. Each feature worker owns only its declared paths.
