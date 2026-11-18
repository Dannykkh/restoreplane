# GitHub Research — DBDelve and RestorePlane Management Workspace

Date: 2026-11-17. Scope: product and architecture research; no DBDelve code is incorporated.
Inputs: `spec.md`, `research-decision.md`, `research/codebase.md`, repository inspection and this session's audit.
The requested `initialspec.md` does not exist; `spec.md` contains the initial requirements.
DBDelve baseline: `307fe2433c2087672651aaa7c12a39c6af418407` (`main` at inspection).
Stable baseline: `v0.7.3`, published 2026-11-16, target `64f250252e3dce28bab627232b4bb2b805cd7548`.
Evidence labels: **Observed** = inspected source/API/log; **Proposed** = RestorePlane design recommendation.
Competitor research is limited to DBDelve; no additional competitor search was performed.

## 1. Product position and boundaries

- **Observed:** DBDelve is a Rust/GPUI desktop database client with native rendering, query editing and result browsing.
- **Observed:** it is not a scheduled backup, signed recovery evidence or restore orchestration service.
- **Observed:** native engines include PostgreSQL, MySQL, MariaDB, SQLite, Turso, DuckDB, Snowflake, SQL Server, MongoDB, Redis and ClickHouse.
- **Observed:** a separate ODBC bridge process extends connectivity; database-specific vendor drivers remain externally installed.
- **Proposed:** use its interaction patterns to add management tasks to RestorePlane while retaining Tauri/React and the Windows service.
- **Proposed:** start with existing PostgreSQL/MySQL/MariaDB/SQLite ownership and credentials; every other engine requires a separate decision.
- Sources: [README](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/README.md), [driver boundary](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/src/db/mod.rs).

## 2. UX and feature disposition

| DBDelve pattern / current capability | RestorePlane disposition | Adaptation and acceptance implication |
|---|---|---|
| Connections grouped into projects, home surface and command palette | Adopt | Map to existing Workspace/connection/target IDs; never create a parallel ownership tree. |
| Explorer for databases, schemas, relations and columns | Adopt | Page large catalogs; surface loading, partial visibility and permission errors explicitly. |
| Connection identity/color in title bar | Adopt | Keep Workspace, server, database, role and access mode visible before execution. |
| Query tabs separated from generated object-preview tabs | Adopt | User buffers remain owned by the user; generated preview SQL is independently inspectable. |
| SQL editor, execution, history and saved queries | Adopt with changes | History is scoped and permissioned; sensitive literals and persistence policy need an explicit contract. |
| Schema-aware completion and foreign-key JOIN suggestions | Later phase | Do not block the first browse/query/edit flow; use bounded, invalidatable catalog caches. |
| Read-only / read-write / full modes and destructive confirmation | Adopt with changes | Client classification assists the user; server credentials and service authorization enforce privileges. |
| User SQL executes without silent LIMIT rewriting | Adopt | Bound fetched rows/bytes/time independently and state when results were capped or stopped. |
| Generated preview SQL with explicit filters/sort/limit | Adopt with changes | Parameterize values, quote identifiers by dialect and validate generated statement shape. |
| Virtualized results and client row caps | Adopt with changes | Use paged/streamed bounded transport, server cursors where appropriate and backpressure across Tauri. |
| Primary-key-based cell update, review SQL before write | Adopt with changes | Typed values, original-value/version predicates, affected-row checks and transaction outcome states are mandatory. |
| Row details panel, JSON and binary presentation | Adopt | Preserve typed NULL, bytes, exact numeric and temporal values separately from display text. |
| Multi-row edits bracketed in a transaction | Adopt with changes | Define per-engine atomicity; reconnect must never replay an uncertain COMMIT automatically. |
| CSV/TSV/JSON row import with statement review | Later phase | Require bounded batches, duplicate handling and documented partial completion before enabling. |
| EXPLAIN visualization and ER diagrams | Later phase | Useful inspection aids; neither proves query safety nor backup/restore consistency. |
| DuckDB file opening for CSV/Excel/Parquet/JSON | Defer | Adds an embedded engine and file trust boundary outside the first four-engine scope. |
| SSH/SSM tunnels and password commands | Defer | Reuse only after service-owned executable, credential, timeout and lifecycle contracts exist. |
| ODBC plugins and additional native engines | Defer | No automatic plugin or engine expansion; capabilities must be independently certified. |
| Themes, glass blur and framework renderer patches | Do not transfer | Keep the established Tauri/React design system; no GPUI migration is required. |
| Backup before edit / inspect restored data | Add in RestorePlane | Connect existing backup/restore evidence to management tasks without treating a backup as transaction isolation. |

