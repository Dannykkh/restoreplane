# UX Expert Analysis — Database Management Workspace

Date: 2026-11-17. Phase: Step 10 / Phase A. Status: design review; runtime NOT RUN.
Inputs: `spec.md`, `interview.md`, `research.md`, `research/{github,codebase,web}.md`, `personas-and-journeys.md`, `design-system.md`, root `DESIGN.md`, `docs/domain-dictionary.md` and database-management extension v1.
Boundary: Tauri/React stays; DBDelve supplies observations, not copied source. Recommendations below require the main planner's integration.
Decision: the dense explorer/editor/grid direction fits both personas; the unsafe gaps are state transitions and target/focus continuity, not visual style.

## Severity Findings

| ID | Severity | Evidence and gap | Required recommendation / acceptance |
|---|---|---|---|
| UX-01 | Critical | `spec.md:F1,F5,F7` binds identity internally, but `design-system.md:Spatial model` does not define the exact label repeated in execution, review and restore entry. A connection alias can name several actual databases. | One Target Context Label uses Workspace, connection alias, actual endpoint/file, database and role; review includes schema/table. Show source-versus-restored purpose text; color is secondary. Reconnect/revision changes invalidate the old review and never silently retarget a tab. |
| UX-02 | Critical | `spec.md:Consistency and delivery invariants` defines `outcome_unknown`; current OperationPanel mentions inspection without defining the user's next action. | Persistent Outcome Unknown view states “result not confirmed; do not resend,” operation ID, last evidence and target. Disable reapply for that operation, retain read-only inspection and open a separate reconciliation flow. Checking current rows cannot automatically prove execution count or resolve trigger effects. |
| UX-03 | High | `spec.md:F3` and `design-system.md:QueryTabs` require transaction-close confirmation but leave pending COMMIT/cancel and dirty-buffer disposition ambiguous. | Tab/window close distinguishes unsent edits, running reads, open transaction and unknown write. Offer Keep open or Request rollback only when meaningful; await acknowledgment before claiming rolled back. No automatic COMMIT. A lost connection yields unknown where evidence is absent. Closing a buffer never implicitly commits or discards a transaction in another tab. |
| UX-04 | High | `design-system.md:Accessibility and interaction` says roving focus survives virtualization; no identity, eviction or target-switch recovery is specified. | Anchor focus/selection by result-set identity plus stable row key and column ID, never viewport index. For keyless previews use result-local row identity, invalidate on refresh and keep read-only. Mount/scroll the destination before transferring DOM focus; reject stale-generation keyboard actions. Test vertical/horizontal eviction, paging and dialog return. |
| UX-05 | High | `spec.md:F2,F4` has live-per-page, unknown counts, caps and eviction; the layout only lists a footer. Users may read a complete current page as a complete query or snapshot. | Separate Result Completeness, Consistency View and freshness in visible text. Distinguish complete, capped, cancelled, interrupted, partial and stale; show received rows rather than false totals. Cap, unordered preview and mixed live pages never inherit green “complete.” Sorting/filter changes start a new result generation and invalidate old selection. |
| UX-06 | High | `spec.md:F4,F5` separates Typed Cell/display but no copy/edit entry contract is stated. | Exact copy uses the complete canonical value; formatted display copy is explicitly labeled. NULL, empty, bytes, decimal precision and timezone provenance remain distinguishable. Truncation badge opens bounded details; unsupported/lossy cells cannot enter edit. Review shows original/draft/current basis and typed parameter values without relying only on SQL text. |
| UX-07 | High | `spec.md:F3` requires unchanged user SQL and restrictive Full policy; `design-system.md` does not distinguish editor buffer, selected statement and generated preview in execution feedback. | Run UI identifies selection/statement/all and displays the exact run text in an explicit ephemeral inspection view. Header sort/filter changes generated previews only unless the user requests editor transformation. Explain defaults to plan-only; executing ANALYZE has a separate action and risk review. Korean IME composition must not trigger Run or Apply on Enter. |
| UX-08 | High | `spec.md:F5` says conflict rolls back a supported batch; review does not specify what happens to the user's draft after conflict. | Preserve unsent intent, display affected row/key and original/current/draft differences, and allow Refresh basis or Discard draft. Refresh never silently reapplies. Show no-op separately from changed-row counts and do not offer “Undo” unless a new conditional compensating edit is explicitly reviewed. |
| UX-09 | High | `spec.md:F7` links backup-before-change but the persona flow can imply backup makes a later edit safely reversible. | Display the backup dependency's target, completion/evidence and age. Continue to edit is an explicit new user action; never auto-dispatch SQL after backup completion. “Open restored database” requires completed restore proof and actual new identity, opens read-only and has no original-target fallback. |
| UX-10 | Medium | Explorer is lazy/paged, but `personas-and-journeys.md` assumes finding an object is immediate. | Distinguish not loaded, no match, permission denied, stale catalog and failed page. Search is server-scoped with a visible scope and bounded results. Preserve expansion/search on layout collapse; refresh invalidates metadata-dependent edit reviews and completion entries. |
| UX-11 | Medium | F4's bounded streaming/ACK prevents queue growth but no backpressure or cancel-feedback presentation is defined. | Show waiting for capacity, receiving and cancel requested separately. Throttle progress announcements; avoid per-row live-region updates. Cancel remains keyboard reachable under load and names its Query Run. Completion may win a cancel race; show observed outcome rather than rewriting it to cancelled. |
| UX-12 | Medium | `DESIGN.md` favors dense operational screens; the new wide grid/inspector introduces accessibility and reduced-window risks beyond existing dashboards. | At narrow width collapse explorer/inspector while retaining identity and transaction/unknown indicators. Provide keyboard-only row details, logical header associations and non-color status text. Test light/dark, 200% zoom, high contrast, Korean fonts, reduced motion and Windows WebView assistive technology on actual render. |

