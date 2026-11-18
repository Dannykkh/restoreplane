# Section 07: Bounded Import and Export

## Background

Windows RestorePlane gains explicit CSV/TSV/JSON movement inside the user-owned Tauri Workbench. P2 covers memory growth, P5 covers duplicate admission/uncertain remote effects, and P7 covers partial files and mixed-time data. This does not replace certified native backup/restore or make an import job one distributed transaction.
All numerical budgets are proposed until measured/certified. No DBDelve source or dependency pins are copied.

## Requirements

- Native file-dialog handles identify source/destination; JavaScript never uploads or materializes a complete file/result. Native parsing, mapping, writes and serialization are streamed and bounded.
- Import preview shows target/schema, typed mapping, encoding/delimiter/NULL policy, row errors, generated INSERT shape and bound-value examples before dispatch. No user-authored SQL import, REPLACE or silent upsert.
- Each import batch is its own certified standalone transaction; earlier confirmed commits survive later cancellation/failure. Unknown child outcome stops every later batch and forbids automatic resend.
- Export shown data consumes only the selected retained Result Set/range and never reruns SQL. Evicted/unavailable rows reject rather than trigger a query.
- Export-all is a separate explicit approved safe read with target identity, source query/table, new read time and effective Consistency View. Volatile/side-effect/unproven user SQL is never automatically requeried.
- A transaction snapshot requires engine/version certification, explicit supported isolation and maximum 60s lifetime; expiry interrupts, never mixes fresh pages or silently downgrades. Live-per-page exports disclose their changing-time scope.
- A destination stays partial until complete source terminal, successful write/flush/sync and same-directory atomic finalization. Cancellation/disk-full cannot produce a file labeled complete.

## Dependencies

- Requires: section-02-sessions-and-credentials; section-03-operation-journal; section-04-schema-explorer; section-05-sql-workspace.
- These supply native user identity/handles/driver capabilities, durable admission/child outcomes, catalog/TypedCell/consistency and reviewed reads/cancel/stream contracts. Do not modify their owned files.
- Blocks: no further section; integration verification still covers all shared budgets and existing backup behavior.

## Module Contract

- **Provides:** `import.preview`, `import.start`, explicit `import.continue`, `import.cancel`, `export.start`, `export.cancel`, typed progress/parent-child projections and bounded partial-file artifacts.
- **Consumes:** section02 `SessionRef`, target authorization and native-dialog handle validation; section03 admission/version/CAS/recovery; section04 catalog identifiers/TypedCell/snapshot; section05 reviewed query and independent cancellation/retained result stream.
- **Owns:** `crates/restoreplane-workbench/src/data_movement.rs`; `apps/console/src/features/workbench/ImportReview.tsx`; `ExportDialog.tsx`; `crates/restoreplane-workbench/tests/data_movement.rs`.
- **Composition Point:** WorkbenchState data-movement handlers plus existing table/results menus. Integration lead registers the closed command group and components; this worker does not own App/Tauri/Cargo/registry/schema shared files.
- Files/durable records reuse validated native handles and journal operations; no new broker, service SQL endpoint or movement credential store.

## Flow Diagram Nodes

- `flow-diagrams/data-movement.mmd`: import `batch → write → known → more/imported`; unknown branches to `unknown` and stops dispatch.
- Export `read → partial → complete → rename → exported`; unapproved reread rejects; incomplete/cancel/write/flush/rename failures branch to `unfinished`.

## Reference Libraries

| Library / interface | Version basis | Use |
|---|---|---|
| Rust std/Tokio and serde_json | Existing selected Cargo.lock; Tokio currently =1.53.3 after security maintenance | Bounded native I/O, lexical/typed JSON handling and asynchronous worker coordination. |
| Existing interactive adapters / prepared statements | Certified versions from sections02/05 | INSERT value binding, batch transaction, safe reads/cancel; MariaDB separately certified. |
| Tauri/React | Current Console locks | Native dialog handles and bounded preview/progress; no full-file base64/JSON bridge. |

Check existing parser utilities first. A new CSV/TSV streaming parser needs exact-version official documentation, license and advisory review before installation; no ad hoc quote/newline splitting. JSON streaming must preserve numeric lexemes and enforce nesting/string/token limits.

## Implementation

### IPC and resource contract