Source contracts: [AGENTS.md](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/AGENTS.md), [gates](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/docs/agents/gates.md), [session UX](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/docs/agents/session.md).

## 3. Architecture lessons and modifications

- **Observed:** engine dispatch remains inside `src/db`; UI consumes rendered values and engine/target capability data.
- **Proposed:** retain this boundary idea, but carry typed cells and origin/key metadata across the service contract so rendering cannot corrupt edit values.
- **Observed:** generated statements and user-owned buffers follow distinct validation and execution paths.
- **Proposed:** separate user query execution, trusted preview generation and guarded edit application in commands, authorization and audit events.
- **Observed:** blocking DB work runs outside GPUI's executor; cancellation uses a separate lane so it does not wait behind the work it stops.
- **Proposed:** reserve independent cancellation/control capacity and cap management work separately from backup/restore workers.
- **Observed:** an external plugin runs over stdin/stdout, reducing in-process driver coupling but not creating an OS sandbox.
- **Proposed:** exclude plugins from the first scope; no existing backup adapter is assumed to be a general interactive driver.
- **Observed:** row virtualization avoids rendering every cell, but uncapped results can still be held entirely in memory.
- **Proposed:** enforce session, cursor, row, byte, queue and lifetime budgets; do not send the whole result set through a Tauri JSON response.
- **Proposed:** design generation IDs, sequence numbers, bounded acknowledgements, duplicate delivery and reconnect invalidation explicitly.
- **Proposed:** model edit conflicts, partial batch completion, cancellation races and unknown commit outcomes; never infer exactly-once execution from request deduplication.
- Sources: [stack](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/docs/agents/stack.md), [plugins](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/docs/agents/plugins.md), local `research/codebase.md` and `research/web.md`.

## 4. Main, stable and unmerged work

- Stable and inspected main are distinct commits; this report does not claim every main change ships in v0.7.3.
- Stable v0.7.3 includes SSM tunnels, ER diagrams and binary-cell representation; main additionally includes SSM cleanup reporting.
- The following items were open PRs when inspected and must not be listed as implemented baseline capabilities.

