# Section 06: Reviewed and verified data editing

## Background

RestorePlane is a Windows/Tauri/React application with a privileged backup service. This section adds user-owned interactive insert/update/delete to a proven base table, solving P3 unsafe execution, P4 row/type/concurrency ambiguity and P5 duplicate/unknown commits. Existing native dump adapters and operational `console_query` are not editable database sessions.
Keep arbitrary SQL and interactive credentials in the standard-integrity user Workbench; existing `service_request` provides canonical metadata/protection authority only. Service schema remains v4: descriptorRevision is a canonical hash from the existing read transaction, authorityEpoch is service boot identity. No SQL or service credential export is added.
CurrentUser DPAPI is the explicitly scoped existing secrets provider extension; do not create another secret framework or change machine defaults. Local client-state schema v1 stores metadata/digests/state/version only, not draft/original/SQL/parameter bodies. UI notifications are hints with operation pull resync; no durable UI outbox/inbox broker.

## Requirements

Implement bounded typed insert/update/delete review and standalone verified apply. Require certified identity/codecs/transaction semantics, explicit actual-target review, fresh dispatch/COMMIT authorization and durable unknown handling. The five ACs below define completion; rendered controls alone do not enforce permission or consistency.

## Dependencies

Requires section-02-sessions-and-credentials, section-03-operation-journal, section-04-schema-explorer and section-05-sql-workspace completed. Their owned source files are consumed through contracts, not modified by this worker.
Consumes: 02 native owner/window/scoped session handle, fresh service binding authorization and certified drivers; 03 atomic admission/attempt/digest/version/unknown journal; 04 versioned catalog, source/key metadata, canonical TypedCell and virtual grid actions; 05 session execution barrier, modes/transactions/cancel and scoped operation status UI.
Provides: reviewable edit drafts, independently gated generated statements, standalone single-target verified apply, conflict preservation and unknown quarantine. Section07 may consume a declared generated INSERT contract; it must retain its separate batch journal rather than bypass this gate.

## Module Contract and owned files

- Own only `crates/restoreplane-workbench/src/edits.rs`, `generated_sql.rs`, `tests/verified_edits.rs` and `apps/console/src/features/workbench/{EditDraft,EditReview,ConflictPanel}.tsx`.
- Runtime composition: WorkbenchState registers edit prepare/apply handlers; Workbench grid emits typed draft actions and mounts the review/conflict panels. The implementation lead owns shared dispatcher/DTO/domain/secrets additions, crate manifests/lockfile, Tauri Builder/capability JSON, App/routes/types and grid/Workbench screen integration patches.
- Keep the live draft state in the feature coordinator, scoped to result/table/key/column identity and generation. Do not attach edits to viewport indices or retain whole query results to keep one draft alive.
- Flow responsibilities: verified-apply nodes eligible/auth/admit/tx/compare/write/commit and commit-loss sequence around target/local commit; refusal -> reason, conflict -> confirmed rollback+preserved draft, uncertain outcome -> quarantine. No hidden retry or automatic restore/backup/Undo.

## Reference Libraries

Use the existing domain/application/secrets crates and section02 driver adapters, section03 journal and section04 typed metadata/stream/grid contracts. Prefer driver parameter binding and per-component dialect quoting; select no additional query framework merely for editing. Official engine/driver sources and their certification limits are linked in [research](../research/web.md) and [IPC](../ipc-spec.md).

## Implementation

### Typed command and UI contract

