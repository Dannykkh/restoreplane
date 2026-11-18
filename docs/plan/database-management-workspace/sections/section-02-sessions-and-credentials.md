# Section 02: User-owned sessions and target bindings

## Background

Additive upgrade to Windows RestorePlane at08ba926. P1,P3,P6 are the owning problems. Existing Console inventories/native backup adapters are not general SQL or a virtual query grid.

## Requirements

- Tauri 유지; 현재 Workspace/Target 소유권과 기존 service_request 보호 계약을 보존한다. 임의 SQL은 현재 사용자 Workbench에서만 실행한다.
- Canonical handle binds native owner/window/workspace/target/profile revision/generation. JS identity claims are not authority.
- Generated values bind; scoped identifiers quote per dialect; exact TypedCell is independent from display text.
- No target exactly-once claim, no automatic write replay, no rollback inference from cancel/disconnect.
- Local operation state/version is authoritative; duplicated hints resync by pull. No new durable UI broker.
- All new limits are proposed/unmeasured until actual tests; never report parser/type/static-file presence as runtime proof.
- Section behavior: 사용자 DPAPI credential ref; service secret export 금지; owner/window/workspace/revision/generation handles; fresh mutation authorization/read lease; driver capabilities read-only/TLS/type/cancel verification; bounded runtime/connection permits.

## Dependencies

none — independently implementable in Wave1.
Consumed artifacts/contracts must be present before the implementation wave starts; dependencies are runtime responsibilities, not permission to modify their owned files.

## Module Contract

- Provides: typed Workbench dispatcher, target binding, user-owned driver/session, user-state schema v1
- Consumes: existing Workspace/Target redacted metadata via service authorization; existing TLS and ID vocabulary
- Owns: crates/restoreplane-workbench/src/lib.rs; session.rs; credentials.rs; store.rs; driver/{mod,postgres,mysql,sqlite}.rs; apps/console/src-tauri/src/workbench.rs; crate manifests; restoreplane-domain/application qualified Workbench contracts and restoreplane-secrets CurrentUser provider (integration-owned)
- Composition Point: Tauri Builder.manage(WorkbenchState) and explicit workbench_request; existing service catalog adds only workbench.binding.authorize (no SQL)
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
사용자 DPAPI credential ref; service secret export 금지; owner/window/workspace/revision/generation handles; fresh mutation authorization/read lease; driver capabilities read-only/TLS/type/cancel verification; bounded runtime/connection permits.

OS principal: derive SID/logon/integrity and invoking window in Rust. Refuse elevated Workbench before user directory/SQLite/DPAPI initialization; existing backup/admin screens still function. Extend restoreplane-secrets with explicit CurrentUser API preserving Machine defaults.
Add WorkbenchCommand/DTO context to existing application/domain crates; register distinct Workbench dispatcher at existing Tauri Builder. Service ApplicationCommand adds only workbench.binding.authorize; never forwards SQL or service secrets.
Service schema stays v4. Read canonical endpoint/database/auth-reference/TLS CA identity plus ownership/activity in one transaction; derive descriptorRevision canonical BLAKE3 and service boot authorityEpoch. Permission admission linearizes at that read, not at remote SQL commit. Earlier-admitted SQL cannot be retroactively revoked.
Fresh authority before every write/COMMIT/protection action; read lease <=30s only with certified server readonly enforcement. Expiry/service loss blocks new dispatch but allows status/cancel/rollback/disconnect. Observe active target backup/restore conflicts; reject new conflicting mutating admission, without claiming a distributed target mutex.
User state schema v1 uses single process mutex and one writer actor; credentials encrypted before persistence. Another app instance activates primary or refuses execution. Window/session ownership invalidates on close/reconnect/descriptor change. Driver permits separate from backup permits.
Use concrete PG/MySQL-MariaDB/SQLite adapters, lossless capabilities, TLS verify-full and native readonly policies. Initial maxima8sessions/global2/target4running/global1/session, request queue32/global, proposed. Driver connect/lock/query timeouts and effective server/file identity must be visible. No global mutex over driver I/O.

## Test Scenarios

Normal: permitted feature action yields typed scoped result. Error: wrong owner/revision/capability fails before driver dispatch. Edge: duplicate/gap/restart/cancel/disk failures preserve evidence and bounds.
1. session.separate_credentials, T1 -> 허용된 Target의 redacted metadata만 바인딩하고 service secret을 사용자 연결에 자동 제공하지 않는다
2. session.handle_scope, T1 -> 다른 owner/window/workspace 또는 오래된 generation/revision handle을 데이터 전송 전에 거부한다
3. driver.tls_capability, T3 -> 인증서 검증을 충족 못하면 거부하고 DB별 미지원 capability를 활성화하지 않는다
4. session.lifecycle, T8 -> 재접속/창닫기/권한만료 때 세션을 무효화하며 진행 transaction을 새 connection으로 복원하지 않는다
5. session.worker_isolation, T10 -> DB·service 접속을 기다리는 동안 UI thread와 별도 backup worker permit을 점유하지 않는다
Tests listed here are to implement and run; planning documents do not count as their evidence.

## Implementation Strategy

Implement typed input/state validation before wire adapters, then feature UI and one vertical live scenario. Add deterministic failure seams at actual risky boundaries. Preserve existing feature behavior. Record AC proof only from executed tests/observations; source presence is weak.

## Quality Gate

Run targeted Rust tests and fmt/Clippy for changed crates, frontend TypeScript/Vitest and relevant Playwright/native WebView checks. New driver changes require live certification on all declared engines. Run cargo-audit/cargo-deny for new/affected dependencies. Broaden only on actual regression risk. Browser fixtures do not prove native IPC, live DB, performance or OS accessibility.

## Risk & Rollback

Disable this capability/route at its typed feature gate without changing old backup formats or deleting unresolved operations. Preserve journal/partial evidence. Never relax an AC because a test is difficult; before implementation, update the design explicitly if a contradiction exists.

## Acceptance Criteria

- [ ] AC-02-1 허용된 Target의 redacted metadata만 바인딩하고 service secret을 사용자 연결에 자동 제공하지 않는다 — 근거: P1,P3,P6 — 검증: session.separate_credentials, T1
- [ ] AC-02-2 다른 owner/window/workspace 또는 오래된 generation/revision handle을 데이터 전송 전에 거부한다 — 근거: P1,P3,P6 — 검증: session.handle_scope, T1
- [ ] AC-02-3 인증서 검증을 충족 못하면 거부하고 DB별 미지원 capability를 활성화하지 않는다 — 근거: P1,P3,P6 — 검증: driver.tls_capability, T3
- [ ] AC-02-4 재접속/창닫기/권한만료 때 세션을 무효화하며 진행 transaction을 새 connection으로 복원하지 않는다 — 근거: P1,P3,P6 — 검증: session.lifecycle, T8
- [ ] AC-02-5 DB·service 접속을 기다리는 동안 UI thread와 별도 backup worker permit을 점유하지 않는다 — 근거: P1,P3,P6 — 검증: session.worker_isolation, T10

## Files

- crates/restoreplane-workbench/src/lib.rs
- session.rs
- credentials.rs
- store.rs
- driver/{mod,postgres,mysql,sqlite}.rs
- apps/console/src-tauri/src/workbench.rs
- crate manifests
- restoreplane-domain/application qualified Workbench contracts and restoreplane-secrets CurrentUser provider (integration-owned)
Shared composition files are lead-integrated per Module Contract. Each feature worker owns only its declared paths.