| Proposal | Observed status | Planning treatment |
|---|---|---|
| [#268 MCP / Claude Code integration](https://github.com/ShayanAbbas1/dbdelve/pull/268) | Open | No automatic MCP scope; require an explicit later product/security decision. |
| [#267 shared explorer/tab strip across connections](https://github.com/ShayanAbbas1/dbdelve/pull/267) | Open | Connection isolation is mandatory; shared navigation is a design choice. |
| [#272 draft/no-primary-key editing](https://github.com/ShayanAbbas1/dbdelve/pull/272) | Open | Require unique row identity in the first editable scope; otherwise remain read-only. |
| [#203 schema and data SQL export](https://github.com/ShayanAbbas1/dbdelve/pull/203) | Open | Keep certified backup/restore separate from ad hoc export. |
| [#275 startup script / driver properties / keepalive](https://github.com/ShayanAbbas1/dbdelve/pull/275) | Open | Do not import settings or execution hooks merely because a proposal exists. |
| [#278 users, roles and grants](https://github.com/ShayanAbbas1/dbdelve/pull/278) | Open | DBA privilege administration is outside the initial management scope. |

## 5. Quality, performance and supply-chain evidence

- **Observed remotely:** [exact-main CI 43947665240](https://github.com/ShayanAbbas1/dbdelve/actions/runs/43947665240) passed Linux, macOS, Windows and Db2 jobs.
- Linux's 2,630-pass suite includes 353 `live_`-named tests and 110 `workspace::e2e` tests; it is not 2,630 isolated unit tests.
- Additional Linux steps passed timing 6, native conformance 92, psqlODBC host 20, bridge 13, Snowflake TLS 1 and renderer pixel 30 tests.
- Windows app suite: 2,243 passed / 508 ignored; macOS: 2,303 passed / 515 ignored. Plugin tests are separate.
- **Limit:** real Snowflake accounts, AWS SSM and Turso live tests are excluded; Db2 is `continue-on-error` despite passing this run.
- **Limit:** headless GPUI tests and software-renderer pixels do not certify every physical desktop, accessibility or IME path.
- **Observed:** [deny workflow](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/.github/workflows/deny.yml) runs `bans licenses sources`, omitting `advisories`.
- **Observed in the parent session:** `cargo audit` exited 1 with 23 advisory matches across 1,155 locked dependencies; advisory DB updated 2026-11-15.
- Audit DB commit: `02e2517f457a359fc6d3440ea4560c8ea188fee8`; matches concern locked package versions, not 23 demonstrated exploitable application paths.
- Tiberius explicitly enables its older TLS stack: `tiberius 0.12.3 → tokio-rustls 0.24.1 → rustls 0.21.12 → rustls-webpki 0.101.7`.
- Optional/target-specific dependencies require feature and reachability analysis before assigning application-level exposure; no upstream version pins are adopted here.
- Five GPUI/component crates are vendored with patches that must be reapplied on upgrades; the UI dependency maintenance cost is real.
- **Observed:** release assets have minisign signatures; macOS remains ad-hoc and unnotarized, and Windows installer lacks Authenticode signing.
- Bootstrap `install.sh` checks signatures only when minisign is installed; app self-update requires verification. These are different trust boundaries.
- Sources: [CI workflow](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/.github/workflows/ci.yml), [dependency decisions](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/Cargo.toml), [shipping](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/docs/agents/shipping.md).
- **Author benchmark:** [BENCHMARKS.md](https://github.com/ShayanAbbas1/dbdelve/blob/307fe2433c2087672651aaa7c12a39c6af418407/BENCHMARKS.md) measures DBDelve 0.3.6, not current 0.7.3, on one M1 Pro Mac/Postgres table.
- Both clients' row caps were disabled; one million rows used 4.9 GB in DBDelve. Virtual rendering is not bounded result storage.
- Its median timing method is documented, but the screen-driving harness is not published; speed claims are not independently reproduced here.
- User reports [#238 MySQL authentication](https://github.com/ShayanAbbas1/dbdelve/issues/238) and [#247 macOS crash](https://github.com/ShayanAbbas1/dbdelve/issues/247) remained open; neither was locally reproduced in this research.

## 6. Licensing and research execution boundary

- Current root/workspace license is **GPL-3.0-only**; RestorePlane currently uses MIT. Current DBDelve source is not copied, linked or vendored by this plan.
- [2026-10-18 transition commit](https://github.com/ShayanAbbas1/dbdelve/commit/432ac9dfdd7e5d3205c3c5f638f53ab274db8978) changed the license; README preserves the earlier MIT notice.
- [Parent snapshot 222ff1c6](https://github.com/ShayanAbbas1/dbdelve/blob/222ff1c649dd0e2ce86df3035276c4b74340d70d/LICENSE) and its Cargo manifest were checked: version 0.3.8 / MIT.
- This historical snapshot does not make later main code MIT. Any future reuse needs exact file/revision provenance and a separate decision.
- Product tasks, interaction observations and independently specified contracts are the inputs to RestorePlane's own implementation.
- **Executed:** repository/source inspection, GitHub API and remote CI log inspection; parent performed the cited dependency audit.
- **NOT RUN:** local DBDelve build, local app launch, real database queries, performance reproduction and exploit reproduction.
- **Proposed completion evidence:** RestorePlane must validate its own bounded query/edit flow, duplicate/reconnect behavior, engine isolation and backup integration.
