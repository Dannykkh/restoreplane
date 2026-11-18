# Section 04: Schema explorer and typed table browsing

## Background

Additive upgrade to Windows RestorePlane at08ba926. P2,P4,P7 are the owning problems. Existing Console inventories/native backup adapters are not general SQL or a virtual query grid.

## Requirements

- Tauri 유지; 현재 Workspace/Target 소유권과 기존 service_request 보호 계약을 보존한다. 임의 SQL은 현재 사용자 Workbench에서만 실행한다.
- Canonical handle binds native owner/window/workspace/target/profile revision/generation. JS identity claims are not authority.
- Generated values bind; scoped identifiers quote per dialect; exact TypedCell is independent from display text.
- No target exactly-once claim, no automatic write replay, no rollback inference from cancel/disconnect.
- Local operation state/version is authoritative; duplicated hints resync by pull. No new durable UI broker.
- All new limits are proposed/unmeasured until actual tests; never report parser/type/static-file presence as runtime proof.
- Section behavior: lazy metadata/search, no per-page COUNT; schema fingerprint; exact unique order cursor; live/snapshot capability; exactNULL/numeric/binary/time codec; row+column virtualization + boundedpageLRU + keyboardfallback; FK and bounded ER diagram.

## Dependencies

section-01, section-02, section-03
Consumed artifacts/contracts must be present before the implementation wave starts; dependencies are runtime responsibilities, not permission to modify their owned files.

## Module Contract

- Provides: versioned metadata, scoped keyset preview, canonical TypedCell, bounded row/column grid
- Consumes: 02 target sessions; 03 operation/events; 01 measured budgets
- Owns: crates/restoreplane-workbench/src/metadata.rs; browse.rs; types.rs; result_stream.rs; apps/console/src/features/workbench/{SchemaExplorer,VirtualResultGrid,RowInspector,SchemaDiagram}.tsx; screens/Workbench.tsx
- Composition Point: App.tsx route workbench + WorkbenchState metadata/browse dispatch
- Shared composition edits: implementation lead owns workspace Cargo.toml/Cargo.lock, Tauri lib.rs/capability registrations, App.tsx/route/types, existing service command routing. Feature workers return integration patch requests; do not edit them concurrently.

## Reference Libraries

Existing Rust std/Tokio, application/domain IDs and error vocabulary first; existing tokio-postgres/rusqlite/secrets interfaces reused only with scope and current advisory checks. New mysql_async and TanStack Virtual are candidates with verified upstream docs, not installed by planning.
- https://github.com/blackbeam/mysql_async (streaming/prepared/TLS)
- https://github.com/tanstack/virtual (row+column virtualization, stable keys)
- https://v2.tauri.app/develop/calling-frontend/ (Channels are not app credit/ACK)
No DBDelve GPL code or dependency pins are copied. Exact new versions/licenses/advisories are implementation gates.

## Implementation

Section04 owns the shared result_stream.rs transport/credit/ACK implementation used by browse and consumed by section05 SQL. Implement result.attach/ack here; no forward dependency on section05 or duplicate transport.

Internal IPC: workbench_request(request,onResult?) with the closed Request envelope {protocolVersion:1,requestId,command,payload,attemptToken?} dispatched in Rust. No HTTP API/MCP. Errors use WBError {code,messageKey,retryable,operationId,field,details} in {ok:false,requestId,error}; no raw SQL/parameters/driver secrets in default diagnostics.
Lists use opaque cursor, limit100/default200/max and512KiB/page (metadata legacy service pages max100); allowed sort/filter are explicit enums/catalog IDs. No N+1 catalog fetch per displayed row. Invalid/foreign/expired cursor returns a typed error.
Data batches max256KiB, credit8MiB, UI retained32MiB and native buffers64MiB proposed; sequence and generation bind all ACKs. Rows/bytes/cells and queued requests are independently bounded.
lazy metadata/search, no per-page COUNT; schema fingerprint; exact unique order cursor; live/snapshot capability; exactNULL/numeric/binary/time codec; row+column virtualization + boundedpageLRU + keyboardfallback; FK and bounded ER diagram.