Use the existing `workbench_request` envelope `{protocolVersion:1,requestId,command,payload,attemptToken?}` dispatcher; no HTTP/MCP route. SessionRef is `{sessionId,connectionGeneration}` and ObjectRef is `{catalogId,schemaVersion}`; IDs are opaque and unbounded counters/versions are decimal-string U64. Shared DTO/API edits are lead integration; this self-contained contract is the minimum behavior to preserve.
- `edit.prepare`: `{session:SessionRef,object:ObjectRef,changes:EditChange[]}` -> `EditReview`. `EditChange` is a discriminated insert/update/delete union: insert supplies column/value pairs without an existing rowHandle; update supplies rowHandle and changed pairs; delete supplies rowHandle. `EditableCell` excludes large/unsupported cells. Update/delete use Rust-issued rowHandle provenance/originals, never client-supplied original strings. Insert omitted columns invoke only explicitly reviewed server defaults; generated columns are omitted/read-only. Update DEFAULT is unsupported unless independently certified; never encode it as arbitrary Cell text. Native resolves metadata/mode, and prepare never sends mutations.
- `EditReview` includes draftId/reviewId, actual `TargetContext`, schemaVersion, parameterized statementPreviews/typed parameter lists, warnings and bounded typed original/draft summaries. Ordinary reply stays within1MiB and draft512KiB; oversized input refuses with WB_LIMIT or offers a separately explicit smaller review, never a hidden batch split. UI approval applies to exactly this immutable review.
- `edit.apply`: `{session:SessionRef,draftId,reviewId,confirmedWarnings:string[]}` -> `{operationId,version,deduplicated}`. First obtain the native attemptToken through `operation.prepare` for this exact payload and pass it in the envelope. Rust resolves the immutable reviewed digest and exact session/target, reauthorizes immediately before dispatch, and uses journal admission before sending anything. Caller cannot replace SQL/parameters in apply. Completion is authoritative `operation.get/list`, not a success toast after sending. `edit.discard {draftId}` disposes an unsent memory draft only, never a transaction or unknown operation.
- Scope all handles to native owner/window/Workspace/target/binding revision/session/connection generation. A changed mode, schema, target, result generation or review basis invalidates approval; return an explicit stale/conflict reason and require a new review.
- Errors use bounded `WBError {code,messageKey,retryable,operationId,field,details}` without default raw driver SQL/parameter text. Use `WB_CAPABILITY_UNSUPPORTED` with read-only/multi-match/manual-transaction reason, `WB_BINDING_STALE`/`WB_SESSION_STALE`, `WB_ROW_CONFLICT`, `WB_TARGET_BUSY`, `WB_OUTCOME_UNKNOWN`, `WB_IDEMPOTENCY_CONFLICT` and `WB_LIMIT`; unknown has retryable:false. Details are allowlisted reason/operation/version metadata, never raw current rows or parameters.
- `EditReview` repeats Workspace/actual endpoint or file/database/DB role/source-restored purpose, operation count, original/new typed values, generated SQL and parameter list, transaction limits and warnings. SQL text is inspectable; parameter values remain separate. Korean IME Enter never applies. Apply requires an explicit reviewed action.
- Proposed draft/apply bounds: at most100 rows and512KiB encoded typed changes, within the adapter's certified parameter limit and global draft memory budget. Reject excess with a reason; do not silently split an atomic batch. Changing bounds requires measurements, not a completion claim.

### Bounded conflict observation

`edit.conflict.get {session,conflictId}` returns the typed ConflictObservation defined in ipc-spec.md. The apply error includes only an opaque conflictId; raw rows stay out of WBError details, journal and diagnostics. Native stores only the bounded values actually observed during that apply, in memory under the existing draft/result budget and handle TTL. Original/draft remain the immutable review. Observed-current has distinct present, missing and unavailable arms plus observedAtMs/currentSchemaVersion; it is not a live requery. Owner/window/session/generation/schema/review checks precede retrieval. Expiry or unavailable observation preserves the draft and requires a separately explicit fresh browse/new review rather than guessing or resending. Current-value body stays within512KiB/full reply1MiB; oversize refuses explicitly.

## Capability and lossless-type gate

Read-only default and DB role/file ACL remain actual authority. Rust checks both mode and certified write capability; disabling an input control is insufficient.
Only one proven ordinary base table, complete ordinary nonnullable PK/unique identity, current source/schema metadata, lossless identity/original/change codecs and certified transactional storage qualify. Join/expression/view/keyless/ambiguous-origin results and partial/expression/nullable keys refuse editing with the specific reason. Initial insert also requires a supported base table/key contract; generated key fields are omitted only when the adapter certifies retrieval. Never turn a join into a guessed target.
Reuse section04 TypedCell tags/codecs: NULL is not empty/default/missing, integer/unsigned/decimal payloads are exact canonical text, bytes carry exact bytes, floats retain specials and temporal cells retain kind/precision/timezone. Parameter encoding must roundtrip in the actual driver/server type. Lossy display, truncated/oversize cells, unsupported/generated columns and unprovable comparison basis stay read-only; never recover originals from formatted strings.
Choose a proven row-version token when available; otherwise lock/read and compare the declared complete original basis using exact typed semantics. Database collation equality may differ from byte/text equality (case, accents, CHAR padding, decimal scale); certification defines canonical equivalence and refuses unsupported keys/bases. Original-value comparison detects current mismatch, not ABA or intervening history.