Use `workbench_request({protocolVersion:1,requestId,command,payload,attemptToken?},onResult?)`; derive owner/window in Rust. Unknown fields/duplicate keys/invalid counts reject. `SessionRef={sessionId,connectionGeneration}`; `ObjectRef={catalogId,schemaVersion}`. IDs are validated strings; U64 rows/bytes/versions are decimal strings.
Errors: `{code,messageKey,retryable,operationId,field,details}` inside `{ok:false,requestId,error}`; use `WB_PAYLOAD_INVALID`, `WB_LIMIT`, `WB_FORBIDDEN`, `WB_BINDING_STALE`, `WB_TARGET_BUSY`, `WB_RESULT_EXPIRED`, `WB_IDEMPOTENCY_CONFLICT`, `WB_JOURNAL_UNAVAILABLE`, `WB_OUTCOME_UNKNOWN`, `WB_IO`. Unknown is retryable:false; retryability never permits target write replay. No row/SQL/secret body in default diagnostics.
`import.preview({session,object,sourceHandle,format,mapping,page}) → {previewId,columns,rows,warnings}`; columns is the exact destination cell order. `import.start({session,previewId,confirmedWarnings}) → Admission`; `import.continue.review({session,parentOperationId,previewId})` issues a fresh native continuation review; `import.continue({session,parentOperationId,reviewId}) → Admission`; `import.cancel({parentOperationId}) → {cancelRequested}`. Mutation admission requires native attemptToken.
`export.start({session,source:{kind:"shown",identity,range:{startRow,rowCount}}|{kind:"fresh",reviewId,snapshotId},destinationHandle,format,formulaPolicy:"escape"|"raw_confirmed",partialPolicy?:"retain"|"remove_on_cancel"}) → Admission`; `export.cancel({operationId}) → {cancelRequested}`. Shown range is explicit retained data(max200), not every row native may still retain. Export is a local file operation with no target mutation guarantee. Query policy still applies to a fresh read.
Native movement.source.pick/destination.pick issue owner/window/session file capabilities and bounded source-header columns with stable ordinal sourceKey; cancellation returns null. movement.file.release cleans an unsent selection, never cancels a job or deletes files. movement.status returns bounded child/progress/file projections and movement.partial.remove removes only an explicitly confirmed matching native-owned partial. Full DTOs and closed state enums are canonical in ipc-spec.md. No caller JS path or whole-file body is accepted.
Import/export parents may persist bounded64KiB movement_metadata_json for IDs/mapping/format/digests/counters and native-selected artifact location/identity/completeness, never rows/SQL/credentials. Five client tables/service v4 stay unchanged. Metadata is durable before child/file dispatch; final artifact evidence and operation version/outcome update atomically. Restart never resumes or finalizes an orphan automatically. Continued review proves a fresh source against prior metadata and all children; unknown always blocks, and sent failed/rolled-back ranges are never silently replayed or skipped. Such cases require a separately reviewed new import with an explicit remaining source.
Preview pages default100/max200 rows and 512KiB; closed source-column mapping and catalog IDs replace arbitrary identifiers. Lists use scoped opaque cursors and stable index order, not unbounded accumulation. Parser staging/read chunks and import bound parameters are each <=256KiB; batch <=200 rows and engine parameter-count limit. Reject oversized individual tokens/records before growth.
Reuse global native result-buffer64MiB, frontend retained32MiB, stream batch256KiB and outstanding-credit8MiB budgets. Acquire shared permits/credit before buffering; account parser, bind, serializer and file buffers independently. UI receives bounded sample/progress only. Large unsupported cells refuse or use separately certified explicit streaming; no silent truncation.

### Import state, source identity and atomic batches

