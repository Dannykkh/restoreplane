# Integration Notes

## Evidence and precedence
Current user direction > existing project contracts > integrated spec/plan > expert proposals > upstream observations. Research proposals pointing interactive drivers at the service are alternatives, not selected architecture. Old service/store wording in web research describes source examples, not the final ownership.

## Expert feedback integrated
See team-review.md for finding→section mapping. Spec now resolves cached write authorization, cancel settlement, per-statement partial outcomes, trigger limits, duplicate ACK credit, user DPAPI before bind, journal epochs and unknown acknowledgment.
External provider outputs preserve their judgments and NOT RUN limitations. Provider-generated local links were normalized for repository navigation without changing review findings.

## Nonintegrated proposals
- Existing service-hosted arbitrary SQL registry: rejected due privilege/lifecycle expansion. Reuse only metadata authorization and protection bridge.
- All engines/ODBC/MCP/DDL administration: deferred to avoid expanding unrequested authority and dependencies.
- Original-value equality as historical/ABA/exactly-once proof: rejected; unsupported guarantees remain visible.

## 역산 점검
- Retain P1~P8 work and safety/performance instrumentation. Defer new engines, plugins, agent access, account administration, bulk visual schema design, auto raw history/results cache and GPUI migration; reasons/reopen conditions are in spec Context Map.
- No feature unrelated to P1~P8 or safety instrumentation remains in the section manifest.
- Decision-maker simulation: conditional adoption, because one product can manage and protect DBs while retaining Tauri and existing backup authority. Conditions are measured bounded UI/IPC, certified per-engine writes and existing backup regression evidence. This AI assessment does not substitute actual user acceptance or authorize production DB operations.

## External plan review
Antigravity plan review completed (111.5s, exit0). Codex plan review timed out after300s with no final response; recorded NOT RUN to completion, not PASS. Earlier Codex domain-process and Antigravity domain-technical reviews completed independently.

### Integrated Antigravity findings
- Reuse existing domain/application contract crates and explicit CurrentUser extension of existing secrets; new Workbench runtime has one independent user/session lifecycle, not a duplicate generic application framework.
- Remove unnecessary new durable UI outbox/inbox broker: operation state/version is authoritative, hints deduplicate/resync by pull. Existing service outbox is preserved. User's duplicate-processing requirement remains covered without a broker.
- Keep service schema v4 concretely: descriptorRevision is a canonical hash derived from existing configuration/ownership read snapshot; authorityEpoch is service boot. Unsupported proof disables feature instead of vaguely requiring a future migration.
- Uncertain cancel physical-discard preserves unknown write transaction effects; elevated Workbench rejects before creating files; cached read requires server readonly certification.
- Add bounded lock/snapshot/idle limits, per-import child batch outcomes, sequential proven script boundaries, observed target-conflict gating, exact input/memory measurement boundaries.
- Rejected overclaim: target-conflict observation is not distributed mutual exclusion; third-party DB clients and later admission races cannot be ruled out. Authority linearizes at service admission, not target commit.
- Dictionary count complaint came from changing draft timing; final extension has30 core rows and existing service definitions unchanged.

### Detailed-design corrections
- edit.prepare now models insert/update/delete as a discriminated union, includes target/generated parameterized SQL and typed original/draft review. Insert omitted defaults are explicit; UPDATE DEFAULT is unsupported until certified.
- Additional read/export/import-parent terminal states are distinct from committed writes and reflected in schema/IPC/state diagram.
- Provider local links normalized to repository-relative links; trailing provider response tags/meta prose removed without altering findings.

### Final contract review
- Native security review found the historical Journal Epoch FK prevented safe header rotation. Removed that FK; native new-admission epoch validation remains required, historical epochs and unknown outcomes remain immutable.
- Unified request/error envelopes and edit cap100 rows (import batch200 separately). Moved shared result_stream.rs transport/credit ownership to section04; section05 consumes it without a reverse dependency.
- Flow review required explicit failed/unknown import stops, finalize failure, transaction opening/aborted/implicit-commit states and verified edit post-write/COMMIT guards. Corrected diagrams are syntax-checked in verification.md.

### Implementation consumption corrections

- Manual-transaction durable operation/statement FK linkage and StatementOutcome.transactionId complete the unreleased client v1 model; aggregate mixed scripts retain per-statement provenance. Five tables/service v4 and AC definitions are unchanged.
- catalog.graph adds the already-queried relation name to each node. This makes the bounded ER view usable without extra metadata requests; request/permission/count bounds and AC remain unchanged.
- Tauri raw request string plus strict recursive duplicate-key decode preserves payload validation before Value conversion. Native data/control/ACK lanes are independently bounded so saturated data admission cannot block credit or cleanup.
