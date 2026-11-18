# Section 08: Protection and restored-target inspection

## Background

Additive upgrade to Windows RestorePlane at08ba926. P1,P6,P8 are the owning problems. Existing Console inventories/native backup adapters are not general SQL or a virtual query grid.

## Requirements

- Tauri 유지; 현재 Workspace/Target 소유권과 기존 service_request 보호 계약을 보존한다. 임의 SQL은 현재 사용자 Workbench에서만 실행한다.
- Canonical handle binds native owner/window/workspace/target/profile revision/generation. JS identity claims are not authority.
- Generated values bind; scoped identifiers quote per dialect; exact TypedCell is independent from display text.
- No target exactly-once claim, no automatic write replay, no rollback inference from cancel/disconnect.
- Local operation state/version is authoritative; duplicated hints resync by pull. No new durable UI broker.
- All new limits are proposed/unmeasured until actual tests; never report parser/type/static-file presence as runtime proof.
- Section behavior: exacttablehandoff revalidate; backupbeforechange optional noautoSQL; existingcommandsidem/confirmationreuse; actualrestoredidentityproof notdisplayname; nooriginalfallback; separatepermits test; nativeSignedArtifact unchanged.

## Dependencies

section-02, section-03, section-04
Consumed artifacts/contracts must be present before the implementation wave starts; dependencies are runtime responsibilities, not permission to modify their owned files.

## Module Contract

- Provides: exact Scope handoff and readonly restored-target open using existing backup evidence
- Consumes: existing service Scope/backup/restore preflight APIs;02bindings;03journal;04selection
- Owns: crates/restoreplane-workbench/src/protection.rs; apps/console/src/features/workbench/ProtectionPanel.tsx; apps/console/src/lib/workbenchProtection.ts; tests/protection_links.rs
- Composition Point: Workbench protection inspector→existing service_request; existing Recovery completion→new readonly Workbench session
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
exacttablehandoff revalidate; backupbeforechange optional noautoSQL; existingcommandsidem/confirmationreuse; actualrestoredidentityproof notdisplayname; nooriginalfallback; separatepermits test; nativeSignedArtifact unchanged.

Service remains owner of backup/restore/signed evidence and all native tools. Workbench selection passes exact workspace/target/table IDs and observed schema version to existing Scope workflow, revalidated by service; user display name is not identity.

Implementation contract refinement: existing discovery/save commands gain optional Workbench selection verification, as defined in ipc-spec.md. Up to100 selected objects/64KiB carry exact native-discovered IDs, names and shared fixed-query structural signatures plus expected descriptor revision; existing discovery operation request_json/result_json stores the bounded request/proof without a schema migration. Scope save rechecks that successful/current proof and exact selection. Existing name/ID fingerprints are not structural schema hashes and their semantics stay unchanged for current backup plans. User SQL never crosses this bridge. SQLite explicit-table backup remains explicitly unsupported; no automatic expansion to whole database is allowed. Fresh structure observations do not claim an atomic remote DDL lock.

Selected discovery coverage is distinct from full inventory and cannot mark other legacy tables missing. Each new Workbench selected Scope uses its exact discoveryRunId as scopeId, with the exact scope fingerprint/selector hash/selection digest link committed in that same save transaction. One discovery creates one Scope; another Scope requires fresh discovery. Native health uses a primary-key lookup and exact link verification, never an inferred time match or unbounded history scan. Missing proof or older full inventory yields unknown; a newer applicable full observation can replace the selected observation. Existing Scope IDs, signed formats and service schema v4 remain unchanged.