1. Resolve source handle to an already-open native file with stable identity; bind source digest, length, encoding, mapping, target/schema revision and policy to preview/review. Compute whole-source digest in a bounded streaming preflight, not a whole-file allocation. Refuse changed file identity/content on continued reads.
2. Parse records incrementally across quoted newlines/UTF-8 boundaries; JSON arrays/records are streamed under depth/token/record limits. Convert using canonical TypedCell, rejecting overflow/lossy precision/unsupported temporal mapping; malformed rows stop with explicit location, never skip silently.
3. Review supported target transactional storage and no active manual transaction. Fresh service ownership/revision/conflict authorization is required immediately before each batch and COMMIT; this does not lock out other clients or a later restore.
4. Parent `kind=import_job` owns children `kind=import_batch`. Child metadata is immutable `(parentOperationId,batchIndex,sourceDigest,sourceByteStart,sourceByteEnd,sourceRowStart,sourceRowEnd,batchDigest)`; byte/row ranges are nonempty half-open ranges. Source range spans full parsed records, not arbitrary chunk boundaries.
5. Journal scope is owner/workspace/target/kind/attempt; unique scoped parent+batchIndex and canonical range/digest reject inconsistent children. Same token/digest returns the existing operation without another dispatch. Save metadata/digests only, not source rows, values or SQL.
6. Persist child dispatch intent before wire. Use scoped catalog-component identifier quoting plus bound INSERT values and an independent generated-INSERT structure guard. Each batch uses one certified standalone transaction, no arbitrary editor transaction, DDL or nontransactional fallback.
7. Child states distinguish accepted/dispatching/running/commit_requested/committed/rolled_back/failed/outcome_unknown with guarded versions and positive target evidence. Unknown blocks subsequent children immediately. Confirmed earlier batches remain visible; later error never relabels them rolled back.
8. Cancellation stops future admission and requests independent active-child cancel/rollback; cancellation intent is not rollback proof. Parent shows confirmed committed rows/batches, proven rollback, partial completion and unresolved children separately. Do not sum unknown rows as committed or failed.
9. Restart invalidates file/review/session handles; sent/commit-requested children without decisive evidence become unknown and are never automatically resumed. Unsent old-boot work retires. Explicit continuation requires a new review/source identity check and no unknown child; it cannot resend a previously sent range.

### Export identity, exactness and files

- Shown export binds exact retained result identity/range and completeness. The source is that explicit finite selection: all selected retained rows and exact totals must be confirmed before the selected-range file completes. The underlying query may be capped/interrupted/in-progress; preserve and disclose that status independently and never promote it to a complete query or whole-table export. Missing/expired selected rows leave a partial file. Consume retained values without query replay.
- Fresh export reviews a known-safe read/table generator and new target identity/time/isolation. Reuse existing readonly capability checks; unknown/volatile statements refuse requery. Snapshot expiry at <=60s yields partial output; no silent live fallback.
- A private FileReadLimits/execute_file_read path separates total file-source quota (max1,000,000rows/1GiB encoded/300s) from buffer retention. Keep UI query defaults100k/64MiB/30s, native QueryLimits hard1Mrows/64MiB and all native64MiB/inline64KiB/frame budgets unchanged; no arbitrary UI quota override. Snapshot remainingTTL/max60s is stricter. Prove actual>64MiB streaming with bounded native sink and retain honest capped/timeout partial state.
- Stream to a unique create-new partial file in the selected destination directory/volume. Never follow caller-provided paths/overwrite silently; native dialog/review owns destination and explicit replace policy.
- Finalization/identity-checked cleanup is enabled only on an actually detected certified filesystem (initial local NTFS). Other/unknown filesystems refuse that capability; Windows alone is not filesystem certification. Hold the approved parent/file object through the operation and never infer historical permanent identity from a path or recycled file ID.
- On complete source terminal and validated row/byte totals, flush/sync, close as required by Windows, then perform certified same-volume atomic finalization. Finalization failure remains partial; no copy/delete fallback labeled atomic. Destination races refuse or follow explicitly reviewed replacement semantics.
- Failure/cancel/disk-full preserves or explicitly removes this job's partial file according to reviewed policy and records its status. Recovery never auto-finalizes an orphan partial or overwrites a complete file. Retained partials contain user data and require visible location/retention choice.
- CSV/TSV policy distinguishes NULL token `\N` from empty; literal text starting with backslash gets an additional leading backslash, reversed after NULL detection on import. Review records encoding/delimiter/escape policy. Typed JSON uses tagged exact integer/decimal strings, bytes and temporal metadata for lossless representation.
- The application typed-JSON wrapper has canonical columns→rows→complete order for bounded streaming; reordered generic row-object keys remain supported. A differently ordered wrapper is an unsupported file-format shape, not permission to buffer the entire file. Whole-source bounded preflight validates framing/EOF and a positive typed footer before the first import child is sent; malformed/truncated/incomplete sources refuse before target mutation.
- Default spreadsheet formula policy `escape` neutralizes leading `=,+,-,@` and control-prefix cases in text fields and discloses changed presentation. `raw_confirmed` requires explicit warning acknowledgment. Spreadsheet safety transformations are never claimed as byte-identical typed roundtrip; typed JSON is the exact alternative. Never parse decimal/u64 through JS number.

## Test Scenarios