| Engine | Enabling criteria and transaction behavior | Required read-only fallback |
|---|---|---|
| PostgreSQL | Proven base/complete nonnullable ordinary unique key; bound parameters with exact type OIDs/codecs; standalone transaction and locked current-row read, certified conditional match/RETURNING counts; schema recheck under supported lock/isolation policy | Unresolved origin, unsupported codecs/key/index or lock/isolation proof missing; do not assume xmin is a universal durable version token |
| MySQL | InnoDB for affected tables and certified server/driver settings; begin standalone transaction, `SELECT ... FOR UPDATE`, exact typed read/compare, bound prepared mutation; prove matched-versus-changed behavior and single-row identity under effective collation | Nontransactional engine/unsupported implicit-commit path, unknown connector row-count flags, lossy/padded key semantics or unproven trigger effects |
| MariaDB | Independent certification row for server/driver/InnoDB/counts/codecs/locking, even when using the same Rust driver implementation | MySQL certification alone never enables MariaDB editing; same refusal rules until independently proven |
| SQLite | Proven base/ordinary nonnullable key, exact SQLite storage-class codec and supported generated-key retrieval; standalone `BEGIN IMMEDIATE` with bounded BUSY handling and conditional match proof; restrictive connection policy | Unproven virtual/view/rowid origin, nullable legacy PK, unsupported affinity/extension/type behavior, or noncertified affected-count semantics |

Triggers/defaults/routines may execute extra or external/nontransactional effects. The promise is supported transactional effects only, not universal rollback of the world; disclose limitations and refuse verified-apply capability when they invalidate the promised match/rollback semantics. DDL is excluded from this generated path. Recheck storage/trigger/schema capability on apply, not only when the tab first opened.

## Verified apply algorithm and generated SQL gate

1. Freeze a bounded in-memory draft and review digest. Check no active manual transaction owns the target session; generated edits never piggyback on arbitrary manual transactions or silently commit/rollback them. Require explicit manual transaction disposition first.
2. Revalidate native session/owner/window/generation, fresh service descriptor authorization, effective DB/file identity and observed backup/restore/maintenance conflict. Refuse changed identity/conflicting admission. This observation is not a distributed lock with other DB clients.
3. Journal scoped `(owner,Workspace,target,kind,key)` admission plus canonical typed payload digest, epochs and review version. Same key/digest returns the existing operation; changed digest conflicts. Persist guarded dispatch intent before target wire, never re-admit a sent mutation.
4. Under the session's execution/cancel barrier, begin a fresh transaction. Apply proposed lock-wait2s/statement30s bounds through certified driver/session settings; failure to establish limits/capabilities refuses. Re-read schema/source identity and lock the relevant existing rows in deterministic key order. For insert, use ordinary INSERT and let unique conflicts refuse; no REPLACE/upsert/merge retry.
5. Compare exact originals/version to the current locked row. Missing/changed/multiple matched rows or schema mismatch -> request rollback of the entire supported batch. Mark rolled_back only on conclusive evidence; an unconfirmed rollback is unknown. Preserve draft/original/current values in memory and show conflict fields; refresh-basis requires explicit new review.
6. Build SQL from catalog-qualified identifier components with per-dialect quoting; values are driver-bound parameters. Structural operations/operators/default omission are closed enums, never raw client clauses. A separate gate reads the generated statement's structure and placeholder bindings: one allowed INSERT/UPDATE/DELETE, exact target/columns/key predicates, no additional statements/comments/functions/subqueries/DDL or unreviewed expressions. Generator and gate must be able to disagree in tests.
7. Execute each certified mutation and assert exactly one identity match. Multi-match, trigger-affected mismatch or count ambiguity rolls back the whole supported batch. A proven no-op records matched1/changed0; do not interpret zero changed rows as row missing. If equality is established before UPDATE, skip that UPDATE to avoid introducing no-op trigger effects and report that fact.
8. Persist commit_requested before COMMIT, with fresh write authority per session policy. Positive server evidence -> committed; rollback confirmation -> rolled_back; lost response/process/cancel disposition -> outcome_unknown. Retire uncertain socket and quarantine the draft/operation. Never infer success/rollback from UI closure, local fencing, timeout, reread equality or operator acknowledgment.
9. Report operation state/version, per-row matched/changed evidence, result staleness and target identity. Refresh after confirmed completion is an explicit bounded read/new generation, not mutation replay. Unknown disables new apply/resend on the affected operation/session; an acknowledged unknown remains historical unknown, and only a separately reviewed fresh intent/session follows the journal's recovery policy.

## Test Scenarios

