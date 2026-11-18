<!-- SECTION_MANIFEST
section-01-bounded-performance
section-02-sessions-and-credentials
section-03-operation-journal
section-04-schema-explorer
section-05-sql-workspace
section-06-verified-editing
section-07-data-movement
section-08-protection-links
END_MANIFEST -->

# Implementation Sections

## Dependency Graph

| Section | Depends on | Provides | Parallel |
|---|---|---|---|
| section-01-bounded-performance | none | Windows/WebView bounded grid+IPC measurement artifact and target budgets | Wave1 |
| section-02-sessions-and-credentials | none | typed Workbench dispatcher, target binding, user-owned driver/session, user-state schema v1 | Wave1 |
| section-03-operation-journal | section-02 | operation admission/idempotency/state versions/restart reconciliation; no durable UI message broker | dependency ordered |
| section-04-schema-explorer | section-01, section-02, section-03 | versioned metadata, scoped keyset preview, canonical TypedCell, bounded row/column grid and shared result stream/credit | dependency ordered |
| section-05-sql-workspace | section-02, section-03, section-04 | explicit SQL/Explain, generation-bound runs/cancel, manual transaction, bounded typed result stream | dependency ordered |
| section-06-verified-editing | section-02, section-03, section-04, section-05 | typed insert/update/delete drafts, independent generated SQL checks, verified single-target apply | after prerequisites |
| section-07-data-movement | section-02, section-03, section-04, section-05 | streaming import batches and complete/partial export artifacts | after prerequisites |
| section-08-protection-links | section-02, section-03, section-04 | exact Scope handoff and readonly restored-target open using existing backup evidence | after prerequisites |

## Harness

Existing desktop composition is apps/console/src-tauri/src/lib.rs; manage WorkbenchState and explicit workbench_request/Channel here. Existing service_request remains the backup bridge. Existing App.tsx route dispatch adds workbench without a second shell. Existing application/domain/secrets crates own shared contract IDs/typed DTOs/CurrentUser scope; new Workbench crate owns runtime feature modules only.
All shared composition files (workspace Cargo.toml/Cargo.lock, Tauri lib.rs/capability JSON, App.tsx/route.ts/types.ts, service command-dispatch registration) are edited by the implementation lead after feature writer patches are reviewed. They are not parallel feature file ownership. Each section registers at exactly one declared WorkbenchState command group or benchmark route.

## Shared Foundation Rationale

section02 session/identity/current-user secret/store baseline is used by03–08; section03 durable admission/outcome/version protocol is used by04–08; section04 typed metadata/value/grid foundation is shared by05–08. These are actual shared consumers, not speculative layers. No generic plugin framework or local message broker is introduced. Functional verticals still own their feature's Rust handlers, UI and tests.

## Execution Order

1. Wave1:01 and02 independently. section01 measures risk before feature expansion; failure leads optimization/scope review within Tauri.
2.03 after02.
3.04 after01–03.
4.05 after02–04.
5.06,07,08 after their own prerequisites; disjoint feature modules can run in parallel.

## Flow Diagram Mapping

| Section | Diagrams | Nodes |
|---|---|---|
|01|browse-stream|credit/wait/send/apply plus independent measurements|
|02|session-access|integrity/authorize/credential/opened/session|
|03|operation-state,commit-loss-sequence|admission/dispatch/state versions/unknown recovery|
|04|browse-stream|cursor/order/page/typed grid/ACK consumption|
|05|query-cancel,transaction-state|run/barrier/target/settled/retire/result|
|06|verified-apply,commit-loss-sequence|eligible/auth/admit/tx/compare/write/commit|
|07|data-movement|batch/write/known/more/partial/complete/rename|
|08|protection-links|identity/plan/proof/actual/open|

## Owned Feature Files

### section-01-bounded-performance
Bounded performance proof. Owns: apps/console/bench/workbench-performance.ts; scripts/measure-workbench.ps1. Composition: isolated benchmark route/build flag; excluded from shipped navigation. Problems: P2.

### section-02-sessions-and-credentials
User-owned sessions and target bindings. Owns: crates/restoreplane-workbench/src/lib.rs; session.rs; credentials.rs; store.rs; driver/{mod,postgres,mysql,sqlite}.rs; apps/console/src-tauri/src/workbench.rs; crate manifests; restoreplane-domain/application qualified Workbench contracts and restoreplane-secrets CurrentUser provider (integration-owned). Composition: Tauri Builder.manage(WorkbenchState) and explicit workbench_request; existing service catalog adds only workbench.binding.authorize (no SQL). Problems: P1,P3,P6.

### section-03-operation-journal
Durable operation outcomes and message delivery. Owns: crates/restoreplane-workbench/src/journal.rs; events.rs; recovery.rs; tests/journal_recovery.rs. Composition: WorkbenchState operation boundary before driver dispatch. Problems: P5,P6.

### section-04-schema-explorer
Schema explorer and typed table browsing. Owns: crates/restoreplane-workbench/src/metadata.rs; browse.rs; types.rs; result_stream.rs; apps/console/src/features/workbench/{SchemaExplorer,VirtualResultGrid,RowInspector,SchemaDiagram}.tsx; screens/Workbench.tsx. Composition: App.tsx route workbench + WorkbenchState metadata/browse dispatch. Problems: P2,P4,P7.

### section-05-sql-workspace
SQL editor, query runs and manual transactions. Owns: crates/restoreplane-workbench/src/query.rs; policy.rs; saved_queries.rs; apps/console/src/features/workbench/{QueryEditor,QueryTabs,QueryResults,TransactionBar}.tsx. Composition: WorkbenchState query handlers and Workbench screen editor region. Problems: P2,P3,P5.

### section-06-verified-editing
Reviewed and conflict-checked data editing. Owns: crates/restoreplane-workbench/src/edits.rs; generated_sql.rs; apps/console/src/features/workbench/{EditDraft,EditReview,ConflictPanel}.tsx; tests/verified_edits.rs. Composition: WorkbenchState edit prepare/apply; grid draft actions. Problems: P3,P4,P5.

### section-07-data-movement
Bounded import and export. Owns: crates/restoreplane-workbench/src/data_movement.rs; apps/console/src/features/workbench/{ImportReview,ExportDialog}.tsx; tests/data_movement.rs. Composition: WorkbenchState data-movement handlers and table/results menus. Problems: P2,P5,P7.

### section-08-protection-links
Protection and restored-target inspection. Owns: crates/restoreplane-workbench/src/protection.rs; apps/console/src/features/workbench/ProtectionPanel.tsx; apps/console/src/lib/workbenchProtection.ts; tests/protection_links.rs. Composition: Workbench protection inspector→existing service_request; existing Recovery completion→new readonly Workbench session. Problems: P1,P6,P8.
