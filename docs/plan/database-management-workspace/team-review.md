# Integrated Team Review

Date: 2026-11-17. Inputs: six files in team-reviews/. Static design review; not six independent runtime verifications.

## Work performed
| Expert | Mode | Output | Execution evidence |
|---|---|---|---|
| Architecture | native artifact writer | architecture-analysis.md | source-backed design findings |
| UX | native artifact writer | ux-analysis.md | spec/flow/accessibility critique; no render |
| Red Team | native artifact writer | redteam-analysis.md | adversarial contract review; no exploit |
| Domain Research | main bounded synthesis | domain-research.md | MEDIUM; existing official sources reused |
| Domain Process | external Codex CLI 0.169.0, runtime default | domain-process-analysis.md | exit0; 204.2s, no source writes |
| Domain Technical | external Antigravity CLI 1.5.9, runtime default | domain-technical-analysis.md | exit0; 91.6s, no source writes |

## Critical findings and disposition
| Finding | Integration decision | Owning sections |
|---|---|---|
| canonical metadata/revision authority missing | new typed workbench.binding.authorize only; no service SQL; writes fresh validation, reads documented lease | 02,08 |
| user-owned process may be elevated | derive native integrity/SID/window; initial Workbench refuses elevated mode; separate user DPAPI | 02 |
| expiry/manual transaction/cancel races | status/cancel/rollback survive expiry; fresh COMMIT authority; execution epoch and cancel-settlement barrier | 02,05 |
| remote commit/local journal gap | unknown, no automatic replay, separate statement/transaction outcomes | 03,05,06 |
| engine type/key/matched-row claims | certify actual per-engine driver capabilities; no unsupported edit fallback | 02,04,06 |
| trigger/implicit commit outside batch rollback | transactional-effects scope only, per-statement partial evidence, explicit limitations | 05,06,07 |
| duplicate ACK can inflate credit | sent-sequence ledger/highest contiguous ACK/exact once byte retirement | 04,05 |
| encrypted content may leak to WAL first | metadata allowlist; DPAPI before bind; no plaintext fallback | 02,03,05 |
| unknown UX falsely resolves uncertainty | durable unknown vs separate operator acknowledgment; no automatic success/resend | 03,06 |
| target/focus/close states ambiguous | canonical context label, exact row identity anchors, IME guards, close disposition | 04,05,06,08 |

## Domain Decisions
- accepted-by-default: user-owned engine; separate credentials; single local writer; no elevated Workbench; read lease versus fresh write authority; explicit live/snapshot/partial states.
- accepted-by-default: cancel barrier, operation/statement/transaction outcomes, metadata-only journal, capability refusal, failure-injection acceptance.
- deferred-by-default: additional engines/plugins, account administration, MCP/AI, GPUI migration, cross-target 2PC, automatic raw result cache.
- user-confirmed: Tauri retention and correctness concerns from current conversation. Q1 preference remains inferred/pending, not user-confirmed.

## Dictionary Changes
Feature dictionary v3 finalized with30 terms. Existing Workspace/Lease/Transaction Owner/Backup Retention remain unchanged; Workbench-qualified terms resolve overload. ADD/REFINE duplicates normalized; no unresolved semantic conflicts.

## Global Dictionary Sync
inferred-skip: all new definitions depend on this product's authority and execution boundaries; no global memory changes.

## Remaining implementation gates
Driver/TLS/transaction/type/cancel certification; actual Windows/WebView rendering/accessibility/IME; data/memory benchmarks; crash/commit/disk tests. All NOT RUN at design time. Writing a plan or review does not satisfy those gates.