Implement `tests/verified_edits.rs` with shared scenario names and real PostgreSQL/MySQL/MariaDB/SQLite subjects; capability absence must produce an explicit disabled result rather than a fake pass. Frontend panel tests may live beside owned components; tests do not modify another section's implementation ownership.
- `edit.capability_gate`, T4: a supported table succeeds; joins, expressions, keyless/nullable/partial keys, unsupported codecs, schema drift, views and MyISAM fail before mutation. UI and direct Rust invocation return the same reason.
- `edit.injection_gate`, T3: hostile Unicode/quotes/semicolon/comments in values and quoted identifiers roundtrip literally, while fabricated catalog IDs/additional statements/invalid placeholders are refused. Mutate generator output deliberately to prove the independent gate rejects it; review label/typed parameters match actual driver target.
- `edit.optimistic_atomic`, T4: two clients edit the same original, deletion/concurrent schema change/multi-match/batch mid-error/lock timeout lead to conflict+confirmed supported rollback and preserved draft. Validate original rows from another connection; test uncertain rollback separately and never label it confirmed.
- `edit.noop_types`, T4: exact NULL/empty/zero/u64/decimal/bytes/time keys and values roundtrip; MySQL/MariaDB changed0/matched1 stays a no-op, changed0/matched0 is conflict, >1 is refusal. Exercise effective collations/CHAR padding, generated columns and typed unsupported fallback.
- `edit.unknown_quarantine`, T6: kill socket/process around COMMIT and local finalization, duplicate same key, stale reviews, cancel race and restart; assert zero automatic mutation replays, state/version resync, blocked apply and acknowledgment preserving unknown. Emit W2/M3 violation count; required value0 is a safety gate, not measured throughput.

## Implementation Strategy

Implement Rust eligibility/type/generator gate first, then prepare/review and standalone apply, then conflict/unknown UI and live fault scenarios. Consume section03 journal atomic methods and section05 execution barrier; do not fork those state machines. Register any new IPC command/DTO/error in the shared IPC specification via lead integration before merging.
Run targeted Rust tests and fmt/Clippy for changed crates, TypeScript/Vitest panel tests and native Tauri/WebView review/IME/focus checks. Run live engine/type/collation/count/lock/commit-loss certification and affected supply-chain checks. Browser fixtures or parser snapshots alone cannot prove rollback, driver types or remote commit behavior.
## Quality Gate

Complete the five acceptance scenarios per supported adapter/settings combination. Preserve certified read-only reasons for unsupported cases; no mock-only certification or unknown-as-success. Record real transaction evidence and zero automatic resend violations. Feature tests and live target evidence are NOT RUN at planning time.

## Risk & Rollback

Disable verified-edit capability to roll back the feature while retaining read-only browsing, backup operation and unresolved journal evidence. Never prune unknown records or loosen an AC to mark completion. Planning validation does not certify runtime behavior.

## Files

Owned edits.rs, generated_sql.rs, verified_edits.rs and EditDraft/EditReview/ConflictPanel paths are listed under Module Contract. Shared grid/Workbench screen/DTO/dispatcher integration remains implementation-lead-owned.

## Acceptance Criteria

- [ ] AC-06-1 join/expression/keyless/unsupported type 또는 nontransactional 대상의 편집을 구체적 read-only 이유와 함께 Rust·UI 양쪽에서 거부한다 — 근거: P3,P4,P5 — 검증: edit.capability_gate, T4
- [ ] AC-06-2 값은 bind하고 식별자는 scoped catalog와 dialect quoting으로 생성하며 독립 구조 gate 및 실제 대상·SQL·매개변수 review를 통과한 변경만 실행한다 — 근거: P3,P4,P5 — 검증: edit.injection_gate, T3
- [ ] AC-06-3 원본 행·schema 충돌이나 multi-match 시 지원 transactional effects의 전체 batch를 rollback하고 draft를 보존하며 불명 rollback을 성공으로 표시하지 않는다 — 근거: P3,P4,P5 — 검증: edit.optimistic_atomic, T4
- [ ] AC-06-4 MySQL/MariaDB no-op의 matched·changed 차이와 NULL·정밀 decimal 등 typed key/value를 검증하며 미지원 codec은 read-only로 유지한다 — 근거: P3,P4,P5 — 검증: edit.noop_types, T4
- [ ] AC-06-5 COMMIT 결과 불명은 새 apply·resend를 차단하고 자동 재실행하지 않으며 사용자 acknowledgment를 commit·rollback 증거로 바꾸지 않는다 — 근거: P3,P4,P5 — 검증: edit.unknown_quarantine, T6