| Test / QA | Inputs and expected observations |
|---|---|
| movement.memory_bounds / T2 | Large synthetic CSV/JSON, multiline records, oversized token and stalled UI: bounded parser/native/IPC/DOM, no full-file allocation, explicit limit error. |
| movement.import_consistency / T9 | Two committed batches then rollback, disk fault or lost COMMIT: review precedes bind-only INSERT; earlier evidence retained; unknown blocks later dispatch on all certified engines. |
| movement.export_identity / T9 | Shown capped/evicted result; fresh live/snapshot; volatile SQL: shown performs zero requery; expired/foreign identity refuses; approved fresh time/consistency disclosed; snapshot expiry interrupts. |
| movement.partial_file / T9 | Cancel, disk-full, incomplete terminal, flush failure, Windows destination race and atomic-rename failure: no false complete file; normal complete+flush finalizes once. |
| movement.duplicate_types / T5 | Duplicate token/child, changed range/digest, crash around COMMIT, NULL/empty/backslash/formula/u64-max/decimal/bytes: no automatic sent-batch replay and declared representation/type policy holds. |

## Implementation Strategy

Red: fault-first tests for duplicate child, lost COMMIT, oversized parser token and failed finalize. Green: bounded preview→one certified batch, then progress/child recovery, shown export and explicit safe fresh export. Refactor: share existing typed codecs/permits/cancel/journal contracts; measure copies after changes. Tests must execute actual parsing/transaction/file boundaries, not mirror internal function names.

## Quality Gate

Run affected Rust tests/fmt/Clippy, frontend build/Vitest and import/export UI flows; run live PostgreSQL/MySQL/MariaDB/SQLite transactional-batch proofs and Windows file finalization/fault tests before enabling their capability. Check current dependency advisories/licenses. Record W2/M3 fault outputs; missing execution evidence stays NOT RUN/weak. New registry/API fields require lead integration into ipc-spec and shared contracts before source additions. No production DB mutations occur during planning.

## Risk & Rollback

| Risk | Mitigation | Rollback |
|---|---|---|
| Changed source or duplicate sent batch | Immutable source/range/digest and journal CAS; unknown halts future batches | Disable new import admission; preserve child evidence and partial progress. |
| Parser/LOB memory growth or formula/type corruption | Token/frame/retention bounds; exact codecs and explicit formula/NULL policy | Refuse unsupported input/export policy; retain user source unchanged. |
| Lost COMMIT or filesystem-finalize result | Separate target outcome from file completeness; never infer rollback/success | Preserve unknown/partial, explicit inspection only; no automatic resend/finalize. |

## Acceptance Criteria

- [ ] AC-07-1 대량 파일·결과를 이동할 때 JS/native에 전체 파일을 적재하지 않고 parser·batch·credit·retained memory 상한을 지킨다 — 근거: P2,P5,P7 — 검증: movement.memory_bounds, T2
- [ ] AC-07-2 검토한 import만 bound INSERT의 단일 child batch transaction으로 실행하며 confirmed commit/rollback·partial·unknown을 구분하고 unknown 뒤 후속 batch를 중단한다 — 근거: P2,P5,P7 — 검증: movement.import_consistency, T9
- [ ] AC-07-3 shown export는 기존 결과를 재실행하지 않고 export-all은 명시적 새 읽기의 실제 target identity·time·consistency를 공개하며 volatile/unproven SQL을 자동 재조회하지 않는다 — 근거: P2,P5,P7 — 검증: movement.export_identity, T9
- [ ] AC-07-4 파일은 complete terminal·flush/sync·atomic finalize 성공 뒤에만 complete로 표시하며 취소·disk-full·finalize 실패는 partial로 남는다 — 근거: P2,P5,P7 — 검증: movement.partial_file, T9
- [ ] AC-07-5 중복 요청·child·재시작에도 전송된 batch를 자동 재실행하지 않고 formula·NULL/empty·정밀숫자·binary 변환은 검토된 정책과 정확한 타입 계약을 따른다 — 근거: P2,P5,P7 — 검증: movement.duplicate_types, T5

## Files

- `crates/restoreplane-workbench/src/data_movement.rs` — bounded parser, child sequencing and import/export handlers.
- `apps/console/src/features/workbench/ImportReview.tsx` and `ExportDialog.tsx` — target/type/policy review and honest progress.
- `crates/restoreplane-workbench/tests/data_movement.rs` — memory, child-state, live adapter and file-fault scenarios.
- Shared command registration, schema, Cargo locks, desktop composition and implementation ledgers remain integration-lead-owned; return patch requests rather than editing them concurrently.
