# Research Synthesis

Date: 2026-11-17. Evidence: [codebase](research/codebase.md), [official docs](research/web.md), [DBDelve](research/github.md).
Facts, proposals and NOT RUN are distinct. Runtime source baseline 08ba926; DBDelve reference 307fe24/stable v0.7.3.

## Existing implementation
- Existing Workspace ownership, profile/target/Scope identity, native backup/restore, durable work, request ledger, purpose-bound service secrets and notification outbox are reusable boundaries.
- Existing console_query is a local inventory projection, not arbitrary SQL. Existing native dump adapters are not interactive drivers.
- Machine DPAPI service secrets are distinct from user interactive credentials. A generic service SQL endpoint would expand privilege; selected design keeps interactive execution user-owned and records the credential duplication trade-off.
- The single service_request/command registry remains the authority for existing backup actions. New Workbench uses the existing Tauri Builder composition and a separate typed dispatcher because its lifecycle and privilege differ. This is a deliberate alternative to the research/codebase proposed service-hosted adapter, not an observed implementation.

## Capability adoption
- Adopt DBDelve schema/FK browsing, SQL tabs/completion/Explain, exact typed row inspector, reviewed edits, bounded import/export, ER diagram and target-colored context.
- Do not copy GPL code. Do not treat open MCP/schema-export/no-PK/draft-edit PRs as merged product features.
- Preserve existing native tool certification and manifest evidence; interactive success never upgrades backup support/verification state.

## Correctness requirements
- Remote commit and local SQLite are not one transaction. Idempotency deduplicates command admission; it cannot guarantee arbitrary target SQL exactly once.
- Timeout/disconnect/cancel intent are not rollback evidence. Preserve outcome_unknown and disallow automatic write replay.
- Local state+outbox and consumer receipt+projection can each commit atomically; event delivery remains at-least-once.
- Keyset pagination requires stable total order, but a stable cursor does not imply one snapshot over changing data.
- Generated writes require bound values, scoped quoted identifiers, independent structure checks, complete row identity and certified transactional storage.
- Typed values preserve NULL, exact integers/decimals, bytes and temporal metadata; no editing from formatted/truncated text.
- MySQL/MariaDB affected count can mean changed versus matched rows; no-op/conflict needs explicit adapter proof. DDL implicit commits prevent blanket atomic batch claims.

## Performance requirements
- Current Scope editor appends pages and maps all loaded objects; InventoryTable is not virtualized. Activity's 50-row page is bounded but is not proof of large query-grid performance.
- Current metadata inventory limit 100k and selection limit 1k are validation limits, not throughput evidence. MySQL discovery stdout 1MiB and default discovery timeout 15s are separate earlier bottlenecks.
- New result transport needs producer byte/row credit, consumer ACK, bounded caches, lossless data and row+column virtualization. Tauri Channel ordering supplies neither durable delivery nor an application memory bound.
- SQL user buffer remains unchanged; a result cap may not reduce server query cost. Stop/drain/cancel semantics must be engine-specific.
- Proposed thresholds are hypotheses to measure on a named Windows/WebView/hardware profile, not claims of measured capability.

## Safety and supply chain
- DBDelve cargo audit was actually executed in this conversation: 1,155 lockfile dependencies, 23 advisory/version matches, exit 1. Not all matches are proven reachable exploits; Excel/SQLite file-opening paths were statically traced. Do not import its dependency pins.
- New dependencies need fresh primary-document/version/license/advisory review at implementation, including existing pinned transitive crates before enabling file parsing and TLS.
- Prefer user scoped credentials, no default raw SQL/results persistence, metadata body limits, redacted diagnostics and no service credential export.

## Selected research boundaries
- Codebase, official docs and user-specified GitHub completed. Competitor UX covered by DBDelve analysis; no additional market search. Academic NOT APPLICABLE.
- Q1 first scope preference is optional/pending; conservative inferred scope is recorded in interview/spec. No private fact blocks planning.
- No live engine, new application build, adversarial file or performance benchmark ran during this planning phase.