Native `protection.selection.get` resolves up to100 scoped ObjectRefs to that canonical selection metadata without dispatching backup/SQL. For restored-open, existing session.open gains optional restoreWorkItemId (read_only required); native fetches the existing work.get optional typed post-restore proof and checks actual server+database OID or final SQLite file identity before publication. Completed work plus a database name alone is insufficient against drop/recreate. Persist proof in existing completion JSON, preserve ordinary API behavior and reject engines/targets whose actual instance identity cannot be proved. Never accept a JS-supplied purpose/proof or fall back to the original target.
Optional backup-before-change shows separate run dependency/age/evidence; no automatic SQL dispatch from backup events and no transactional undo claim.
Fresh target maintenance status gates observed conflicting writes/import/DDL; the admission read is not distributed mutual exclusion. Existing service restore preflight/confirmation remains authoritative. Request current-app session disposition; unconfirmed old transaction effects remain unknown.
Open restored target only on completed restore proof + actual server/database/file identity revalidation. New readonly session/user credential, never service secret/original-target fallback. Schema diff/row observation doesn't assert semantic equality or L1/L3 verification.
File-identity certification is filesystem-specific. New restored-open uses only an actually detected, certified local NTFS path with a held no-delete guard through publication; unsupported/unknown filesystems refuse proof-based opening. The existing64bit volume/file-index helper is not certified for ReFS. Historical deletion/recreation may reuse IDs, so this is not an eternal ABA-proof identity. Existing SourceFingerprint semantics remain unchanged. See [Microsoft file ID guarantees](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/ns-fileapi-by_handle_file_information).
Regression runs existing service scheduling/backup/restore/retention/cancel contracts plus slow active queries/import streams. Separate app permits bound resource consumption; remote DB QoS and third-party clients remain outside control.

## Test Scenarios

Normal: permitted feature action yields typed scoped result. Error: wrong owner/revision/capability fails before driver dispatch. Edge: duplicate/gap/restart/cancel/disk failures preserve evidence and bounds.
1. protection.scope_handoff, T1 -> 선택 table IDs를 기존 Scope API에 전달하고 service가 Workspace/target/schema identity를 다시 검사한다
2. protection.no_global_tx, T10 -> backup-before-change를 별도 dependency로 표시하고 backup 완료 이벤트만으로 SQL을 실행하지 않는다
3. protection.restore_identity, T10 -> 기존 restore preflight/confirmation/evidence를 유지하며 일치하는 restored target만 새 readonly session으로 연다
4. protection.no_fallback, T10 -> 복원 대상 불일치·권한거부 때 원본 fallback이나 더 높은 service credentials 사용을 하지 않는다
5. protection.backup_regression, T10 -> 긴 query/import/느린 consumer와 백업 동시 실행에서 기존 service lifecycle·scheduling·cancel 회귀가 없다
Tests listed here are to implement and run; planning documents do not count as their evidence.

## Implementation Strategy

Implement typed input/state validation before wire adapters, then feature UI and one vertical live scenario. Add deterministic failure seams at actual risky boundaries. Preserve existing feature behavior. Record AC proof only from executed tests/observations; source presence is weak.

## Quality Gate

Run targeted Rust tests and fmt/Clippy for changed crates, frontend TypeScript/Vitest and relevant Playwright/native WebView checks. New driver changes require live certification on all declared engines. Run cargo-audit/cargo-deny for new/affected dependencies. Broaden only on actual regression risk. Browser fixtures do not prove native IPC, live DB, performance or OS accessibility.

## Risk & Rollback

Disable this capability/route at its typed feature gate without changing old backup formats or deleting unresolved operations. Preserve journal/partial evidence. Never relax an AC because a test is difficult; before implementation, update the design explicitly if a contradiction exists.

## Acceptance Criteria

- [ ] AC-08-1 선택 table IDs를 기존 Scope API에 전달하고 service가 Workspace/target/schema identity를 다시 검사한다 — 근거: P1,P6,P8 — 검증: protection.scope_handoff, T1
- [ ] AC-08-2 backup-before-change를 별도 dependency로 표시하고 backup 완료 이벤트만으로 SQL을 실행하지 않는다 — 근거: P1,P6,P8 — 검증: protection.no_global_tx, T10
- [ ] AC-08-3 기존 restore preflight/confirmation/evidence를 유지하며 일치하는 restored target만 새 readonly session으로 연다 — 근거: P1,P6,P8 — 검증: protection.restore_identity, T10
- [ ] AC-08-4 복원 대상 불일치·권한거부 때 원본 fallback이나 더 높은 service credentials 사용을 하지 않는다 — 근거: P1,P6,P8 — 검증: protection.no_fallback, T10
- [ ] AC-08-5 긴 query/import/느린 consumer와 백업 동시 실행에서 기존 service lifecycle·scheduling·cancel 회귀가 없다 — 근거: P1,P6,P8 — 검증: protection.backup_regression, T10

## Files

- crates/restoreplane-workbench/src/protection.rs
- apps/console/src/features/workbench/ProtectionPanel.tsx
- apps/console/src/lib/workbenchProtection.ts
- tests/protection_links.rs
Shared composition files are lead-integrated per Module Contract. Each feature worker owns only its declared paths.