Versioned lazy metadata using native driver catalog APIs; do not reuse existing full metadata discovery's 15s/1MiB assumptions. Define allowed prefix/name/schema filters and stable catalog ordinal snapshot; cache invalidates on descriptor/schema change.
Generated table browse uses catalog identifiers plus bound values, stable complete nonnullable unique tie-breaker and cursor identity. Without proven ordering offer bounded readonly preview. Display live-per-page/explicit-certified snapshot separately from result complete/capped/partial and count unknown/estimated/exact.
Canonical TypedCell stores precise integer/decimal text, NULL, bytes, float specials, timestamp kind/timezone/precision and typeId. Unsupported or truncated values cannot be edit keys. Large cell explicit details/export has separate quota.
Row+column virtualizers share scroll viewport; stable getItemKey anchors to result/row/column. Page cache LRU32MiB proposed, selection/drafts separately bounded. Eviction mounts destination before focus, keyless preview identity invalidates on refresh. ARIA logical coordinates and row-detail fallback; Korean IME Enter never triggers execute/apply.
FK explorer and bounded ER diagrams use same metadata version; cap nodes/edges, cancellation and layout cache, SVG/Mermaid export. Register one Workbench screen in current App shell via lead integration.

## Test Scenarios

Normal: permitted feature action yields typed scoped result. Error: wrong owner/revision/capability fails before driver dispatch. Edge: duplicate/gap/restart/cancel/disk failures preserve evidence and bounds.
1. browse.metadata_bounds, T2 -> 10k metadata 목록을 lazy pages로 탐색하며 화면 DOM과 retained pages를 상한 안에 유지한다
2. browse.cursor_identity, T1 -> cursor의 target/filter/order/schema mismatch를 거부하고 keyless preview의 한계를 표시한다
3. types.roundtrip, T3 -> NULL/empty/u64/decimal/binary/temporal 값을 lossless로 구분하고 truncated/unsupported 값은 편집하지 않는다
4. browse.consistency, T2 -> 페이지 일관성·count 정확도·cap 상태를 표시하며 live pages를 snapshot으로 주장하지 않는다
5. grid.keyboard_diagram, T2 -> 가상화·page eviction 후 키보드 focus/selection/ARIA 좌표를 보존하고 FK/diagram을 같은 metadata version으로 표시한다
Tests listed here are to implement and run; planning documents do not count as their evidence.

## Implementation Strategy

Implement typed input/state validation before wire adapters, then feature UI and one vertical live scenario. Add deterministic failure seams at actual risky boundaries. Preserve existing feature behavior. Record AC proof only from executed tests/observations; source presence is weak.

## Quality Gate

Run targeted Rust tests and fmt/Clippy for changed crates, frontend TypeScript/Vitest and relevant Playwright/native WebView checks. New driver changes require live certification on all declared engines. Run cargo-audit/cargo-deny for new/affected dependencies. Broaden only on actual regression risk. Browser fixtures do not prove native IPC, live DB, performance or OS accessibility.

## Risk & Rollback

Disable this capability/route at its typed feature gate without changing old backup formats or deleting unresolved operations. Preserve journal/partial evidence. Never relax an AC because a test is difficult; before implementation, update the design explicitly if a contradiction exists.

## Acceptance Criteria

- [ ] AC-04-1 10k metadata 목록을 lazy pages로 탐색하며 화면 DOM과 retained pages를 상한 안에 유지한다 — 근거: P2,P4,P7 — 검증: browse.metadata_bounds, T2
- [ ] AC-04-2 cursor의 target/filter/order/schema mismatch를 거부하고 keyless preview의 한계를 표시한다 — 근거: P2,P4,P7 — 검증: browse.cursor_identity, T1
- [ ] AC-04-3 NULL/empty/u64/decimal/binary/temporal 값을 lossless로 구분하고 truncated/unsupported 값은 편집하지 않는다 — 근거: P2,P4,P7 — 검증: types.roundtrip, T3
- [ ] AC-04-4 페이지 일관성·count 정확도·cap 상태를 표시하며 live pages를 snapshot으로 주장하지 않는다 — 근거: P2,P4,P7 — 검증: browse.consistency, T2
- [ ] AC-04-5 가상화·page eviction 후 키보드 focus/selection/ARIA 좌표를 보존하고 FK/diagram을 같은 metadata version으로 표시한다 — 근거: P2,P4,P7 — 검증: grid.keyboard_diagram, T2

## Files

- crates/restoreplane-workbench/src/metadata.rs
- browse.rs
- types.rs
- result_stream.rs
- apps/console/src/features/workbench/{SchemaExplorer,VirtualResultGrid,RowInspector,SchemaDiagram}.tsx
- screens/Workbench.tsx
Shared composition files are lead-integrated per Module Contract. Each feature worker owns only its declared paths.