## End-to-End Interaction Contract

1. Enter through existing Workspace navigation; choose or confirm Database Target and a distinct user credential. Missing interactive credentials explain the required action without suggesting service-secret export.
2. Explorer opens a generated table preview; the preview's SQL, ordering, completeness and Consistency View can be inspected without modifying a user query buffer.
3. Query tabs remain bound to one target/session. Switching the active explorer or another tab never changes an existing tab's target; manual transaction state stays attached to its live connection.
4. Select a proven row and supported cell, create an Edit Draft, review exact typed differences and explicitly Apply. Read-only reasons remain discoverable without hover.
5. Running status identifies the operation; cancel requested remains distinct from terminal evidence. Duplicate notifications do not repeat focus changes, dialogs or success announcements.
6. Commit evidence yields committed; a confirmed rollback yields rolled back; lost evidence yields Outcome Unknown with no write replay. Draft reset follows conclusive state only.
7. Protection actions navigate through existing Backup Scope/Restore flows with exact object identity and return context. Viewing restored rows is separate from claiming restored semantic equality.

## Confirmation and Close Matrix

| State | Allowed user action | Required visible result |
|---|---|---|
| Unsent buffer / Edit Draft | Keep editing; explicit discard; explicitly save named query if supported | State what is lost; no default disk persistence and no false DB rollback claim. |
| Running read | Keep open; request cancel and close view | Read subscription stops; session resource cleanup is tracked. |
| Open manual transaction | Keep open; request rollback and close after acknowledgment | Target, transaction age and unsaved buffer shown separately; no implicit commit. |
| COMMIT dispatched / running write | Keep operation visible; request close with warning about losing the view | Outcome tracking remains in native journal; no replay or claimed cancellation/rollback. |
| Outcome Unknown | Close presentation; open reconciliation/diagnostics later | Durable unresolved operation remains discoverable at restart. |
| Conflict with proven rollback | Inspect differences; refresh basis; discard draft | Whole supported batch rolled back; unsent intent preserved until explicit disposition. |

Window exit/process termination cannot promise rollback; native recovery must preserve uncertainty even if the confirmation dialog never completes.

## Delivery Waves and Validation

- Wave 1 implements shared target labels, credentials/session entry, explorer, bounded typed preview/results and query execution with cancellation/unknown-state handling; these are prerequisites for editing.
- Wave 2 adds Verified Apply with conflict/reconciliation and transaction-close acceptance gates; it cannot bypass Wave 1 budgets or accessibility.
- Separate later implementation sections cover completion, plan visualization, ER diagrams, import/export and protection links already selected by the spec. Deferral controls order, not their removal from the agreed roadmap.
- New engines/plugins, MCP, DBA account administration and platform ports remain separate scope decisions; no reference-project PR is treated as an implemented baseline.
- UX acceptance fixtures: two same-named databases in different targets; refresh during review; stale-generation result; keyless preview; NULL/empty/u64-max/decimal/binary/LOB; conflict/no-op; lost COMMIT reply; duplicate/gap events; close during transaction.
- Accessibility fixtures: keyboard-only explorer/editor/grid/review, virtual row/column eviction with focus, screenreader row detail, IME Enter, zoom/narrow layout, themes/fonts/reduced motion. Measure responsiveness and memory under slow consumers on actual Windows/WebView hardware.
- **NOT RUN:** application render, accessibility assistive technology, IME, timing and usability sessions. Persona frequency and budget values remain inferred/proposed.

## Dictionary Updates

| Action | Term / identifier | Definition or clarification | Reason / affected contract |
|---|---|---|---|
| Add | Target Context Label / `target_context_label` | Human-readable projection of bound Workspace/endpoint/database/role/purpose; never an authority token. | Same target meaning across editor, review, confirmation and restore entry (UX-01). |
| Add | Result Completeness / `result_completeness` | Observed completion/cap/interruption status of one Result Set, separate from its Consistency View and freshness. | Avoid complete-page/snapshot ambiguity (UX-05). |
| Add | Selection Anchor / `selection_anchor` | Result identity plus row/column identity retained independently from mounted viewport cells. | Virtual focus, refresh invalidation and exact selected-row actions (UX-04). |
| Clarify | Edit Draft / `edit_draft` | Unsent intent and original basis; survives conflict presentation but cannot execute after generation/schema invalidation without new review. | Preserve work without silently reapplying it (UX-08). |
| Clarify | Outcome Unknown / `outcome_unknown` | Unresolved target effect, retained durably; cancellation, window close or matching final data does not resolve it. | Prevent resend/success affordances from overstating evidence (UX-02/03). |
| Add | Transaction Disposition / `transaction_disposition` | Explicit keep-open or requested rollback choice and the observed acknowledgment/unknown state when leaving a live manual transaction. | Closing an editor buffer and ending a transaction are distinct acts (UX-03). |
| Clarify | Verified Apply / `verified_apply` | Certified conditional target transaction; “verified” covers its declared checks, not backup verification level or guaranteed historical-change detection. | Preserve existing Verify Level terminology and ABA limits (UX-08/09). |

Integrate accepted changes into the feature dictionary v2; do not rename existing Workspace, Database Target, Snapshot or Recovery Point.
