# Chronos Log

## Active Run: Program-Owned Backup Engine and Selective Backup Scope

### Run Metadata

- Started: 2026-08-24T00:04:25+09:00
- Project: `D:\git\dbbackupsystem`
- Engine: native goal + Chronos FIND/FIX/VERIFY loop
- Scope: program-owned scheduler, durable Backup Scope, bounded native execution, automatic retention, Workspace status UI
- Constraints: preserve the existing dirty worktree, no automatic commit, no per-Plan Windows Task Scheduler or SCM registration, no false whole-database recovery claims for partial backups
- Gate: focused RED/GREEN tests, workspace Rust fmt/clippy/tests, console Vitest/build/Playwright, provider restore evidence, large-payload bounded-resource evidence
- Promise: the application owns schedules and durable work while one background engine survives UI exit; PostgreSQL/MySQL/MariaDB can back up either a whole database or an exact selected-table set in one native dump invocation, and the UI reports truthful coverage, execution, storage, and retention state.

### Completion Contract

| Requirement | Pass evidence | State |
|---|---|---|
| PBE-R01. Operational Store schema v4 represents reusable whole-database and explicit-table scopes, immutable revision/run snapshots, and safely backfills every v3 target as a whole-database scope. | migration RED/GREEN, v3-to-v4 backfill, constraint and immutability tests | proved |
| PBE-R02. PostgreSQL/MySQL/MariaDB resolve and validate exact selected tables and pass the complete selection to one native dump process; manifests label partial coverage and dependency limitations. | provider argument tests plus live dump/decrypt/restore evidence | proved |
| PBE-R03. The engine itself evaluates published schedules, persists its cursor, applies catch-up and overlap policy, and materializes durable work without Windows Task Scheduler entries. | scheduler restart/DST/idempotency/overlap integration tests and SCM assembly test | proved |
| PBE-R04. Native Backup claims pass atomic resource and spool admission, enforce an execution deadline, limit destination pressure, and recover expired claims/orphan spools after process restart. | admission saturation, disk-floor, timeout/abort, restart reconciliation, and spool scavenger tests | proved |
| PBE-R05. GFS retention is configured consistently and the engine automatically plans tombstone/delete work while preserving pins, legal holds, active restores, object locks, and the last compliant replica. | planner/controller/delete-worker safety and lifecycle tests | proved |
| PBE-R06. Workspace/Plan UI can discover tables, choose whole database or exact tables, show protected/unprotected coverage, and display scheduler, queue, spool, and retention health. | TypeScript contract tests, Vitest, production build, Chromium workflow/accessibility/screenshots | proved |
| PBE-R07. Documentation and installation semantics state that SCM owns only the singleton engine lifecycle; configuration remains in the Operational Store and normal uninstall drains work without deleting backups. | README/architecture/installer assertions and full regression gates | proved |

### Active Cycle Log

#### Cycle 1 - schema v4 Backup Scope contract

- ISSUE: v3 binds a Plan revision directly to a Database Target, so it cannot distinguish full-database recovery from an exact selected-table set or freeze that selection for a historical run.
- RED: `schema_v4_backup_scopes` failed to compile because `apply_operational_store_v4` did not exist.
- FIX: added schema v4 with append-only whole/explicit Scope selectors, exact object rows, immutable revision/run snapshots, paged discovery objects, durable scheduler cursors, automatic whole-Scope compatibility, and transactional v3 target/revision/run backfill with a real canonical BLAKE3.
- VERIFY: focused migration contract passed 3/3, including bootstrap table/version, v3 history backfill, whole-Scope object rejection, and published snapshot immutability.
- NEXT: expose typed Scope repository operations and make Plan save/materialization consume `scopeIds` while retaining `targetIds` as a compatibility alias.

#### Cycle 2 - exact selective providers and signed Scope binding

- ISSUE: provider execution and manifests could not prove whether a Recovery Point covered a whole database or an exact table set.
- RED: selection-contract tests exposed duplicate/empty selectors, PostgreSQL globals leakage, MySQL routines/events leakage, and missing run-to-manifest Scope validation.
- FIX: added canonical explicit-table selection, one native dump process per Scope, strict PostgreSQL qualified-table matching, MySQL/MariaDB positional table selection, immutable Scope snapshots, and fail-closed signed manifest/run binding.
- VERIFY: core/provider suites passed; the digest-pinned Docker selective fixture restored PostgreSQL, MySQL, and MariaDB selected tables while proving excluded tables and database-wide objects absent.
- NEXT: connect program-owned schedule materialization and resource-bounded native execution.

#### Cycle 3 - program-owned scheduler and lifecycle

- ISSUE: schedule evaluation existed as a library test seam but the installed engine did not continuously materialize due work; Windows Task Scheduler semantics conflicted with application-owned schedules.
- RED: service assembly and overlap tests showed no running scheduler authority, no persisted cursor, and no drainable lifecycle handle.
- FIX: added durable scheduler cursors, bounded catch-up, hourly schedule support, `coalesce_one`, a service-owned scheduler thread, and stop/join integration with the pipe host.
- VERIFY: scheduler, catalog, and service regressions passed, including restart/catch-up and active Backup overlap behavior.
- NEXT: admit native Backup claims against global resources and enforce large-job safety.

#### Cycle 4 - native admission, deadline, and crash reconciliation

- ISSUE: native Backup workers bypassed durable resource admission, reserved no spool space, could hang during planning/publication, and could leave fast-restart claims stranded.
- RED: saturation, disk-floor, never-planning source, fast-restart, committing-recovery, and live-child identity tests reproduced each unsafe boundary.
- FIX: added atomic global/source/destination/spool admission, disk floor and recursive spool cleanup, one absolute six-hour pre-commit deadline, PID/start/path/SHA child evidence, and a five-second periodic reconciler with a fresh bounded recovery source per pass.
- VERIFY: admission/reconciliation/service tests and strict Clippy passed; a real short-lived Windows child proved starting-to-running registration, deadline termination, and exit recording.
- NEXT: make manifest-last publication and cancellation settle only on a known commit boundary.

#### Cycle 5 - manifest-last publication and retention deletion integrity

- ISSUE: a publication could continue past the absolute deadline, while retention could delete payload objects even when the Set Manifest marker deletion failed.
- RED: a pending destination reproduced post-begin timeout with an unresolved commit; retention tests proved payload deletion continued after locked/retryable/terminal marker failure.
- FIX: pre-begin timeout remains retryable with cleanup, post-begin uncertainty preserves `committing`, recovery intent, spool, and reservation for reconciliation; retention now gates every payload delete on successful marker deletion and records explicit blocked observations.
- VERIFY: service Backup 16/16, pipeline retention 12/12, catalog retention 8/8, focused fmt and Clippy all passed.
- NEXT: project automatic retention and verification truth into the operator UI.

#### Cycle 6 - automatic retention, verification truth, and health projection

- ISSUE: retention was not continuously executed, verification terminal results disappeared from derived health, and dashboards could show inferred or stale protection state.
- RED: zero-candidate retention, failed-delete attention, verification success/failure/cancel/unsupported, L3 restore-drill, rebuild replay, and unknown-state UI tests failed against the old projections.
- FIX: attached a drainable automatic retention runtime, persisted scan heartbeat and failures, projected terminal local verification evidence atomically, replayed it during rebuild, and exposed tri-state scope/target scheduler, recovery, verification, and retention health.
- VERIFY: catalog/runtime/service projection suites and Console tests/build passed; unknown and partial evidence remain non-current and actionable.
- NEXT: remove unbounded nested inventory responses and prevent cached or partial UI data from authorizing mutations.

#### Cycle 7 - bounded operational inventory and crash-safe mutations

- ISSUE: connection, discovery, Scope, activity, and snapshot responses could exceed the IPC ceiling; response loss could repeat mutations; incomplete inventory could be widened into whole-database protection.
- RED: byte-budget, cursor/filter/count, mutation crash-window, transport replay, and partial-inventory action tests reproduced the unsafe paths.
- FIX: added keyset-paged target/discovery/Scope object APIs, server-side Scope filters, bounded activity, summary-only snapshot lists with paged detail sections, a terminal `APPLICATION_OUTCOME_UNKNOWN` ledger state, and an in-memory UI attempt registry that only reuses keys for transport ambiguity.
- VERIFY: focused Rust and Console contracts passed; the remaining full-suite gate is recorded in the next cycle only after the final shared tree is stable.
- NEXT: prove the release package trust boundary and execute every safe full regression gate.

#### Cycle 8 - restore, verification, and native process ownership

- ISSUE: production SCM omitted Restore and Verification claim runtimes, native dump children were not owned by a Job Object, and restore target identity could drift between preflight and destructive DDL.
- RED: queued Restore/Verification work remained unclaimed; crash reconciliation observed unowned native children; same-version/different-server identity and post-start native-tool tamper fixtures crossed the old boundary.
- FIX: attached drainable Restore and Verification runtimes, moved native children under named kill-on-close Job Objects, bound executable and dependency hashes through launch, and revalidated signed Scope, replica, live server identity, target absence, deadline, lease, and cancellation immediately before destructive work.
- VERIFY: service Restore 12/12, runtime Restore 13 passed/1 external Docker ignored, Verification 4/4, SQLite 9/9, PostgreSQL 15/15, MySQL/MariaDB 48/48, runtime supervisor/reconciliation and strict Clippy all passed.
- NEXT: bind the installed package, provider certifications, installer ACL, and exact signed artifact evidence into one fail-closed release boundary.

#### Cycle 9 - package certification and Windows installer trust

- ISSUE: bundle-only execution still needed exact package certification, dependency-closure locking, immutable install-root transition, and evidence that a signed artifact rather than a source rebuild performs the advertised operations.
- RED: missing/tampered provider documents, extra or replaced dependencies, writable/reparse roots, manifest swaps, cross-build evidence, and installed-artifact bypass fixtures each failed the intended contract.
- FIX: added atomic provider certification bootstrap, executable/dependency/ancestor handle locks, exact ACL and FILE_ID provenance, whole-root handle quarantine, externally pinned package-manifest digest, verified rollback/start gates, and an exact-artifact functional qualification job using only the installed CLI/service IPC.
- VERIFY: package bootstrap 13/13, runtime dependency supervisor 15/15 and full 64/64, PowerShell scripts/tests 17/17, parser checks, artifact/provider/release-policy contracts, and two independent HIGH/MEDIUM-zero security reviews passed.
- NEXT: freeze documentation and execute the current-tree full Rust, Console, browser, and three-engine selective evidence gates.

#### Cycle 10 - final regression and evidence freeze

- ISSUE: the first full Rust run exposed a timing-dependent fast-restart test: its 60 ms lease could expire during fixture setup, so a single immediate reconciliation contradicted the test's two-cycle assertion.
- ROOT CAUSE: the test used elapsed wall time as a hidden precondition instead of observing completion of the initial pass; production reconciliation was not at fault.
- FIX: the fixture now starts with a bounded non-expiring lease, waits for the first completed pass, then atomically expires the exact fenced claim before observing the periodic pass.
- VERIFY: the regression passed 10 consecutive runs; `cargo test --workspace --all-targets --locked`, workspace strict Clippy, rustfmt, and diff checks passed. Console Vitest passed 27 files/94 tests, production build passed, and Chromium Playwright passed 36/36 across three viewports.
- NEXT: regenerate the digest-bound PostgreSQL/MySQL/MariaDB selective live artifact after this documentation freeze; production promotion remains parked until external signing, clean-runner evidence, and owner stop metrics exist.

## Active Run: Selective DBX Operations UX and Service Backup Dispatch

### Run Metadata

- Started: 2026-08-13T14:32:00+09:00
- Completed: 2026-08-13T16:32:00+09:00
- Project: `D:\git\dbbackupsystem`
- Engine: native goal + direct Chronos loop
- Scope: Backup Plans 운영 UX, `backup.enqueue` IPC/service dispatch, durable work status/cancel, 기존 backup runtime 연결
- Constraints: 기존 dirty worktree 보존, 자동 commit 금지, 브라우저 fixture를 실백업 증거로 사용 금지, 한 사이클 한 이슈
- Gate: focused Rust/React test 뒤 `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `npm test`, `npm run build`, Chromium Playwright
- Promise: 선택적 DBX UX와 실제 서비스 backup enqueue/status/cancel 경계가 구현되고 현재 revision의 자동 검증이 모두 통과하거나, 외부 DB 자격증명만 Owner Decision Brief로 분리됨

### Completion Contract

| Requirement | Pass evidence | State |
|---|---|---|
| DBX-R01. Backup Plans에서 사람이 읽는 예약, Run Now, durable 작업 ID, 정직한 진행률, 취소를 제공한다. | Vitest, production build, Chromium 1024/1280/1600 + axe + screenshot | proved |
| DBX-R02. 실제 서비스가 `backup.enqueue`를 authoritative plan revision에서 durable work로 생성한다. | multi-target durable IPC replay/atomicity integration tests | proved |
| DBX-R03. `work.get`이 backup subject와 측정 가능한 경우에만 progress evidence를 반환한다. | catalog query tests, TypeScript contract tests, Chromium task-center flow | proved |
| DBX-R04. 모든 취소 요청이 최신 fencing token에 결합된다. | stale-fence tests, commit-boundary tests, child-abort pipeline test | proved |
| DBX-R05. PostgreSQL/MySQL/MariaDB/SQLite provider와 전체 품질 게이트가 회귀 없이 통과한다. | workspace fmt/clippy/tests, console 23/23 + 30/30, Docker G2 3/3 | proved |
| DBX-R06. service Backup worker가 실제 provider 출력에서 암호화·서명된 Recovery Point를 원자적으로 만든다. | recoverable SQLite service E2E, signed manifest/decrypt/restore oracle, G2 native restore | proved |

### Active Cycle Log

#### Cycle 5 - atomic Backup claim projection commit

- ISSUE: no normal Backup worker could atomically turn a fenced committing claim and a verified Set Manifest into a durable Recovery Point; `complete_claim` alone could falsely report success without recoverable catalog evidence.
- RED: `active_backup_claim_atomically_commits_verified_recovery_point_and_measured_bytes` failed to compile because `backup_claim_context` and `commit_backup_claim` did not exist.
- FIX: added immutable Backup claim context lookup, service-owned Backup running transition, measured plaintext aggregation, and one immediate transaction that verifies the Backup origin, ingests the complete projection, schedules post-backup verification, links the Recovery Point, and settles Backup/Work/Attempt/Occurrence.
- VERIFY: focused catalog regression passed 1/1 and proved a stale fenced claim cannot be reused after terminal settlement.
- NEXT: record ciphertext hashes during the existing compression/encryption pass so the native Backup worker can publish a truthfully verifiable Manifest without rereading payloads.

#### Cycle 6 - single-pass ciphertext evidence

- ISSUE: `SpoolCommit` exposed plaintext evidence and ciphertext length but not the ciphertext BLAKE3 required by Set Manifest v2, forcing a future worker either to invent evidence or reread every spool.
- RED: the pipeline round-trip test failed to compile because `SpoolCommit.ciphertext_blake3` was absent.
- FIX: wrapped the encrypted output writer in a second `HashingWriter`, finalized it only after zstd and STREAM encryption, and checked its byte counter against the fsynced spool length.
- VERIFY: the focused pipeline test passed 1/1 and compared the recorded digest with a fresh BLAKE3 of the exact committed encrypted file.
- NEXT: persist and reopen service signing/encryption key material through the DPAPI Secret Store.

#### Cycle 1 — Backup Plans operational task center

- ISSUE: `지금 실행`이 동작하지 않았고 실행 중 backup의 durable ID, 진행 단계, 취소 상태를 한 화면에서 볼 수 없었다. Recovery 취소도 service 계약의 fencing token을 누락했다.
- RED: 새 task center/API/gate/cron/screen 테스트가 missing component, payload mismatch, dead action으로 실패했다.
- FIX: measured/indeterminate progress를 구분하는 작업 센터, service-authority gate, `backup.enqueue`/poll/cancel 연결, cron 요약, Recovery fenced cancel, 반응형 container query를 구현했다.
- VERIFY: Vitest 21/21, production build, 새 Playwright 시나리오 3/3, axe WCAG 위반 0, 1024/1600 screenshot 직접 검수 통과.
- NEXT: 실제 Rust `ServiceApplication`의 누락된 `BackupEnqueue` dispatch를 RED 테스트로 고정한다.

#### Cycle 2 — atomic multi-target `backup.enqueue`

- ISSUE: `backup.enqueue` was classified as an enqueue mutation but the durable dispatcher rejected it with `IPC_TYPED_TRANSACTION_REQUIRED`; a Plan run therefore could not create authoritative service work.
- RED: `manual_backup_dispatch_materializes_durable_work_and_replays_atomically` failed at the dispatcher boundary with `TypedTransactionRequired`.
- FIX: added a typed IPC transaction that resolves the enabled published revision, creates one manual Plan Occurrence, one Backup Run and Work Item per Target, emits per-target activity, and commits the replay payload in the IPC ledger atomically. The response exposes Target-level `backupRunId` and `workItemId` values instead of an aggregate fake work ID.
- VERIFY: focused RED became GREEN; the two-Target assertion passed; the complete `restore_ipc_dispatch` suite passed 6/6, including idempotency conflict and exact replay checks.
- NEXT: extend `work.get` with durable backup subject/timestamps and truthful progress, then update the console to track every returned Target work item.

#### Cycle 3 — truthful `work.get` and Target-level console tracking

- ISSUE: `work.get` exposed queue mechanics but no Backup Plan/Target context or timestamps, while the console assumed one aggregate Work Item for a multi-Target Plan.
- RED: the catalog contract failed to compile because `ConsoleWorkItem` lacked `subject`, `progress`, `createdAtMs`, and `updatedAtMs`; UI tests failed because only one aggregate target and duplicate Plan-only accessible labels existed.
- FIX: joined Backup Work to its immutable revision, Plan, and Database Target; returned exact Target context and durable timestamps. Progress stays indeterminate until a succeeded Backup Run has measured plaintext bytes. The enqueue TypeScript contract and browser fixture now return and track one Work Item per Target with Target-specific accessible controls.
- VERIFY: catalog console query 3/3, service application 4/4, and focused Vitest 8/8 passed. The completed-byte test proved 4096/4096 only after durable success; queued work remained unmeasured.
- NEXT: expose stale fencing-token cancellation as a stable application error and prove wrong-token rejection before the current token succeeds.

#### Cycle 4 — stable fenced cancellation failure

- ISSUE: the runtime correctly rejected a stale fencing token, but `ServiceApplication` collapsed the result into retryable `APPLICATION_INTERNAL`, hiding the required refresh boundary from the console.
- RED: the service application regression expected `WORK_ITEM_FENCE_STALE` and observed `APPLICATION_INTERNAL`.
- FIX: added a non-retryable `WorkItemFenceStale` application failure and mapped `RepositoryError::StaleFence` without changing the queued Work Item.
- VERIFY: the focused service test proved the wrong token leaves state `queued`, then token 0 cancels the same Work Item successfully.
- NEXT: connect claimed Backup Work to the native provider pipeline and service lifecycle for PostgreSQL, MySQL, MariaDB, and SQLite.

#### Cycle 7 - persistent key custody and recoverable service backup

- ISSUE: a durable Backup claim still had no service-owned executor, persistent signing/encryption keys, destination bindings, or recoverable end-to-end proof.
- RED: the new SQLite service integration could enqueue and claim work but could not produce a signed, decryptable Recovery Point.
- FIX: added `NativeBackupExecutor`, DPAPI-backed service signing/KEK material, role-bound Local/S3/SSH destinations, per-part DEK wraps, signed Set Manifest v2, and atomic fenced settlement.
- VERIFY: `backup_worker_sqlite` created a real online SQLite backup, verified the persistent signing key, unwrapped the DEK, decrypted/decompressed the part, reopened it, and found both seeded rows.
- NEXT: bind the executor to SCM lifecycle with bounded concurrency.

#### Cycle 8 - bounded service worker lifecycle

- ISSUE: native execution existed only as a callable executor and could not consume production service claims or drain with the host.
- RED: SCM/service lifecycle compilation and worker ownership tests exposed the missing handle.
- FIX: attached four fixed Backup worker threads to RehearsalReady/ProductionReady service startup, serialized heavy dumps per Connection ID, retained the handle in `ServiceApplication`, and stopped it during drain.
- VERIFY: source-gate tests proved same-Connection serialization and distinct-Connection concurrency; complete service tests passed.
- NEXT: make cancellation and the publish boundary crash-safe.

#### Cycle 9 - commit and cancellation boundary

- ISSUE: allowing cancellation after a claim entered `committing` could publish artifacts and then prevent catalog settlement.
- RED: the atomic commit regression showed a committing claim could transition to cancel-requested.
- FIX: added `CommitInProgress`, rejected late cancellation without mutating the claim, disabled the UI cancel control during commit, and moved manifest validation/tool hashing before `begin_claim_commit`.
- VERIFY: catalog and BackupTaskCenter tests proved pre-commit cancellation remains fenced while post-boundary commit completes.
- NEXT: ensure cancellation kills native dump children and removes partial spools.

#### Cycle 10 - native child abort and spool cleanup

- ISSUE: dropping an async dump future could leave `pg_dump`/`mysqldump` alive and leak a partial encrypted spool.
- RED: `cancellation_aborts_dump_joins_encoder_and_removes_spool` reproduced the orphan boundary.
- FIX: added abort callbacks to stream/directory artifacts, killed and awaited native children, joined the blocking encoder, and deleted the partial spool before returning cancelled.
- VERIFY: pipeline E2E passed 6/6 and the service cancellation monitor no longer ignores thread-spawn failure.
- NEXT: align provider TLS and manifest evidence with actual runtime behavior.

#### Cycle 11 - provider TLS, tool selection, and MySQL evidence

- ISSUE: profile TLS modes were not consistently propagated to native clients, PATH tools lacked signed identity evidence, and MySQL pre-dump consistency/binlog data was collected but discarded.
- RED: provider TLS mapping and source-evidence tests failed before the new contracts existed.
- FIX: mapped PostgreSQL `PGSSLMODE`, mapped the supportable MySQL/MariaDB CLI modes and rejected unverifiable CA modes, hashed the resolved native executable, added bounded source evidence to the signed manifest, and downgraded non-transactional MySQL/MariaDB backups to `crash_consistent`.
- VERIFY: PostgreSQL 8/8, MySQL/MariaDB 33 unit tests plus integrations, focused service Clippy, and the recoverable SQLite service E2E passed.
- NEXT: run all local, browser, and live-engine gates.

#### Cycle 12 - full verification and live engine gate

- ISSUE: cross-crate fixtures and browser assertions still encoded pre-change `SpoolCommit` and single-Target assumptions.
- FIND: full service tests found a stale unimplemented-`backup.enqueue` assertion; Playwright found Target-ambiguous labels; the first G2 run found two missing `ciphertext_blake3` test fixtures.
- FIX: updated only those stale test contracts, preserving the new durable multi-Target behavior and ciphertext evidence requirement.
- VERIFY: `cargo fmt --all -- --check`, `git diff --check`, workspace Clippy with `-D warnings`, and `cargo test --workspace` all exited 0. Console Vitest passed 23/23, production build passed, and Chromium 1024/1280/1600 passed 30/30 with axe checks. Docker G2 passed PostgreSQL 18.4, MySQL 8.4.11, and MariaDB 11.4.12 with source/restore oracle hashes equal and four contract checks passed.
- RESULT: DBX-R01 through DBX-R06 are proved. Browser data remains explicitly labeled fixture data; live engine evidence is stored under `artifacts/g2/`.

---

## Active Run: Connected Backup Platform v2

### Run Metadata

- Started: 2026-07-29T18:23:01.2655766+09:00
- Project: `D:\git\dbbackupsystem`
- Branch/HEAD: `main` / `bd6a971`
- Engine: native goal + direct Chronos loop
- Scope: `docs/plan/connected-backup-platform-v2/sections/section-01`부터 `section-10`까지의 실행 가능한 구현과 검증
- Constraints: 기존 dirty worktree 보존, 자동 commit 금지, 한 사이클 한 이슈, production authority cutover는 G1 evidence 이후
- Gate: section별 focused test 뒤 `cargo fmt --all -- --check`, `git diff --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `npm run build --prefix apps/console`, PowerShell truth/integration gate, 가능한 managed fixture
- Promise: 10개 section 요구사항이 모두 현재 revision의 증거로 `proved`이거나, 실행 가능한 범위를 소진한 외부 접근·제품 결정 항목만 Owner Decision Brief와 함께 주차됨

### Completion Contract

| Requirement | Pass evidence | State |
|---|---|---|
| V2-R01. G0 truth baseline과 release gate가 실제 capability, public copy, ignored test, direct execution 경계를 정직하게 판정한다. | section-01 G0-A 명령 전체와 false-green 회귀 테스트 | proved |
| V2-R02. Connection/Target/Plan/Occurrence/Work/Recovery Point 도메인과 53-table Operational Store가 불변식·migration·shadow import를 보장한다. | section-02 schema/integrity/golden/power-loss/online-backup gate | proved |
| V2-R03. singleton Windows service, SID 기반 local IPC, replay/token 경계, Secret Store custody가 구현된다. | section-03 S03 current-build evidence 14/14, offline rollback fault suite, managed Windows Task/pipe/DPAPI gate | proved |
| V2-R04. durable dispatcher가 occurrence 멱등성, queue, lease/fencing, resource admission, process supervision, crash reconciliation을 보장한다. | section-04 G1 fault matrix와 동일 revision/build evidence | proved |
| V2-R05. signed multi-part Recovery Point, Replica durability/compliance, repair, retention이 실제 artifact 사실과 일치한다. | section-05 S05 evidence 13/13, crash/rebuild/reader/retention/L1 gate | proved |
| V2-R06. PostgreSQL/MySQL/MariaDB backup·verify·restore와 알림 delivery가 실제 fixture에서 인증된다. | G2 3/3 engine + 4/4 contract checks, build `bd707b35…393a5`, actual content/webhook fixtures | proved |
| V2-R07. 연결 중심 제품 IA와 Connection/Database/Plan/Activity UI가 IPC 계약, 접근성, 반응형 기준을 충족한다. | 9-route Tauri UI, durable Connection/Destination Work, Vitest 6/6, Playwright 15/15, production build, service/catalog regression | proved |
| V2-R08. Snapshot inventory와 안전한 restore/preflight/overwrite/repair/drill UX가 오작동 없이 동작한다. | Snapshot/diagnostics inventory, exact L1/L2/L3, authoritative preflight, atomic create-new/overwrite/repair/drill, catalog preview, Vitest 8/8, Playwright 24/24, production build | proved |
| V2-R09. SQL Server BACPAC과 SQLite가 공통 provider 계약과 정직한 Engine Matrix 상태로 제공된다. | SQLite G4 9-capability 실제 fixture와 Engine Matrix는 proved; SQL Server는 후보 계약 proved, 실제 BACPAC 왕복은 외부 fixture 부재로 PARKED | parked |
| V2-R10. 확장 경계, 설치·업데이트·관측·지원 번들·롤백과 단계적 출시 gate가 완성된다. | 14-stage update/rollback, Windows installer lifecycle, diagnostics/support/telemetry archive, exact MSRV·SBOM·audit, 최종 managed G0–G5 15/15 checks | proved |

### Active Cycle Log

#### Phase 0 — 2026-07-29

- READ: `codemap/index.md`와 라우터, README, 최신 handoff, 기존 Chronos 로그, v2 planning index와 section-01 전체를 확인했다.
- BASELINE: 이전 원격 reader 복구 범위 R1-R5는 기존 로그에서 모두 proved이며 해당 dirty worktree를 보존한다.
- ENVIRONMENT: Rust 1.97.0, Cargo 1.97.0, Node 24.18.0, npm 11.16.0, Windows PowerShell 5.1.
- FIND: section-01의 첫 Critical/High false-green인 L2/L3 요청의 조용한 L1 하향을 RED 테스트로 고정한다.

#### Cycle 1 — L2/L3 false-green 차단

- ISSUE: `verify --level 2/3`이 실제로 L1을 실행한 뒤 성공할 수 있어 요청한 검증 수준을 거짓으로 충족했다.
- RED: `l2_and_l3_are_rejected_instead_of_falling_back_to_l1`이 guard와 stable error code 부재로 컴파일 실패했다.
- FIX: command 진입 직후 `validate_requested_level`을 실행하고 L2/L3를 `VERIFY_LEVEL_NOT_EXECUTED`로 거부했다. config, keystore, manifest, artifact와 Tokio runtime보다 먼저 실행된다.
- VERIFY: focused test 1 passed, 실제 missing-config CLI probe가 exit 1과 stable code를 반환하고 config 오류를 내지 않음, `cargo test -p dbbackup-cli` 42 passed/0 failed/2 ignored, package clippy `-D warnings` 통과.
- NEXT: 과거 성공 backup과 별도로 최신 failed/running/partial Attempt를 반환하는 catalog query를 TDD로 추가한다.

#### Cycle 2 — 최신 Attempt truth projection

- ISSUE: catalog read API가 backup 성공 이력만 제공해 더 최근의 failed/running Attempt를 UI가 별도로 읽을 수 없었다.
- RED: `truth_projection` 통합 테스트가 `latest_run_per_job` 공개 API 부재로 컴파일 실패했다.
- FIX: `LatestRunRecord`와 job별 최신 `runs` 조회를 추가하고 crate root에서 export했다. backup row와 결합하거나 성공 상태를 합성하지 않는다.
- VERIFY: focused integration test 1 passed, `cargo test -p dbbackup-catalog` 7 passed/0 failed, package clippy `-D warnings` 통과.
- NEXT: 같은 timestamp의 Attempt를 `rowid DESC`로 결정하는 edge contract를 RED로 고정한다.

#### Cycle 3 — 최신 Attempt 동률 결정성

- ISSUE: 두 Attempt의 `started_at`이 같으면 최신 행이 둘로 반환돼 UI 상태가 비결정적일 수 있었다.
- RED: 같은 timestamp의 running 뒤 failed 행을 넣은 테스트가 1건 대신 2건을 반환해 실패했다.
- FIX: `started_at DESC, rowid DESC` 의미가 되도록 correlated query에 rowid tie-break를 추가했다.
- VERIFY: `truth_projection` 2 passed/0 failed, package clippy `-D warnings` 통과.
- NEXT: Plan의 Destination을 artifact 성공으로 합성하지 않고 실제 `artifacts` row만 반환하는 query를 TDD로 추가한다.

#### Cycle 4 — 실제 Artifact row projection

- ISSUE: UI가 config의 Destination 목록을 성공 artifact처럼 합성할 수 있었고 catalog에는 실제 row만 읽는 API가 없었다.
- RED: 한 Destination artifact row만 넣은 통합 테스트가 `artifacts_for_backup` 공개 API 부재로 컴파일 실패했다.
- FIX: nullable immutability/retention을 보존하는 `ArtifactRecord`와 backup별 실제 row query를 추가했다.
- VERIFY: focused integration test 1 passed, package clippy `-D warnings` 통과.
- NEXT: Tauri summary를 최신 Attempt와 실제 artifact query에 연결해 config 기반 합성을 제거한다.

#### Cycle 5 — Tauri 최신 Attempt 우선 health

- ISSUE: Tauri health가 마지막 성공 backup만 평가해 더 최근 failed/partial/running Attempt를 숨겼다.
- RED: 과거 signed/L1-pass backup과 최신 failed Attempt를 함께 준 테스트가 두 번째 인자를 받는 projection 경계 부재로 컴파일 실패했다.
- FIX: dashboard가 `latest_run_per_job`을 별도 조회하고 `lastAttemptStatus`/`lastAttemptStartedAt`을 직렬화하며, health는 failed/partial과 running/coalesced를 backup evidence보다 먼저 평가한다.
- VERIFY: focused test 1 passed, Tauri 11 passed/0 failed, package clippy `-D warnings`, Rust camelCase shape와 TypeScript/mocks 교차비교, frontend production build 통과.
- NEXT: Tauri artifact 목록을 config Destination 합성 대신 `artifacts_for_backup`의 실제 row로 전환한다.

#### Cycle 6 — Tauri 실제 Artifact evidence

- ISSUE: Tauri가 job의 config Destination을 `uploaded`와 불변 보관으로 합성해 catalog 증거가 없는 보호 상태를 표시했다.
- RED: nullable immutability를 가진 실제 `ArtifactRecord`를 UI view로 바꾸는 테스트가 mapping 함수 부재로 컴파일 실패했다.
- FIX: dashboard가 backup별 `artifacts_for_backup`을 읽어 실제 row만 직렬화하고, NULL immutability는 `unknown`으로 유지한다. config 합성 함수와 capability 추론을 제거했다.
- UI: DESIGN.md의 기존 warning 토큰으로 `불변성 미확인` 배지를 추가하고 `unknown`은 불변 보관 수에 포함하지 않았다. 목 artifact도 actual-row 의미의 `committed`로 맞췄다.
- VERIFY: focused test 1 passed, Tauri 12 passed/0 failed, package clippy `-D warnings`, frontend production build 통과.
- NEXT: claims matrix와 G1 evidence requirement 정본을 작성하고 schema/promotion guard의 RED fixture를 만든다.

#### Cycle 7 — G0 claims와 promotion truth checker

- ISSUE: 공개 capability와 G1 cutover prerequisite의 machine-readable 정본 및 자동 promotion guard가 없었다.
- RED: PowerShell fixture suite의 valid case가 `scripts/check-g0-truth.ps1` 부재로 실패했다. 첫 실행의 BOM 없는 한글 parse 오류는 새 스크립트 두 개를 UTF-8 BOM으로 고정해 기능 RED와 분리했다.
- FIX: capability 24개 claims, G1 evidence ID 9개, 사람용 truth projection, direct execution inventory, cutover recovery matrix를 추가했다. checker는 schema/enum/중복/promotion, baseline drift, ADR 승인, inventory, 과대 문구, G0 이전 신규 engine source, optional G1 revision/build 일치를 검증한다.
- VERIFY: fixture 7개(valid, duplicate, enum, unsupported promotion, ADR missing, banned copy, stale G1) 통과. 실제 repository 검사는 나머지 3개 banned copy를 정확히 탐지하고 다른 guard는 통과했다.
- NEXT: Dashboard와 Restore의 세 과대 문구를 현재 evidence 범위로 낮춘다.

#### Cycle 8 — 공개 문구 truth boundary

- ISSUE: Dashboard, Restore, README와 SECURITY가 current evidence 없이 안전·오프사이트 보관·실제 복원·완료를 종합 보장했다.
- RED: actual `check-g0-truth.ps1`이 Dashboard 2건과 Restore 1건의 banned claim으로 실패했다.
- FIX: Dashboard는 최근 Attempt, 서명, L1, 실제 artifact row를 개별 증적으로 설명한다. Restore 성공 badge는 종합 검증을 주장하지 않는다. README/SECURITY는 자동 verification/retention/DB-target restore/installer의 현재 미배선과 `not_run` 의미를 명시하고 claims JSON을 정본으로 연결했다.
- VERIFY: fixture suite 7 passed, actual truth checker exit 0 (`legacy_direct`, `rehearsal_only`, `durableGate=false`), frontend production build 통과.
- NEXT: Restore UI와 Tauri command를 decrypt-only로 강제해 실제 DB restore action을 비활성화한다.

#### Cycle 9 — Restore decrypt-only 실행 경계

- ISSUE: Restore UI가 DB-target/sandbox 실행처럼 보이는 요청을 만들 수 있었고 출력 경로가 없어도 sidecar 실행으로 진입할 수 있었다.
- RED: `restore_args` 계약 테스트가 해당 순수 검증 함수 부재로 컴파일 실패했다.
- FIX: Tauri에서 `decryptOnly=true`와 명시적 `to` 경로만 허용하고 각각 `RESTORE_DECRYPT_ONLY_REQUIRED`, `RESTORE_OUTPUT_REQUIRED`로 거부하도록 고정했다. TypeScript 요청 타입도 `decryptOnly: true` 리터럴로 좁히고 UI를 평문 덤프 생성 흐름으로 한정했다.
- VERIFY: Tauri 13 passed/0 failed, package clippy `-D warnings`, frontend production build, actual G0 truth checker 모두 통과했다.
- NEXT: Jobs 화면에서 L2/L3와 retention을 실제 시행 기능처럼 표현하는 정책 문구를 false-green guard와 함께 교정한다.

#### Cycle 10 — verification/retention configured-only 표시

- ISSUE: Jobs 화면이 L2/L3를 선택 가능한 실제 검증 레벨로 제시하고 저장된 verification/retention 값을 자동 시행 증적처럼 보이게 했다.
- RED: `missing-policy-copy` fixture가 오해 가능한 Jobs 문구를 넣어도 truth checker가 성공해 예상 실패와 달랐고, guard 추가 뒤 실제 저장소가 요구 문구 부재로 실패했다.
- FIX: truth checker가 Jobs의 configured-only 경계를 강제하도록 확장했다. 화면에는 “정책 저장됨, 자동 시행 미배선”을 표시하고 L2/L3 선택을 비활성화했으며 L1도 수동 실행만 지원함을 명시했다.
- VERIFY: truth checker fixture 8 passed, actual truth checker exit 0, frontend production build 통과.
- NEXT: Schedule 화면을 legacy job별 Task Scheduler 경계로 표시하고 durable queue/RPO 보장 오해를 제거한다.

#### Cycle 11 — legacy schedule 실행 권한 표시

- ISSUE: Schedule 화면이 `StartWhenAvailable`을 놓친 백업을 따라잡는 경로로 표현하고 generic “완료” 및 알림 발송 문구로 durable/RPO 보장을 암시했다.
- RED: `missing-schedule-copy` fixture가 “RPO 보장 완료”만 포함해도 truth checker가 성공해 예상 실패와 달랐다.
- FIX: checker가 `legacy job별 Task Scheduler` 경계 문구를 강제한다. 화면은 durable queue/occurrence ledger/RPO 보장이 없음을 명시하고 등록 성공을 Windows Task 등록 성공으로 한정했으며 알림 전송·retry 증적 부재를 밝혔다.
- VERIFY: truth checker fixture 9 passed, actual truth checker exit 0, frontend production build 통과.
- NEXT: Connection/Destination 등록 성공 문구를 config/Secret 저장 성공으로 한정하고 live probe 성공과 분리한다.

#### Cycle 12 — 등록 성공과 live probe 분리

- ISSUE: Connection/Destination 등록 완료 문구가 저장 성공과 live 연결 점검 성공을 명확히 구분하지 않았다.
- RED: `missing-registration-copy` fixture가 Destination을 “연결 성공”으로만 표현해도 truth checker가 성공해 예상 실패와 달랐다.
- FIX: checker가 두 등록 화면의 live probe 미실행 고지를 강제한다. 성공 메시지는 설정과 암호화된 Secret/자격증명 저장 성공만 알리고 “live 연결 점검은 실행하지 않았습니다.”를 명시한다.
- VERIFY: truth checker fixture 10 passed, actual truth checker exit 0, frontend production build 통과.
- NEXT: 실패 뒤에도 나머지 명령을 수집하고 ignored test를 `not_run`으로 분리하는 G0 evidence collector를 TDD로 구현한다.

#### Cycle 13 — G0 evidence collector

- ISSUE: G0 명령별 revision/runner/timing/exit 증거와 ignored `not_run` 목록을 생성하는 수집기가 없었고 중간 실패 뒤 진단을 보존할 경로도 없었다.
- RED: collector fixture가 `collect-g0-evidence.ps1` 부재로 evidence 및 ignored 목록을 만들지 못해 실패했다.
- FIX: 여섯 기본 gate를 각각 외부 process로 실행하고 실패 후에도 계속하는 collector를 추가했다. JSON에는 command metadata와 요약만 저장하며 출력 원문·환경변수 값은 제외한다. worktree는 상태/경로만 기록하고 ignored test는 별도 파일과 `not_run` 배열로 정규화한다.
- VERIFY: 실패 지속성·최종 exit 1·출력 비밀 비저장 fixture와 정상/ignored 분리 fixture 2개 통과.
- NEXT: 생성 artifact를 worktree에서 격리하고 G0 evidence JSON 자체의 revision/command/not_run 계약을 truth checker에 연결한다.

#### Cycle 14 — G0 evidence 신뢰 경계

- ISSUE: 생성된 evidence JSON이 현재 revision의 정확한 여섯 gate와 ignored `not_run` 계약을 지키는지 checker가 판정하지 않았고 로컬 artifact 경로도 추적 대상이었다.
- RED: 유효 G0 evidence fixture가 새 `-EvidencePath` 계약 부재로 실패했다.
- FIX: checker가 schema/gate, HEAD revision, runner/tool metadata, 정확한 command 집합, command별 exit/timing/revision/OS, 출력 원문 금지 속성, ignored 고유성 및 `not_run` 상태를 검증하도록 확장했다. `artifacts/g0/`는 root ignore에 추가했다.
- VERIFY: truth checker fixture 13 passed(유효 evidence, 실패 command, ignored false-pass 포함), actual repository truth checker exit 0이며 evidence 미제공 상태는 `evidenceGate=false`로 정직하게 표시했다.
- NEXT: Windows CI에서 frontend, collector, evidence-aware truth checker를 실행하고 실패 여부와 무관하게 G0 artifact를 업로드한다.

#### Cycle 15 — Windows CI G0 배선

- ISSUE: Windows CI가 Rust 기본 gate만 실행해 frontend, ignored 분리 evidence, truth promotion 판정과 실패 시 진단 artifact를 남기지 않았다.
- RED: `missing-ci-evidence-wiring` fixture가 cargo test만 있는 workflow를 허용해 예상 실패와 달랐고, guard 추가 뒤 실제 CI에서 일곱 필수 배선이 누락된 것으로 검출됐다.
- FIX: Node 설정과 두 PowerShell 계약 테스트, G0 collector, evidence-aware truth checker를 Windows job에 연결했다. collector/checker/upload는 `always()`로 진단을 보존하며 `actions/upload-artifact@v4`가 `artifacts/g0`를 업로드한다. 기존 supply-chain/secret-scan job은 유지했다.
- VERIFY: truth checker fixture 14 passed, actual repository truth checker exit 0.
- NEXT: 기본 command plan으로 전체 G0 evidence를 실제 수집하고 생성 JSON을 evidence-aware checker로 검증한다.

#### Cycle 16 — 실제 G0 evidence 실행

- ISSUE: 구현된 collector/checker가 실제 dirty worktree와 전체 workspace에서 성공한다는 외부 증거가 아직 없었다.
- RED: 첫 전체 수집은 나머지 다섯 gate를 끝까지 통과·기록했지만 새 Rust 코드 세 곳의 rustfmt 차이로 `cargo-fmt`만 exit 1이었고 evidence-aware checker가 정확히 거부했다.
- FIX: 해당 Rust 파일에 `cargo fmt --all`의 기계적 포맷만 적용했다.
- VERIFY: 재수집 exit 0, 6/6 commands pass, failureCount 0, ignored 10건은 모두 `not_run`, source revision `bd6a97145db03fe2ccb2fa46e453f84f1e1ca575`. evidence-aware truth checker는 `evidenceGate=true`, `durableGate=false`, `legacy_direct`, `rehearsal_only`로 통과했다. collector fixture 2, truth fixture 14, `git diff --check`, artifact ignore 확인도 통과했다.
- RESULT: V2-R01 proved. G0는 후속 구현 시작을 허용하지만 production execution cutover는 아직 금지된다.
- NEXT: section-02 전체를 읽고 53-table Operational Store의 기존 구현/계획 중복을 대조한 뒤 첫 migration invariant를 RED로 고정한다.

#### Cycle 17 — canonical Operational Store migration

- ISSUE: 1,438-line planning DDL은 53개 STRICT table을 정의했지만 runtime에 embedded되어 원자 적용·checksum journal 검증되는 경로가 없었다.
- RED: `schema_v2` integration test가 `apply_operational_store_v2`와 checksum API 부재로 컴파일 실패했다.
- FIX: canonical `schema-v2.sql`을 compile-time embed하고 마지막 commit 직전에 BLAKE3 journal row와 `user_version=2`를 같은 `BEGIN IMMEDIATE` transaction에 기록하는 migration runner를 추가했다. 적용된 checksum/user_version drift는 stable migration error로 write 시작을 거부한다.
- VERIFY: fresh bundled SQLite에서 53/53 tables STRICT, `integrity_check=ok`, FK violation 0, journal 1건, FULL synchronous, FK ON, busy timeout 5000 확인. catalog 10 passed/0 failed, package clippy `-D warnings`, fmt 통과.
- NEXT: migration 재실행 멱등성과 checksum/user_version drift, DDL 중간 실패 rollback을 fault test로 고정한다.

#### Cycle 18 — migration drift와 power-loss 원자성

- ISSUE: migration 재실행, journal/user_version 변조, DDL 중간 실패가 write mode를 어떤 stable code로 막고 어느 상태로 rollback하는지 회귀 계약이 없었다.
- RED: drift/fault integration test가 `MigrationError::code()` 부재로 컴파일 실패했다.
- FIX: 모든 migration 오류에 raw SQLite 문구와 분리된 stable application code를 부여했다. 기존 atomic runner의 rollback 경계를 checksum/version drift와 deterministic DDL collision로 검증 가능하게 했다.
- VERIFY: 첫 적용/재실행은 applied true/false로 멱등, checksum·user_version drift는 각각 stable code로 거부, 중간 DDL 실패 뒤 신규 table/journal/user_version은 전부 rollback되고 충돌 전 table만 보존됨. focused 3 passed, clippy `-D warnings`, fmt 통과.
- NEXT: `dbbackup-domain` crate를 추가하고 typed ID, serialized enum, Plan publish/occurrence/state transition pure contract를 RED부터 구현한다.

#### Cycle 19 — typed domain IDs

- ISSUE: v2 계층 사이에서 Connection/Target/Revision/Occurrence/Run/Work/Recovery 식별자를 raw `String`으로 섞지 않게 할 독립 domain crate와 typed ID가 없었다.
- RED: `typed_ids` integration test가 domain ID와 `DomainError` export 부재로 컴파일 실패했다.
- FIX: DB/IPC/Tauri를 의존하지 않는 `dbbackup-domain` crate를 workspace에 추가하고 20개 UUID newtype을 만들었다. v4 생성, v5 canonical-name 결정 ID, lowercase canonical boundary parse, serde string 계약과 stable error code를 제공한다.
- VERIFY: 생성/serde, deterministic v5, malformed/noncanonical rejection 3 passed. domain all-target clippy `-D warnings`, fmt 통과.
- NEXT: DDL과 동일한 serialized enum 집합 및 Work Item/Attempt/Occurrence/Run/Recovery Point pure transition table을 golden test로 고정한다.

#### Cycle 20 — domain enum과 pure state machine

- ISSUE: queue와 catalog 상태를 임의 문자열/직접 UPDATE로 다루면 DDL enum drift, terminal replay, partial-success 소실을 application에서 막을 수 없었다.
- RED: golden/transition test가 12개 enum과 5개 transition API 부재로 컴파일 실패했다.
- FIX: DDL과 같은 snake_case enum 정본 및 Work Item, Attempt, Occurrence, Backup Run, Recovery Point의 pure `(current, command)` transition table을 추가했다. 불허 전이는 `DOMAIN_STATE_TRANSITION_INVALID`로 통일했다.
- VERIFY: 12개 DDL CHECK 값의 순서·집합 golden 일치, 정상 queue/commit, abandoned/terminal replay 거부, occurrence/run partial success, complete→quarantine 경로 3 passed. domain all-target clippy `-D warnings`, fmt 통과.
- NEXT: immutable Plan Revision publish validation과 scheduled/manual Occurrence identity builder를 typed entity로 구현한다.

#### Cycle 21 — immutable Plan Revision과 Occurrence identity

- ISSUE: application에서 publish membership을 검증하거나 scheduled/catch-up/manual 실행 의도를 DDL partial unique index와 같은 규칙으로 만들 typed entity가 없었다.
- RED: plan/occurrence integration test가 `PlanRevisionDraft`와 `OccurrenceIdentity` 부재로 컴파일 실패했다.
- FIX: draft→published 소유권 전환으로 publish 뒤 mutation API를 제거하고 Target/Destination/required membership 및 중복을 검증했다. scheduled와 catch-up은 `(revision, UTC time)`을 공유하고 manual은 canonical request key로 식별하며 둘 다 deterministic v5 occurrence ID를 만든다.
- VERIFY: publish 오류 3종, immutable snapshot/membership, duplicate 거부, scheduled/catch-up 동일성, manual key/origin 거부 3 passed. domain 전체 9 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: Operational Store writer/read connection 경계를 v2 migration에 연결하고 WAL/FULL/read-only/transaction-owner 계약을 검증한다.

#### Cycle 22 — Operational Store connection ownership

- ISSUE: 기존 catalog connection은 projection용 schema와 `synchronous=NORMAL`만 제공해 v2 Operational Store의 WAL/FULL migration owner와 query-only reader 경계가 없었다.
- RED: file-backed connection test가 `OperationalStore` API 부재로 컴파일 실패했다.
- FIX: 전용 OS thread가 유일한 read-write connection과 migration을 소유하는 `OperationalStore`를 추가했다. reader는 별도 read-only/no-mutex connection에 FK, 5초 busy timeout, query-only를 강제하며 raw writer connection을 노출하지 않는다.
- VERIFY: writer WAL/FULL/FK/busy timeout/query_only=0/user_version=2/53 tables, reader WAL/FK/busy timeout/query_only=1/SQLite readonly를 확인. catalog 전체 14 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: configuration repository에서 Connection/Target/Destination/Plan draft+publish transaction을 구현하고 DB trigger와 application invariant를 함께 검증한다.

#### Cycle 23 — Configuration Repository publish boundary

- ISSUE: v2 Connection/Target/Destination/Plan을 writer owner를 우회하지 않고 저장하고, draft membership과 publish 불변식을 application 및 SQLite 양쪽에서 지키는 repository 경계가 없었다.
- RED: configuration integration test가 `ConfigurationRepository`와 입력 DTO, publish/query API 부재로 컴파일 실패했다.
- FIX: writer thread에 typed write job을 추가하고 각 설정 변경을 transaction으로 실행하는 repository를 구현했다. publish는 `BEGIN IMMEDIATE`에서 required Target/Destination, best-effort 단독 구성, draft 여부를 검증한 뒤 revision publish와 plan current pointer 변경을 원자화한다. published revision mutation에는 stable domain code를 추가했다.
- VERIFY: 정상 Connection→2 Targets→Destination→Plan publish/query와 application invalid-membership rejection, SQLite trigger의 독립 rejection, published revision/membership mutation rejection 2 passed. catalog 전체 21 passed, catalog/domain all-target clippy `-D warnings`, fmt 통과.
- NEXT: Runtime Repository의 Occurrence 멱등 upsert, Backup Run 생성, Work Item subject XOR와 claim/lease/fencing CAS를 DB 불변식과 함께 TDD로 구현한다.

#### Cycle 24 — Runtime Repository occurrence와 fenced claim

- ISSUE: scheduled/catch-up/manual 실행 의도를 멱등 생성하고 Backup Run/Work Item을 원자 enqueue하며, retry마다 증가하는 fence로 stale worker mutation을 막는 runtime repository가 없었다.
- RED: runtime integration test가 `RuntimeRepository`, Occurrence/Backup Work/Claim DTO와 store 진입점 부재로 컴파일 실패했다.
- FIX: published revision만 받는 identity upsert, occurrence 상태 전이를 포함한 Backup Run+단일-subject Work Item transaction, `BEGIN IMMEDIATE` 기반 due claim CAS, Attempt 생성, heartbeat fence CAS, pure domain transition을 거치는 retry requeue를 구현했다. repository 오류를 공통 모듈로 분리해 raw SQLite 문구 대신 stable code를 유지했다.
- VERIFY: scheduled/catch-up 1건 및 manual replay 1건의 멱등성, 첫 claim `(attempt=1,fence=1)`, retry claim `(2,2)`, 이전 fence heartbeat 거부, 현재 fence 허용, DB의 다중 subject CHECK 독립 거부 2 passed. catalog 전체 23 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: IPC request ledger와 destructive confirmation의 동일-key replay/conflict, one-time consume, overwrite Restore transaction을 repository+trigger 불변식으로 구현한다.

#### Cycle 25 — IPC ledger와 destructive confirmation consume

- ISSUE: SID/command/idempotency/payload 결합을 기록하고 파괴적 Restore 확인을 정확히 한 번 소비하면서 Run/Work 생성까지 원자화하는 repository가 없었다.
- RED: IPC/confirmation integration test가 `IpcRepository`, request/confirmation/overwrite DTO와 store 진입점 부재로 컴파일 실패했다.
- FIX: 64자리 lowercase digest와 bounded metadata만 받는 경계를 추가했다. 동일 key·동일 payload는 최초 Work Item을 반환하고, 다른 payload는 `IPC_IDEMPOTENCY_CONFLICT`로 거부한다. ledger insert/compare, token digest·SID·action·resource·payload·expiry CAS, overwrite Restore Run과 Work Item 생성, result 기록을 하나의 `BEGIN IMMEDIATE` transaction으로 묶었다.
- SECURITY: 보안 리뷰 지침의 tampering/repudiation/information-disclosure 관점에 따라 raw SID와 raw token을 API/DB에 받지 않고 파라미터화 SQL, generic stable error, terminal binding trigger를 유지했다.
- VERIFY: 최초 consume, 동일 payload replay, payload conflict, 다른 key의 consumed-token 재사용 및 rollback, overwrite confirmation CHECK, consumed→issued 역전과 binding 변경 trigger 거부 2 passed. request/restore는 실패 시 추가 row 없이 각각 1건이며 catalog 전체 25 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: Recovery Point completeness, L3 restore evidence, Stored Object composite FK, Snapshot 네 축을 Catalog Projection Repository transaction과 query로 검증한다.

#### Cycle 26 — Recovery Point completeness와 Snapshot 네 축

- ISSUE: 서명 상태와 선언 count만으로 complete를 기록하지 않고 실제 Part/Object 관계를 확인하며 Snapshot의 완전성·내구성·준수·복원 가능성을 독립 투영하는 repository가 없었다.
- RED: catalog projection integration test가 complete ingestion DTO, 네 축 query와 store 진입점 부재로 컴파일 실패했다.
- FIX: verified input을 먼저 `incomplete` Recovery Point로 넣고 required Part, committed Replica, present Object를 같은 transaction에서 기록한 뒤 실제 distinct required/present Part 수를 재계산한다. 모두 존재할 때만 `complete`로 CAS하고 네 개 typed enum을 Snapshot row에 각각 저장한다.
- VERIFY: 실제 required/present count `(1,1)`과 `complete/degraded/non_compliant/conditional` 네 축 독립 round-trip, required Part의 present Object 누락 시 전체 rollback, DB의 invalid-signature/zero-part complete CHECK, cross-Recovery Part/Replica composite FK, Restore evidence 없는 L3 CHECK 거부 2 passed. catalog 전체 27 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: Activity/Attention append와 Notification outbox dedupe/delivery claim을 bounded repository 경계로 추가해 남은 표별 소유권을 채운다.

#### Cycle 27 — bounded Activity lane와 Attention lifecycle

- ISSUE: Activity append가 control/lease write와 같은 무제한 FIFO를 쓰면 burst 시 heartbeat가 굶을 수 있고, 동일 시각 cursor 및 active Attention dedupe 경계도 없었다.
- RED: activity integration test가 `ActivityRepository`, bounded batch DTO, deterministic recent query와 Attention lifecycle API 부재로 컴파일 실패했다.
- FIX: writer actor를 high-priority unbounded control lane과 capacity 32의 low-priority Activity lane으로 분리하고 biased selection을 적용했다. Activity는 최대 128건 transaction batch, `(occurred_at_ms DESC, seq DESC)` 조회를 사용한다. Attention은 active fingerprint만 upsert/count하며 acknowledge와 resolve를 CAS한다.
- VERIFY: 동일 timestamp 3건의 증가 seq와 역순 cursor, active attention 2회 dedupe/count, acknowledge→resolve, terminal 뒤 새 Attention 생성 2 passed. 내부 actor test에서 이미 대기 중인 low write보다 후발 high write가 먼저 실행됨을 확인했다. catalog 전체 30 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: Notification Channel/Rule/Outbox의 transaction dedupe와 delivery lease/fence/retry를 별도 repository로 구현한다.

#### Cycle 28 — Notification outbox dedupe와 delivery fence

- ISSUE: 알림 경로에 Channel probe 상태, Rule, transactional outbox dedupe와 재시도별 stale delivery 차단을 소유하는 repository가 없었다.
- RED: notification integration test가 Channel/Rule/Outbox DTO, delivery claim/fence API와 store 진입점 부재로 컴파일 실패했다.
- FIX: Channel은 draft로 저장하고 명시적 probe-pass 뒤에만 ready가 되며, ready Channel에만 enabled Rule을 만든다. Outbox payload는 canonical JSON으로 저장해 `(rule,dedupe key)` replay를 판정한다. due claim은 `BEGIN IMMEDIATE` CAS로 attempt/fence를 증가시키고 delivery attempt를 생성하며 retry/delivered mutation은 owner·boot·attempt·fence·unexpired lease를 모두 검사한다.
- VERIFY: 동일 payload는 최초 outbox 반환, 다른 payload는 `NOTIFICATION_DEDUPLICATION_CONFLICT`, 첫 delivery `(1,1)` 재시도 뒤 `(2,2)`, 이전 fence 완료 거부와 현재 fence 완료, attempt 2건/최종 delivered 확인 2 passed. catalog 전체 32 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: Native Tool inventory와 Engine Certification을 실제 capability/evidence 분리 및 만료 판정이 가능한 Certification Repository로 구현한다.

#### Cycle 29 — Native Tool inventory와 effective certification

- ISSUE: 실행 파일 발견만으로 엔진 backup/restore 지원을 주장하지 않게 하고, binary 변경·후보 evidence·capability별 제한·만료를 분리하는 repository가 없었다.
- RED: certification integration test가 Tool observation, Certification/Capability DTO, effective query와 store 진입점 부재로 컴파일 실패했다.
- FIX: active path별 Tool 관찰은 SHA-256 변경 시 signature/trust/publisher 검증을 초기화한다. Certification은 candidate와 capability evidence를 transaction으로 저장하고 명시적 certify 시각/만료 뒤에만 effective가 된다. 조회는 `passed|limited`, certified, `certified_at <= now < expires_at`을 모두 만족한 capability만 반환한다.
- VERIFY: Tool 존재와 candidate가 support를 만들지 않음, backup passed/L2 limited/restore missing 독립 판정, expiry exclusive, 동일 path binary 변경 뒤 trusted/valid→unreviewed/not_checked 초기화 2 passed. catalog 전체 34 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: live WAL Operational Store의 SQLite online backup, 별도 경로 restore rehearsal, integrity/FK/schema 검사를 Control Store repository로 구현한다.

#### Cycle 30 — live WAL online backup과 restore rehearsal

- ISSUE: 실행 중 WAL DB를 파일 복사하지 않고 백업하며 별도 경로 복원본의 integrity/FK/schema까지 검사하고 이력을 남기는 control-store 경계가 없었다.
- RED: online backup integration test가 `ControlStoreRepository`, backup/rehearsal DTO와 store 진입점 부재로 컴파일 실패했다.
- FIX: pinned rusqlite 0.31의 `backup` feature와 `Backup::run_to_completion`을 사용해 read-only source connection에서 create-new destination으로 page backup한다. backup과 별도 rehearsal DB 각각 `integrity_check=ok`, FK violation 0, user_version 2, 53 tables, migration checksum을 검사한다. 장시간 IO 중 writer transaction을 열지 않고 시작/성공·실패 row만 high lane에서 갱신한다.
- VERIFY: 1,000건 seed와 동시 Activity writer가 있는 WAL store의 online backup/rehearsal, BLAKE3 checksum, schema 2/53, succeeded row 통과. rehearsal parent IO 실패는 failed row와 성공 0건을 남기고 source writer/reader가 정상임을 확인했다. focused 2 passed, catalog 전체 36 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: Catalog Projection 전량 삭제/rebuild가 Plan/queue/Run 등 Control State hash와 행 수를 바꾸지 않는 FK 방향 및 repository isolation test를 구현한다.

#### Cycle 31 — Catalog Projection rebuild isolation

- ISSUE: rebuildable `catalog_*` 8개 표를 비운 뒤 재투영해도 Plan/Occurrence/Run/Work/Attempt가 변하지 않고, Control→Catalog FK가 0임을 자동 증명하는 경계가 없었다.
- RED: projection integration test가 전체 clear, Control State digest probe와 FK 방향 검사 API 부재로 컴파일 실패했다.
- FIX: manifest ingest부터 Recovery Point까지 8개 rebuildable 표만 child-first transaction으로 비우는 API를 추가했다. Control probe는 Connection/Target/Plan/Revision/Occurrence/Backup Run/Work/Attempt/Restore/Verification의 정렬된 행을 length-framed BLAKE3로 계산하고 핵심 행 수를 함께 반환한다. 모든 비-projection table의 FK target도 introspection한다.
- VERIFY: published Plan 1, Occurrence 1, Backup Run 1, leased Work/Attempt 각 1과 complete RP를 만든 뒤 clear 중·동일 RP 재ingest 후 Control probe가 byte-identical했다. Control→Catalog FK 0, catalog 0→동일 Snapshot 복구 확인. focused 3 passed, catalog 전체 37 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: v1 config를 canonical intermediate model과 stable UUID로 shadow DB에 두 번 import하고 production file 불변·secret canary·diff 동일성을 검증한다.

#### Cycle 32 — v1 canonical shadow import와 원본 불변성

- ISSUE: v1 TOML을 Connection/Target/Plan 관계로 분리하고 Secret payload 없이 별도 Operational Store에 멱등 import하는 실행 경계가 없었다.
- RED: `v1_shadow_import` 통합 테스트가 `dbbackup_config::load_v1_shadow_model`과 `OperationalStore::shadow_import` 부재로 컴파일 실패했다.
- FIX: v1 parser 결과를 정렬된 canonical intermediate model로 변환하고 endpoint/user별 Connection, DB별 Target, Destination, Plan revision 1, membership, 외부 Secret Ref를 deterministic UUID로 import했다. 전체 작업은 `BEGIN IMMEDIATE` 한 transaction이며 기존 control row가 있는 DB는 shadow 대상이 아니라고 거부한다. diff에는 source/imported row, skipped/ambiguous, next-due 미계산 상태, 안전한 secret binding, `legacy_direct`, `cutoverPerformed=false`를 기록한다.
- SECURITY: 원본 EncField는 `LegacySecretSink` 경계에서만 처리하고 SQLite에는 반환된 Secret Ref metadata만 저장한다. canary 검사는 main DB와 `-wal`/`-shm` 전체에 적용했다.
- VERIFY: 같은 source를 같은 DB에 재실행하고 다른 shadow DB에 다른 timestamp로 import해 stable relation digest와 byte-identical diff를 확인했다. 중간 UNIQUE 실패는 import row 전체를 rollback하고 정상 model 재실행에 성공했다. focused 2 passed, catalog 34 passed, config 24 passed, 양 package all-target clippy `-D warnings`, fmt check 통과.
- NEXT: generic tool rescan Operation Run을 정확히 하나의 Work Item subject로 enqueue하는 S02-N05 경로를 Runtime Repository에 추가한다.

#### Cycle 33 — generic Operation Run과 Work Item subject

- ISSUE: Runtime Repository는 Backup Run만 enqueue할 수 있어 tool rescan 등 장기 operation을 durable Work Item에 연결하는 S02-N05 경로가 없었다.
- RED: `generic_tool_rescan_operation_has_exactly_one_durable_work_subject`가 `OperationWorkSpec`과 `enqueue_operation_work` 부재로 컴파일 실패했다.
- FIX: DDL에 허용된 15개 operation kind, bounded subject/request JSON, priority/attempt/time을 application boundary에서 검증하고 Operation Run과 정확히 하나의 `kind=operation` Work Item을 `BEGIN IMMEDIATE` transaction으로 생성했다. 같은 ID와 같은 payload는 기존 result를 반환하고 다른 payload는 stable runtime error로 거부한다.
- VERIFY: tool rescan 최초 생성/동일 replay/다른 payload conflict, operation/work 각 1행, Work Item subject non-null 합계 1과 정확한 Operation Run ID를 확인했다. focused 3 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: resource 하나가 부족하면 allocation과 spool reservation을 하나도 만들지 않고 wait reason만 남기는 S02-X05 admission transaction을 구현한다.

#### Cycle 34 — atomic resource와 spool admission

- ISSUE: Resource Pool, Allocation, Spool Volume/Reservation table은 있었지만 Runtime Repository에 capacity admission 연산이 없어 S02-X05의 all-or-none 보장을 실행할 수 없었다.
- RED: `resource_admission` 통합 테스트가 pool/spool 등록 DTO와 `admit_claim` API 부재로 컴파일 실패했다.
- FIX: 현재 Work Item/Attempt의 owner, boot, attempt, fence, unexpired lease를 다시 확인하고 resource type/key/id canonical 순서로 capacity를 계산한다. 모든 resource와 spool hard/free-floor 조건이 충족된 경우에만 deterministic Allocation/Reservation을 한 transaction에 삽입하며 동일 claim replay는 멱등하다. 부족하면 leased Work Item의 bounded wait reason만 기록한다.
- VERIFY: source capacity를 다른 Attempt가 점유한 상태에서 global+source+spool 요청은 `resource_capacity:source:db.internal:5432`로 대기했고 candidate Allocation 0, Spool Reservation 0이었다. 정상 요청은 Allocation 2, Reservation 1이며 replay 뒤에도 중복이 없었다. focused 2 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: child process identity와 lifecycle mutation을 current Attempt fence에 묶는 Runtime Repository process boundary를 구현한다.

#### Cycle 35 — fenced process identity repository

- ISSUE: Runtime Repository가 Process table을 소유한다고 정의했지만 process start/running/exit를 current Work Item fence에 묶는 API가 없었다.
- RED: `process_repository` 통합 테스트가 `ProcessStartSpec`, `ProcessExitSpec`, process lifecycle API 부재로 컴파일 실패했다.
- FIX: absolute executable path와 optional SHA-256, PID, process start time, service boot ID, Job Object key를 Attempt에 기록한다. start/running/exit는 Work Item ID, Attempt ID/no, fence, owner, boot, unexpired lease를 모두 재확인하며 raw command line이나 secret은 저장하지 않는다.
- VERIFY: 정상 process가 `starting -> running -> exited`로 전이하며 identity와 exit code가 보존됐다. 첫 Attempt를 requeue하고 두 번째 fence가 발급된 뒤 이전 Attempt의 exit mutation은 `WORK_ITEM_FENCE_STALE`로 거부되고 process row는 running 그대로였다. focused 2 passed, all-target clippy `-D warnings`, fmt 통과.
- NOTE: 이 사이클은 Section 02 repository boundary만 구현하며 실제 no-shell spawn, pipe drain, Windows Job Object 제어는 Section 04 범위다.
- NEXT: discovery에서 기존 DB가 누락되어도 Target을 삭제하지 않고 missing/drift 상태를 기록하는 S02-X06을 구현한다.

#### Cycle 36 — discovery drift without Target deletion

- ISSUE: successful discovery observation을 기존 selected Target과 비교하고 누락/변경을 보존하는 Configuration Repository 경계가 없었다.
- RED: `discovery_repository` 통합 테스트가 discovery snapshot DTO, record API, Target discovery status query 부재로 컴파일 실패했다.
- FIX: Discovery Run과 observed database를 transactionally 저장하고 stable engine object ID로 기존 Target과 비교한다. 새 observation은 added count만 기록하고 자동 선택하지 않으며, 누락 Target은 삭제하지 않고 `attention`과 최초 `missing_since_ms`를 남긴다. 다시 나타난 동일 Target만 active로 복구하고 fingerprint/name drift는 attention으로 표시하되 selected source fingerprint를 덮지 않는다.
- VERIFY: DB 2개가 선택된 Connection에서 1개 기존+1개 신규 observation을 기록해 added 1/missing 1, Target 2개 보존, 최초 missing 시각 고정을 확인했다. 이후 fingerprint 변경은 changed 1/attention이며 원 Target fingerprint가 유지됐다. focused 1 passed, all-target clippy `-D warnings`, fmt 통과.
- NEXT: Section 02 요구사항·시나리오·quality gate를 현재 테스트와 1:1 대조해 남은 실행 가능한 결손을 우선순위화한다.

#### Cycle 37 — Section 02 전체 품질 게이트

- ISSUE: 개별 repository 계약은 통과했지만 Section 02의 schema, migration, state machine, writer ownership, shadow import, online backup, projection isolation을 같은 현재 revision에서 함께 검증한 종합 증거가 없었다.
- AUDIT: section 요구사항·acceptance scenarios·quality gate를 1:1 대조해 각각의 실행 테스트 경로를 확인했다. Task XML, 실서비스 catalog, cutover, 실제 child process supervision은 후속 section의 실행 권한·복구·dispatcher 범위이므로 Section 02 완료 주장에 포함하지 않았다.
- VERIFY: `cargo fmt --all -- --check`, `git diff --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `npm run build --prefix apps/console`이 모두 통과했다. catalog 40개를 포함한 workspace non-ignored test는 실패 0이며 외부 MinIO/OpenSSH, live MySQL/MariaDB, pg_restore/Docker, elevated Task Scheduler 항목은 명시적 ignored/not_run 상태를 유지했다.
- TRUTH: evidence-aware `check-g0-truth.ps1`가 `executionAuthority=legacy_direct`, `cutoverMode=rehearsal_only`, `evidenceGate=true`, `durableGate=false`로 통과했다. 따라서 Operational Store 구현을 proved로 닫되 production authority나 durable cutover를 주장하지 않는다.
- RESULT: V2-R02 proved.
- NEXT: Section 03 전체 계획과 기존 service/IPC/secret 구현을 대조하고 첫 미충족 보안 경계를 RED로 고정한다.

#### Cycle 38 — application command와 authority 경계

- ISSUE: CLI/Tauri와 향후 service가 공유할 transport-neutral command catalog가 없어 authorization, idempotency, destructive confirmation, G1 실행 차단을 한곳에서 강제할 수 없었다.
- RED: 새 `dbbackup-application` 계약 테스트가 command/access/mutation/confirmation catalog, canonical payload digest, pre-G1 authority API 부재로 컴파일 실패했다.
- FIX: 35개 command를 read/operate/admin과 read-only/mutate/enqueue/destructive로 분류하고 schema의 8개 confirmation action과 연결했다. object payload는 key-order 독립 BLAKE3 digest로 canonicalize하며 float, 과도한 depth, 1 MiB 초과를 거부한다. Authority Snapshot은 G1 미입증 production enqueue/cutover를 `APPLICATION_REHEARSAL_ONLY`로 차단하고 stable public error만 노출한다.
- VERIFY: command roundtrip/unknown denial, canonical digest, pre/post-G1 경계 3 passed. crate 전체 test, all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: protocol version, request identity/time/body length를 payload decode 전에 거부하는 bounded IPC frame codec과 malformed/fuzz corpus를 TDD로 구현한다.

#### Cycle 39 — bounded IPC frame codec

- ISSUE: IPC transport가 생기기 전에 untrusted length로 allocation하거나 만료·알 수 없는 protocol의 payload를 먼저 decode하지 않도록 하는 wire contract가 없었다.
- RED: `ipc_codec` 테스트가 `RequestEnvelope`, encode/decode, fixed header와 size limit API 부재로 컴파일 실패했다.
- FIX: 4-byte bounded length prefix와 48-byte fixed header에 magic, protocol v1, flags, UUID request ID, issued/expiry, command/key/payload length를 넣었다. version·reserved flags·5분 TTL·30초 clock skew·expiry·각 길이를 JSON payload보다 먼저 검사한다. mutation은 256-byte 제한 idempotency key를 요구하고 payload는 application canonicalizer와 1 MiB limit을 재사용하며 decode 임시 buffer는 zeroize한다.
- VERIFY: protocol roundtrip, unknown version·oversize·expired-invalid-JSON·body-length mismatch, mutation key 경계, 0–511 byte deterministic malformed corpus 4 passed. service crate 전체 test, all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: 실제 client read 중 partial disconnect와 여러 frame 경계를 다루는 bounded stream decoder/response codec을 구현하고 secret-bearing buffer drop zeroize를 검증한다.

#### Cycle 40 — streaming frame wipe와 redacted response

- ISSUE: chunked named-pipe read에서 partial frame이 command로 실행되지 않고 disconnect 시 secret-bearing capacity까지 지워지는 경계와 client용 stable response codec이 없었다.
- RED: stream boundary/multi-frame/partial-secret/response roundtrip 테스트가 `RequestStreamDecoder`와 response API 부재로 컴파일 실패했다.
- FIX: 최대 64 KiB read chunk를 preallocated single-frame buffer에 조립하고 prefix가 완성되는 즉시 declared size를 검사한다. 완성 frame은 decode 뒤, 오류·abort·EOF partial은 `Zeroizing<Vec<u8>>`의 전체 capacity wipe로 제거한다. response는 별도 `DBBR` v1 header, UUID correlation, success/dedup flags, bounded stable code와 object body만 허용하며 application failure의 내부 필드를 private/redacted mapping으로 제한했다.
- VERIFY: 7-byte chunk와 한 chunk의 복수 frame, 1-byte 부족 secret canary frame의 EOF 후 buffered 0/wiped byte count, success/dedup과 `APPLICATION_INTERNAL` redaction roundtrip을 포함해 service 7 passed, application 3 passed, 양 crate all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: SID/session principal과 read/operate/admin command authorization을 deny-by-default로 구현하고 unauthorized 요청이 payload decode·state mutation 전에 0건임을 검증한다.

#### Cycle 41 — SID/session deny-by-default authorization

- ISSUE: named-pipe token에서 얻을 SID/session을 command catalog와 연결하고 unauthorized body를 parser 전에 차단하는 application-independent 인가 경계가 없었다.
- RED: owner/admin 정상 경로와 unknown SID, owner cross-session, non-elevated admin, remote identity, malformed SID 테스트가 auth policy API 부재로 컴파일 실패했다.
- FIX: canonical Windows SID type, local token identity, service/설치 owner/명시 승인 elevated administrator policy와 redacted principal을 추가했다. owner는 설치 시 기록한 logon SID와 session이 모두 일치해야 하며 ledger용 BLAKE3 SID hash만 노출한다. decode를 header preflight와 payload 단계로 분리해 identity 인증→command access 검사→JSON decode 순서를 강제했다.
- VERIFY: read와 destructive command 정상 인가, SID hash 비노출, unknown/cross-session/non-elevated/remote 거부, unauthorized invalid JSON이 `IPC_PAYLOAD_INVALID`보다 먼저 `IPC_UNAUTHORIZED`를 반환하는 3 tests passed. service 전체 10 passed, all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: generic mutating command의 durable ledger를 overwrite restore 전용 경로에서 분리해 재시작 뒤 same-payload result 재사용과 different-payload conflict를 application dispatch 경계로 구현한다.

#### Cycle 42 — durable generic IPC mutation replay

- ISSUE: overwrite restore 외 mutation은 service 재시작 뒤 idempotency 결과를 재사용할 generic ledger/dispatch 경계가 없었다.
- RED: service restart replay, different-payload conflict, terminal failure replay, pre-G1 no-ledger 테스트가 `DurableIpcDispatcher`와 generic ledger API 부재로 컴파일 실패했다.
- FIX: Operational Store에 request를 `running`으로 reserve하고 succeeded/failed_terminal의 bounded JSON 결과를 완성하는 repository API를 추가했다. 같은 SID/command/key의 digest가 다르면 handler 전에 conflict, 미완료 request는 재실행하지 않고 incomplete로 멈춘다. service dispatcher는 authority를 ledger보다 먼저 검사하고 read는 직접, 일반 mutation은 durable replay하며 enqueue/destructive는 atomic typed transaction 없이는 거부한다.
- VERIFY: store close/reopen 뒤 same payload가 handler call 1회와 `deduplicated=true` 결과를 유지하고 다른 payload는 conflict였다. terminal application failure도 1회만 실행·재사용됐으며 pre-G1 backup은 ledger/handler 0건이었다. catalog focused 3 passed, service 12 passed, 양 package all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: confirmation 발급 시 raw random token을 메모리에만 두고 authoritative binding digest만 저장하며, 모든 schema action의 revoke/expire/constant-time validation을 확장한다.

#### Cycle 43 — digest-only destructive confirmation service

- ISSUE: repository에는 overwrite용 수동 digest row만 있었고 service가 raw token을 안전하게 발급·검증하거나 schema의 8개 destructive action을 동일 계약으로 다루는 경계가 없었다.
- RED: 전체 action 발급, raw token DB/WAL/SHM canary, SID/action/resource/payload/token/expiry mismatch, revoke/expire terminal 테스트가 Confirmation Service API 부재로 컴파일 실패했다.
- FIX: OS CSPRNG으로 256-bit token을 생성해 zeroizing memory에만 보관하고 URL-safe 표현을 제공한다. SQLite에는 BLAKE3 digest와 SID/action/resource/fingerprint/canonical payload digest/expiry만 기록한다. 검증은 token digest를 `subtle` constant-time compare한 뒤 모든 binding을 확인하며 mismatch는 하나의 redacted code로 합친다. repository는 8개 action 발급, 조회, SID-bound revoke, expiry sweep을 지원한다.
- VERIFY: 8개 action의 exact binding validation과 각 main DB/`-wal`/`-shm` raw token byte/text 0건, 6종 mismatch 동일 오류, revoke/expire terminal 상태를 포함해 confirmation 3 passed. 기존 overwrite/ledger 3 passed, service 전체 15 passed, application/catalog/service all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: service-owned versioned Secret Store를 Windows machine-scope DPAPI와 install entropy, role metadata, zeroizing plaintext로 구현하고 payload canary가 Operational Store/IPC metadata에 0건인지 검증한다.

#### Cycle 44 — service-owned Windows DPAPI Secret Store

- ISSUE: 기존 keystore는 interactive CLI 경계였고, 무인 service가 역할별 secret을 versioned Ref로 보관·rotation할 machine-scope custody가 없었다.
- RED: Windows DPAPI create/resolve/reopen/rotate/revoke, purpose mismatch, DB·WAL·SHM·secret file canary, protected DACL 감사 테스트가 Secret Store와 secret-ref repository 부재로 컴파일 실패했다.
- FIX: 64 KiB 제한 zeroizing plaintext, 256-bit install entropy, `CRYPTPROTECT_LOCAL_MACHINE|UI_FORBIDDEN` DPAPI, versioned framed blob을 service-owned 디렉터리에 저장한다. entropy/blob DPAPI 반환 buffer는 copy 후 원 allocation까지 zeroize하고 `LocalFree`한다. Operational Store에는 exact Secret Ref, role purpose, version, state, keyed fingerprint만 남긴다. rotation은 같은 locator의 v+1을 만들고 이전 Ref를 retired로 원자 전환해 queued revision이 이전 version을 계속 resolve할 수 있다.
- ACL: protected DACL은 SYSTEM, service SID, 설치 owner SID만 full access로 두며 Everyone/Authenticated Users ACE를 배제한다. root/object/entropy/blob에 적용하고 reopen 시 entropy ACL을 재강제한다.
- VERIFY: process reopen 뒤 unattended decrypt, v1 retired와 v2 active 독립 resolve, purpose mismatch, revoke 차단, main DB/`-wal`/`-shm`/entropy/DPAPI blob plaintext canary 0건, 실제 DACL SDDL 감사를 포함해 Windows 2 passed. service 전체 17 passed, catalog IPC 3 passed, catalog/service all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: SCM 독립 service host의 singleton authority lock, lifecycle start/stop/preshutdown, schema/authority health와 ordered drain을 구현한다.

#### Cycle 45 — SCM-independent service host and ordered lifecycle

- ISSUE: GUI lifetime과 무관한 singleton authority, schema/authority health, SCM stop/preshutdown 상태 전이와 writer checkpoint 순서가 구현되어 있지 않았다.
- RED: 동시 두 번째 host, pre-G1 production claim, GUI client drop, preshutdown 재호출, unjournaled future schema, SCM 상태 계약 테스트가 `host`/`scm` 경계 부재로 컴파일 실패했다.
- FIX: BLAKE3 instance-key 기반 global named mutex로 단일 service authority를 만들고, v2 schema 53-table/WAL/FULL/FK와 G1 authority를 검사하는 health shell을 추가했다. 스키마 drift는 DB를 덮어쓰지 않고 diagnostics-only IPC shell로 격리한다. 종료는 accept 중단 → claim 중단 → client drain → WAL FULL checkpoint → stopped 순으로 한 번만 수행한다. `windows-service 0.8.1` SCM entry는 StartPending/Running/StopPending/Stopped와 STOP/PRESHUTDOWN control을 연결한다.
- VERIFY: future `user_version=99` overwrite 방지 catalog 4 passed, singleton/GUI independence/drift/ordered idempotent shutdown 2 passed, SCM status contract 1 passed, service 전체 20 passed, service all-target clippy `-D warnings`, workspace fmt 통과.
- NEXT: explicit protected-DACL named-pipe listener에서 client impersonation token의 실제 SID/session/elevation을 추출하고, bounded codec과 durable dispatcher를 연결한 end-to-end local IPC transport를 구현한다.

#### Cycle 46 — kernel-enforced named-pipe identity boundary

- ISSUE: command authorization은 synthetic `TokenIdentity`로만 검증됐고, 실제 pipe kernel DACL과 client impersonation token에서 identity를 얻는 OS 경계가 없었다.
- RED: protected DACL 감사, first-instance 경쟁 차단, 현재 client의 user/logon SID·session·elevation 대조 테스트가 secure pipe/identity API 부재로 컴파일 실패했다.
- FIX: Tokio named-pipe 생성에 직접 `SECURITY_ATTRIBUTES`를 전달해 SYSTEM, service SID, owner SID, 명시 승인 admin만 GR/GW 가능한 protected DACL을 적용하고 remote client와 경쟁 first instance를 거부했다. client message read 뒤 `ImpersonateNamedPipeClient` → thread token query → user SID/logon SID/session/elevated-admin 추출 → `RevertToSelf` 순서로 처리하며 pipe session과 token session도 대조한다. impersonation 원복 실패는 privileged thread 재사용을 막기 위해 fail-closed abort한다.
- VERIFY: 실제 kernel object SDDL에 protected DACL과 허용 SID만 존재하고 WD/AU ACE가 없으며, 경쟁 listener가 거부되고 impersonated identity가 실제 client process token과 일치하는 Windows integration 1 passed. service 전체 21 passed, all-target clippy `-D warnings`, fmt와 scoped diff check 통과.
- NEXT: one-frame request/response transport와 bounded concurrency/drain을 만들고 OS identity → authorize/decode → durable dispatch → redacted response의 실제 pipe 왕복을 검증한다.

#### Cycle 47 — bounded end-to-end named-pipe transport

- ISSUE: OS identity, codec, authorization, durable dispatcher가 각각 존재했지만 실제 pipe request/response 경로와 연결 제한·timeout·drain 계약이 없었다.
- RED: 실제 current-user client의 read/mutation/replay/conflict 왕복과 partial-frame shutdown drain 테스트가 frame handler/server/client API 부재로 컴파일 실패했다.
- FIX: one-frame-per-connection transport를 추가해 4-byte 길이를 검증한 뒤에만 최대 크기만큼 할당하고, 전체 frame read 후 OS identity → authorize/preflight/payload decode → bounded `spawn_blocking` durable dispatch → redacted response 순서로 처리한다. listener는 1~128 범위 semaphore로 동시 client를 제한하고 read/write timeout, 완료 task 즉시 reap, accept 중단과 2×I/O timeout drain을 제공한다. client도 SQOS identification 기본값과 전체 request deadline 아래서 connect/write/read한다.
- VERIFY: 실제 pipe에서 read 성공, mutation 최초 실행, 동일 frame durable replay `deduplicated=true`, same-key/different-payload `IPC_IDEMPOTENCY_CONFLICT`, partial frame timeout 후 graceful drain을 확인했다. accepted 5/completed 4/failed 1/peak≤2/forced-close 0인 Windows integration 1 passed. service 전체 22 passed, all-target clippy `-D warnings`, fmt와 scoped diff check 통과.
- NEXT: production SCM runtime에 transport를 조립하고 service/schema health use case와 공통 Rust IPC client를 만든 뒤 CLI/Tauri의 read/config 진입점을 strangler 방식으로 전환한다.

#### Cycle 48 — live service diagnostics application boundary

- ISSUE: transport가 호출할 production use case가 없어 `service.health`와 `schema.status`조차 test fake에 의존했고, host 종료 상태를 client가 읽을 shared probe가 없었다.
- RED: live host의 health/schema 조회와 종료 후 상태 갱신 테스트가 `ServiceApplication`, health probe, operational store handle 부재로 컴파일 실패했다.
- FIX: host 내부 health를 cloneable read probe로 분리해 boot/phase/schema/authority/claims/IPC 상태를 동적으로 읽게 하고, healthy store handle은 clone된 writer actor endpoint만 제공한다. `ServiceApplication`은 empty-object payload를 검증한 뒤 `service.health`와 `schema.status`를 stable JSON으로 반환하며 debug/source error를 노출하지 않는다. diagnostics-only에서는 store 없이도 알려진 schema/diagnostic 상태를 반환할 수 있다.
- VERIFY: rehearsal-ready 53-table v2/WAL/FULL/FK, authority와 claim/IPC flag, ordered shutdown 뒤 stopped/IPC false 갱신을 확인한 1 passed. service 전체 23 passed, all-target clippy `-D warnings`, fmt와 scoped diff check 통과.
- NEXT: reusable Rust IPC client crate를 추가하고 실제 service diagnostics server에 연결해 CLI와 Tauri가 같은 request/response 구현을 공유하도록 첫 read-only strangler를 만든다.

#### Cycle 49 — shared CLI/Tauri IPC client strangler

- ISSUE: CLI와 Tauri가 service wire request를 만들고 응답을 해석할 공통 client가 없어 향후 각각 protocol/timeout/오류 처리를 복제할 위험이 있었다.
- RED: 실제 service diagnostics server에서 공통 client health/schema 왕복 테스트가 새 crate와 `IpcClient` 부재로 컴파일 실패했고, CLI `service status|schema` parsing도 command 부재로 실패했다.
- FIX: `dbbackup-ipc-client` crate가 UUID request ID, 30초 TTL, canonical frame encode, 전체 deadline pipe roundtrip, response ID 대조, stable `CommandOutcome`을 단일 구현한다. CLI는 `service status|schema`, Tauri는 `service_health`/`schema_status` thin command로 같은 client를 호출하며 React data layer에도 동일 typed outcome을 추가했다. 기존 direct execution/config 경로는 아직 변경하지 않았다.
- VERIFY: 실제 protected pipe를 통한 shared client health/schema Windows integration 1 passed, CLI parsing 22 passed, client/CLI/Tauri all-target clippy `-D warnings`, Tauri cargo check, TypeScript+Vite production build, fmt와 scoped diff check 통과.
- NEXT: SCM `ShellRuntime`을 실제 diagnostics pipe runtime으로 교체하고 install-time owner/service identity 설정을 검증·보호해 production endpoint `DbBackup.control.v1`이 service Running 전에 준비되고 stop/preshutdown에서 drain되도록 한다.

#### Cycle 50 — production SCM pipe runtime and protected install identity

- ISSUE: SCM entry가 no-op `ShellRuntime`만 시작해 `Running` 상태에도 production named pipe가 없었고, 설치 owner/admin SID의 보호된 권위 설정과 재부팅 후 owner session 갱신 방식도 없었다.
- RED: 첫 local owner session binding, host-owned pipe ready/drain, schema-drift read-only IPC, 보호된 identity config, SCM bootstrap 실파이프 테스트가 `new_unbound_owner`, `PipeHostRuntime`, read-only handler, identity loader와 assembly 부재로 컴파일 실패했다.
- FIX: owner SID는 설치 설정에 지속하되 logon SID/session은 서비스 부팅 후 첫 유효 local owner 연결에서 원자적으로 고정하고 다른 session을 거부한다. `PipeHostRuntime`은 별도 Tokio runtime의 first listener 준비 handshake, accept 중단, bounded drain/report를 `ServiceHost` lifecycle에 연결했다. diagnostics-only host는 store 없이 health/schema read만 허용하고 mutation을 `APPLICATION_FORBIDDEN`으로 거부한다.
- SECURITY: version 1 `service-identity.json`은 64 KiB/32-admin 상한, canonical SID, 중복·service/owner 충돌, unknown field, symlink를 거부한다. 실제 file DACL을 감사해 protected full-control ACE가 SYSTEM/service/owner에만 있는지 확인하며 Windows SDDL 별칭은 문자열 추측이 아니라 binary SID equality로 비교한다. 누락·오구성·ACL mismatch는 모두 fail-closed다.
- SCM: 실제 process SID, 보호된 owner/admin 설정, authorization/DACL, healthy durable dispatcher 또는 drift read-only handler를 `DbBackup.control.v1`에 조립한다. initial listener가 준비된 뒤에만 bootstrap이 반환되어 SCM이 `Running`을 보고하며 stop/preshutdown은 accept→drain→WAL checkpoint 순서를 유지한다.
- VERIFY: focused owner binding 1, protected identity 2, pipe runtime 2, SCM bootstrap real-pipe 1 passed. `dbbackup-service --all-targets` 29 passed/0 failed, all-target clippy `-D warnings`, workspace fmt와 scoped whitespace check 통과.
- NEXT: section-03 acceptance를 현재 증거에 대조해 section-04 G1/cutover 및 section-10 installer에 속한 의존 항목을 명시한 뒤, durable dispatcher의 첫 미구현 process supervision/fault-matrix 경계를 TDD로 진행한다.

#### Cycle 51 — suspended no-shell Windows process-tree supervisor

- ISSUE: catalog에는 fenced process identity row만 있었고 실제 native tool을 검증·시작·소유하는 runtime crate와 Job Object 경계가 없어 child/grandchild orphan 및 path/hash swap을 막을 수 없었다.
- RED: 상대 executable 거부, wrong-hash no-start, suspended launch와 grandchild tree termination 통합 테스트가 `dbbackup-runtime`, `LaunchSpec`, `ProcessSupervisor` 부재로 컴파일 실패했다.
- FIX: 새 `dbbackup-runtime` crate에 secret-redacted launch spec, 최대 256 args/128 env의 NUL·길이 검증, parent environment를 상속하지 않는 Unicode allowlist block, Windows command-line quoting을 구현했다. absolute non-symlink executable을 read-only sharing handle로 잠근 채 SHA-256을 다시 계산해 expected tool identity와 비교한다.
- WINDOWS: `CreateProcessW(CREATE_SUSPENDED|CREATE_NO_WINDOW|CREATE_UNICODE_ENVIRONMENT)` 뒤 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`이며 breakaway flag가 없는 Job Object에 먼저 assign하고 main thread를 resume한다. PID와 kernel creation time, random Job Object key를 함께 노출하며 terminate/drop이 전체 inherited process tree를 소유한다.
- VERIFY: 재실행된 test helper가 실제 grandchild를 만들고 job termination 뒤 grandchild process handle도 signaled됨을 확인했다. wrong hash는 marker 0건, relative path는 spec 단계에서 거부됐다. runtime 3 passed/0 failed, all-target clippy `-D warnings`, workspace fmt 통과.
- SOURCE: Microsoft Job Objects/AssignProcessToJobObject 공식 계약의 child 기본 상속, suspended assign, breakaway opt-in, kill-on-last-handle semantics를 기준으로 구현했다.
- NEXT: stdout/stderr를 동시에 bounded drain하고 redaction된 tail/progress만 보존하며, 정상 exit·stderr flood·timeout·cancel 모두 wait/reap 뒤 반환하는 supervisor I/O lifecycle을 TDD로 확장한다.

#### Cycle 52 — bounded concurrent process output and deterministic reaping

- ISSUE: suspended process supervision은 stdout/stderr 파이프를 소유하지 않아 대용량 출력에서 교착하거나 비밀 원문을 보존할 수 있었고, timeout 관찰과 명시적 취소 뒤 root/process tree 및 drain thread의 종료 계약도 없었다.
- RED: stdout/stderr 각각 4 MiB를 동시에 쓰는 실제 helper, 4 KiB bounded tail과 literal redaction, timeout 뒤 ownership 유지 및 explicit cancel/reap, 대소문자만 다른 Windows 환경 변수 중복, read 경계를 가로지르는 겹치는 redaction literal 테스트가 각각 필요한 API 또는 경계 위반으로 실패했다.
- FIX: `STARTUPINFOEXW`의 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`로 stdin/stdout/stderr 세 handle만 상속하고 두 전용 drain thread를 resume 전에 시작했다. zeroizing fixed ring과 streaming longest-first literal redactor가 총 byte 수·truncation·bounded tail만 보존하며, parent environment는 비우고 Windows 환경 키를 case-insensitive하게 유일화한다.
- LIFECYCLE: `wait_timeout`은 관찰 timeout에서 ownership을 보존한다. 정상 종료와 explicit terminate는 root를 wait하고 Job Object로 descendants를 종료한 뒤 stdout/stderr drain을 join하여 `ProcessExit` 하나로 반환하며, suspended launch 실패는 root를 직접 종료한다.
- VERIFY: focused redaction 및 pre-launch rejection GREEN. `dbbackup-runtime --all-targets` unit 1 + Windows integration 5 passed/0 failed; 4 MiB × 2 flood, 4 KiB tails, cross-boundary secret 제거, timeout/cancel/reap를 실제 프로세스로 확인했다. all-target clippy `-D warnings`와 workspace fmt check도 통과했다.
- NEXT: catalog의 durable work/occurrence/resource/process primitives를 실제 dispatcher tick과 연결하여 claim→admit→launch→heartbeat→terminal finalize의 첫 영속 실행 경계를 TDD로 구현한다.

#### Cycle 53 — retry settlement releases durable admission atomically

- ISSUE: `requeue_claim`은 Work/Attempt만 갱신하고 `resource_allocations`와 `spool_reservations`를 lease 만료까지 활성 상태로 남겨, 실패한 Attempt가 permit과 디스크 예산을 계속 점유했다.
- RED: capacity 1 resource와 900-byte usable spool을 첫 Attempt가 전부 예약한 뒤 retry로 반환하고, lease 만료 전 두 번째 Work가 즉시 같은 용량을 획득하는 통합 테스트가 admission 거절로 실패했다.
- FIX: 현재 Work Item/Attempt/owner/boot/fence를 확인한 동일 `BEGIN IMMEDIATE` transaction 안에서 retry 상태 전이와 allocation `failed` release, non-quarantined spool release를 함께 commit한다. allocation/spool fence가 하나라도 다르면 전체 transaction을 `WORK_ITEM_FENCE_STALE`로 rollback하며 quarantine은 cleanup 전까지 보존한다.
- VERIFY: focused RED→GREEN. stale retry가 새 Attempt의 예약을 건드리지 않고 첫 Attempt의 release timestamp/reason과 spool terminal state가 정확함을 직접 SQL로 확인했다. `dbbackup-catalog --all-targets` 43 passed/0 failed, all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: claim의 leased→starting→running→committing→terminal 수명주기와 subject Run/Occurrence 및 자원 반환을 하나의 fenced repository 계약으로 구현한다.

#### Cycle 54 — fenced claim, process, subject, and terminal lifecycle

- ISSUE: process row 기록은 Work/Attempt를 `leased/claimed`에 둔 채 따로 움직였고 terminal API가 없어, 실제 supervisor 성공·실패가 Operation/Backup Run과 Occurrence 및 admission 반환에 원자적으로 수렴할 수 없었다.
- RED: `start_claim`, `begin_claim_commit`, `complete_claim` 부재로 operation 전체 수명주기 테스트가 compile-fail했다. running Attempt retry는 pure state machine이 `release`를 거절했고, backup process 실패를 Work/Attempt/Run/Occurrence에 함께 반영할 경계도 없었다.
- FIX: fenced `leased→starting→running→committing→terminal` repository 계약을 추가했다. start는 subject Run/Occurrence를 함께 시작하고, process-running은 process/Work/Attempt를 한 transaction에서 전이한다. commit은 active process 0을 요구하며, completion은 subject와 occurrence aggregate를 갱신하고 lease를 제거한 뒤 allocation/spool을 사유별로 반환한다. reaped `starting/running/committing` Attempt는 retry release가 가능하다.
- SAFETY: claim identity는 Work ID, Attempt ID/no, fence, worker, service boot, unexpired dual lease를 모두 비교한다. malformed 또는 1 MiB 초과 result JSON, 비정규 error code, active process terminalization, terminal replay, stale retry를 fail-closed 처리하며 quarantined spool은 자동 반환하지 않는다.
- VERIFY: operation success가 `succeeded` 3계층과 result JSON, `completed` permit release로 수렴했다. backup native failure가 `failed_terminal/failed/failed/failed` Work/Attempt/Run/Occurrence로 수렴했다. domain 9 + catalog 44 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: runtime crate에 bounded `dispatch_once`를 만들고 catalog claim/admission/lifecycle과 실제 supervisor를 연결하되, trusted launch/resource planner를 trait 경계로 주입한다.

#### Cycle 55 — first durable dispatcher execution boundary

- ISSUE: catalog primitives와 process supervisor가 분리되어 실제 Work가 claim→admission→lifecycle→heartbeat→process exit→terminal settlement로 이어지는 실행 경계가 없었다. admission 대기를 retry로 취급하면 단순 contention 세 번만으로 attempt budget도 소진됐다.
- RED: runtime `Dispatcher`/planner/launcher/clock 계약 부재로 deterministic dispatch 테스트가 compile-fail했고, missing pool을 다섯 번 만난 Work는 기존 `current_attempt_no < attempt_limit` 조건에서 네 번째 claim부터 사라졌다.
- FIX: trusted `DispatchPlanner`, `ProcessLauncher`, `ManagedProcess`, clock seam과 bounded `dispatch_once`를 추가했다. plan은 verified `LaunchSpec`과 canonical resource/spool 요청만 제공하며 poll 간격은 lease보다 짧아야 한다. dispatcher는 admission, pre-launch durable start, process identity, running 전이, heartbeat, bounded timeout, exit evidence, commit 및 terminal resource release를 순서대로 수행한다.
- ADMISSION: `defer_claim`은 leased/claimed를 delayed/abandoned로 fenced 전이하고 lease를 제거한다. abandoned admission 이력은 monotonic attempt/fence audit에는 남지만 execution attempt limit 계산에서는 제외되어 5회 defer 뒤 6번째 claim/admission이 성공했다. permit 부족에서는 native process launch가 0회임을 확인했다.
- WINDOWS: `WindowsProcessLauncher`가 실제 Job Object supervisor의 PID, FILETIME creation time을 Unix ms로 변환한 start identity, job key 및 bounded output counters를 dispatcher에 연결한다. 실제 별도 test process가 stdout/stderr를 쓰고 종료한 뒤 Work/Attempt/Operation/Process가 모두 succeeded/exited로 수렴했다.
- SAFETY: 현재 terminal-on-zero는 `operation` kind에만 허용하여 backup process 성공을 Recovery Point 없이 성공 처리하지 않는다. 다른 kind는 `DISPATCH_KIND_NOT_SUPPORTED`로 fail-closed하며 section-05/06 commit adapter 전에는 production authority를 열지 않는다.
- VERIFY: runtime 10 passed/0 failed(실제 Windows dispatcher 1 포함), catalog 45 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: boot/startup reconciliation이 expired claim, live/missing/reused process identity, exited process, partial spool 증거를 분류하고 old fence를 차단한 뒤 retry/terminal/attention으로 수렴하도록 구현한다.

#### Cycle 56 — startup fencing for expired pre-launch work and real Job identity

- ISSUE: expired leased/starting Work를 찾는 durable scan과 boot recovery가 없었고, DB의 `job_object_key`는 실제로는 익명 Job Object와 무관한 임의 UUID라 Windows에서 다시 열거나 소유 관계를 확인할 수 없었다.
- RED: expired starting claim scan/reconcile API와 `StartupReconciler` 부재로 compile-fail했다. supervisor test는 persisted job key가 `Local\DbBackup.Job.*` 실제 kernel object name이 아니어서 실패했다.
- FIX: bounded `expired_claims(now, limit)` projection이 current Work/Attempt/fence/owner/dual lease와 latest process identity를 읽는다. process row 없이 만료된 operation은 동일 immediate transaction에서 old fence를 즉시 증가시키고, started Attempt를 reconcile-failed로 기록하고, Work를 delayed로 되돌리며 permit/spool을 `reconciled`로 반환한다. old worker write는 다음 claim 전부터 stale이다.
- WINDOWS: supervisor가 UUID 기반 `Local\DbBackup.Job.{uuid}` named Job을 실제로 생성하고 그 정확한 이름을 process row에 기록한다. existing-name collision은 fail-closed한다. 실제 child tree 실행 중 `OpenJobObjectW(JOB_OBJECT_QUERY)`로 같은 Job을 다시 여는 테스트가 통과했다.
- SAFETY: startup reconciler는 현재 processless `operation`만 자동 requeue한다. recorded process는 OS creation-time/Job membership 확인 전 `process_inspection_required`, backup 등 artifact-bearing subject는 Manifest/spool 증거 확인 전 `subject_evidence_required`로 주차하여 성공이나 retry를 추정하지 않는다.
- VERIFY: stale fence가 1→reconcile 2→new claim 3으로 증가하고 first Attempt failure 및 allocation release timestamp를 SQL로 확인했다. catalog 46 passed/0 failed, runtime 11 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check 통과.
- SOURCE: Microsoft `CreateJobObjectW`/Job Objects 문서의 named-only reopen, last-handle kill/destroy 계약과 `OpenJobObjectW` case-sensitive name/access 계약을 기준으로 구현했다.
- NEXT: Windows process inspector가 PID creation time과 named Job membership을 판별하고, missing/reused/exited/matching-owned를 서로 다른 reconciliation 결정으로 연결한다.

#### Cycle 57 — creation-time and named-Job process reconciliation

- ISSUE: startup scan의 recorded PID가 실제 원래 process인지, PID가 재사용됐는지, 해당 process가 supervisor의 named Job에 속하는지 판별할 OS 경계가 없어 모든 process-bearing claim이 수동 attention에 머물렀다.
- RED: 실제 Windows helper를 Job Object 아래 실행한 뒤 `ProcessInspector`, `ProcessProbe`, `ProcessObservation`, `WindowsProcessInspector`를 요구하는 통합 테스트가 공개 API 부재로 compile-fail했다.
- FIX: inspector가 `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE)`와 `GetProcessTimes`로 PID+creation time을 함께 비교하고, zero-time wait로 exit를 확인한 뒤 persisted `Local\DbBackup.Job.*` 이름을 `OpenJobObjectW`로 다시 열어 `IsProcessInJob` membership을 확인한다. 결과는 `Missing`, `Reused`, `Exited`, `MatchingOwned`, `MatchingUnowned`로 분리한다.
- RECONCILE: processless와 달리 recorded-process 경로는 candidate의 process ID/start time/boot/job/state 및 다른 active process 부재를 같은 immediate transaction에서 재검증한다. OS가 missing/reused/exited를 확인한 operation만 process를 `killed/reconciled_process_gone`으로 정리하고 old fence 증가, Attempt failure, Work delay/requeue, permit/spool 반환을 원자적으로 수행한다. matching-owned/unowned는 각각 별도 attention으로 보존한다.
- VERIFY: 실제 Windows owned process는 `MatchingOwned`, 변조한 creation time은 `Reused`, explicit terminate 후에는 `Exited`로 관찰됐다. repository SQL 검증에서 process/Attempt/allocation이 `killed/failed/released`로 수렴하고 old heartbeat가 stale임을 확인했다. catalog 47 passed/0 failed, runtime 14 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check 통과.
- SOURCE: Microsoft `GetProcessTimes`, `OpenProcess`, `WaitForSingleObject`, `OpenJobObjectW`, `IsProcessInJob` 및 Job Objects의 named-object/handle lifetime 계약을 기준으로 구현했다.
- NEXT: retry budget exhaustion과 cancel/timeout race를 terminal settlement로 수렴시키고 stale dispatcher가 새 Attempt를 종료하지 못함을 fault test로 고정한다.

#### Cycle 58 — bounded retry settlement and terminal exhaustion

- ISSUE: retryable process exit를 `requeue_claim`으로 반환할 수는 있었지만 마지막 허용 Attempt도 delayed로 남겨 다음 claim 대상에서 영원히 제외했다. dispatcher에는 retry policy와 retry/exhausted 결과 구분도 없어 모든 nonzero exit를 즉시 terminal failure로 처리했다.
- RED: attempt limit 3인 operation이 같은 capacity-1 permit으로 세 번 nonzero exit할 때 앞의 두 번은 requeue되고 세 번째는 terminal failure가 되어야 하는 테스트가 `with_retry_delay`와 `RetryScheduled` 부재로 compile-fail했다.
- FIX: `DispatchPlan`의 opt-in bounded retry delay와 `RetryScheduled` outcome을 추가했다. catalog의 `retry_claim`은 non-abandoned Attempt 수와 durable attempt limit를 immediate transaction에서 읽어 budget 안이면 delayed/retryable로, 마지막이면 `failed_terminal`/`RETRY_BUDGET_EXHAUSTED`로 Work, Attempt, Operation Run을 함께 수렴시킨다.
- SAFETY: process exit가 먼저 기록되어 active process가 0일 때만 settlement하며 claim/fence/owner/boot를 다시 확인한다. 각 Attempt의 permit/spool release는 같은 transaction에서 한 번만 수행되고 terminal Work는 다시 claim되지 않는다. retry는 plan이 명시적으로 opt-in한 process timeout/nonzero 경로에만 적용된다.
- VERIFY: 세 번의 독립 process launch에서 Attempt 1·2는 `PROCESS_EXIT_NONZERO` retry, Attempt 3은 terminal budget exhaustion이 됐다. SQL로 Work/Operation terminal state, 세 Attempt, 세 allocation의 단일 release를 확인했다. catalog 47 passed/0 failed, runtime 15 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: durable cancel request를 current fence에 연결하고 process exit와 cancel 요청이 경합해도 terminal 전이와 allocation release가 정확히 한 번만 일어나도록 구현한다.

#### Cycle 59 — durable cancellation wins process-exit races

- ISSUE: `cancel_requested` domain/schema 상태는 있었지만 이를 설정·조회하는 repository API가 없었고, successful process exit와 취소 요청이 같은 순간 발생하면 dispatcher가 성공을 먼저 commit할 수 있었다.
- RED: fake process가 successful exit를 반환하는 같은 wait call에서 durable cancel을 요청하고, 최종 Work/Attempt/Operation 및 permit이 정확히 한 번 cancelled로 수렴해야 하는 테스트가 cancel API와 `DispatchOutcome::Cancelled` 부재로 compile-fail했다.
- FIX: `request_cancel(work_item_id, expected_fence, time)`이 active Work만 CAS로 `cancel_requested`로 전이하고 repeated/terminal 요청을 명시적으로 구분한다. dispatcher는 wait slice 사이의 cancel을 감지해 tree termination으로 연결하며, process exit 기록은 cancel-requested phase도 허용한다.
- RACE: begin-commit은 cancel 여부를 원자적으로 판정하고, terminal settlement는 transaction을 다시 열었을 때 cancellation이 도착했으면 proposed success/failure/retry보다 cancellation을 우선한다. retry settlement도 같은 우선순위를 사용한다. 따라서 exit 직전, begin-commit 직전, terminal commit 직전 어느 시점의 요청도 successful overwrite가 불가능하다.
- SAFETY: external cancel은 expected fencing token이 필수라 stale UI/dispatcher가 새 Attempt를 취소하지 못한다. process row가 active인 동안 terminal settlement는 계속 거부되며, exit evidence 뒤 Work/Attempt/subject 전이와 allocation/spool release가 한 transaction에서 한 번만 일어난다.
- VERIFY: 성공 exit와 동시 요청 결과가 `cancelled/cancelled/cancelled`, `CANCEL_REQUESTED`, allocation 1개/해제 1개로 수렴했고 terminal 재요청은 `AlreadyTerminal`로 무변경 처리됐다. catalog 47 passed/0 failed, runtime 16 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: Activity burst가 control/heartbeat writer를 굶기지 않는 priority lane의 latency budget을 100k enqueue stress로 계측하고 bounded batch/backpressure를 고정한다.

#### Cycle 60 — bounded Activity backpressure and heartbeat priority

- ISSUE: high/low writer lane과 Activity batch 상한은 구현돼 있었지만 100k burst에서 queue가 실제로 bounded reject하고 control/heartbeat write가 backlog를 추월한다는 정량 회귀 증거가 없었다.
- FIND: Activity append는 transaction당 최대 128 event, low lane은 `try_send` 기반 capacity 32, control/heartbeat는 별도 high lane이며 writer는 `select_biased`로 high를 우선한다. low producer는 full queue에서 block하거나 unbounded allocation하지 않고 stable `OPERATIONAL_STORE_LOW_PRIORITY_QUEUE_FULL`을 받는다.
- VERIFY: writer를 첫 low job에서 의도적으로 정지한 뒤 low submission 100,000건을 시도했다. 정확히 32건만 queue되고 99,968건은 즉시 backpressure됐으며, release와 동시에 넣은 high control write는 queued low 실행 0건 상태에서 1초 budget 내 먼저 완료됐다.
- SAFETY: queue capacity를 이름 있는 상수로 고정해 구현과 stress assertion의 drift를 막았다. Activity payload는 기존 128-event transaction 상한과 16 KiB message 검증을 유지하며 high lane에는 Activity가 들어가지 않는다.
- VERIFY-GATE: catalog 48 passed/0 failed, runtime 16 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: scheduler의 due/DST/latest-one catch-up/coalesce-one을 현재 crate와 대조하고, 중복 tick에서 occurrence/Run/Work를 transactionally 한 번만 만드는 service wake 경계를 구현한다.

#### Cycle 61 — deterministic IANA timezone and DST due evaluation

- ISSUE: 기존 scheduler는 Windows Task Scheduler용 daily/weekly XML parser뿐이라 service wake가 IANA timezone, DST gap/fold, 절전 복귀 catch-up을 결정적으로 계산할 수 없었다.
- RED: New York DST fold·gap, 4일 recovery window, invalid timezone와 unbounded window 테스트가 `occurrence` module 부재로 compile-fail했다.
- FIX: current `chrono 0.4.45`/`chrono-tz 0.10.4`의 `Tz::from_local_datetime`와 `LocalResult`를 사용한 순수 due evaluator를 추가했다. 최대 366일 window에서 daily/weekly local schedule을 UTC instant로 계산하며 regular와 recovery wake를 분리한다.
- DST: fold는 두 UTC instant 중 earliest를 단 한 번 선택하고 `DstFoldEarliest` resolution을 남긴다. gap은 최대 180분 안의 첫 valid local instant, 즉 transition UTC를 identity로 삼아 `DstGap` miss를 남기며 조용히 다음 시각의 backup 성공으로 바꾸지 않는다.
- CATCH-UP: `bounded_latest_one`은 가장 최신 due 하나만 `CatchUp`으로 선택하고 이전 due를 `SupersededByLatestCatchUp` miss로 보존한다. `skip`은 모든 due를 `CatchUpPolicySkip` miss로 기록한다. timezone 오타와 366일 초과 scan은 stable code로 fail-closed한다.
- VERIFY: New York 2024 fold의 01:30은 05:30Z 한 번, spring gap 02:30은 07:00Z missed, UTC 4일 recovery는 latest 1 + missed 3으로 확인됐다. scheduler 29 passed/0 failed/1 ignored, all-target clippy `-D warnings`, workspace fmt check 통과.
- SOURCE: Chrono-TZ current documentation의 `Tz` parsing, `TimeZone::from_local_datetime`, ambiguous earliest/latest, nonexistent local time 계약을 기준으로 구현했다.
- NEXT: due/missed evaluation을 catalog의 occurrence·Target Run·Work 생성과 한 immediate transaction으로 연결하고 동일 tick/restart replay의 중복 0을 증명한다.

#### Cycle 62 — transactional service wake materialization

- ISSUE: 기존 `upsert_occurrence`와 Target별 `enqueue_backup_work`가 별도 transaction이라 service crash가 pending occurrence 또는 일부 Target Run만 남길 수 있었고, replay에 caller-generated random Run/Work ID를 쓰면 unique conflict를 멱등 성공으로 구분하기 어려웠다.
- RED: published cron revision의 due evaluation을 두 번 materialize해 occurrence 1, Target Run 2, Work 2만 남겨야 하는 service wake 테스트가 scheduled plan constructor와 `ServiceWake` 부재로 compile-fail했다.
- FIX: plan draft가 cron expression/timezone/bounded-latest-one/coalesce-one 정본을 저장하도록 확장했다. `materialize_occurrence`는 deterministic occurrence ID와 occurrence+target 기반 deterministic Backup Run/Work ID를 계산하고, published revision의 frozen target membership·connection engine·source fingerprint를 직접 읽는다.
- TRANSACTION: occurrence insert/select, 모든 Target Run, 모든 Work Item, pending→enqueued 전이를 하나의 `BEGIN IMMEDIATE` transaction으로 commit한다. 기존 row replay는 ID/revision/target/engine/fingerprint/subject를 대조하고 한 필드라도 다르면 rollback한다. missed decision은 `missed_reason` occurrence만 만들고 native Work는 0개다.
- IDEMPOTENCY: 동일 tick/restart replay는 기존 occurrence와 deterministic children을 검증만 하고 생성 count 0을 반환한다. process 중간 crash는 SQLite transaction rollback으로 partial fan-out을 남기지 않으며 published target 최대 1,000개로 batch를 제한한다.
- VERIFY: 2개 Target cron revision에서 첫 wake는 occurrence 1/Run 2/Work 2, 동일 replay는 0/0/0이었다. 직접 SQL cardinality와 occurrence+target distinct count가 모두 정확했다. catalog 48 passed/0 failed, scheduler 30 passed/0 failed/1 ignored, 두 package all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: Section-04 G1 아홉 항목의 machine-readable fault matrix와 current revision/build evidence collector를 만들고, 구현된 항목을 실제 명령으로 묶어 cutover가 stale/missing evidence를 거부하도록 한다.

#### Cycle 63 — G1 fault matrix and durable evidence truth gate

- ISSUE: G1 요구사항은 9개 evidence ID만 선언했고 실행할 명령, 동일 dirty source tree 식별, 전체 실패 수집, stale/위조 증거 거부가 없어서 수동 결과로 cutover를 잘못 승인할 수 있었다.
- RED: collector의 all-pass/one-fail fixture와 truth checker의 valid/stale durable evidence fixture를 먼저 추가했다. 실제 무인자 실행은 PowerShell 5.1의 `param()` 시점 `$PSScriptRoot` 공백과 단일 Git 결과의 `Char.Trim()` 오류를 드러냈고 회귀 테스트로 고정했다.
- FIX: `g1-fault-matrix.json`에 occurrence uniqueness, stale fencing, process-tree reaping, checkpoint crash, spool, permit, destination truth, singleton service, online backup의 정확한 9개 Windows 명령을 선언했다. collector는 HEAD와 tracked+untracked source-tree SHA-256, toolchain/OS 기반 build ID를 모든 row에 결합하고 실패 후에도 끝까지 실행하며 raw stdout/stderr는 증거 JSON에 저장하지 않는다.
- TRUTH: checker는 matrix/requirements의 정확한 ID·명령 대응, 9개 row, pass/exit 0, 동일 revision/tree/build/runner, 현재 HEAD/tree freshness, timing 필드, raw output 금지를 검사한다. fixture의 정상 durable evidence는 통과하고 stale row는 거부되며 무인자 실제 저장소 실행도 통과했다.
- VERIFY: collector fixture는 9/9 pass와 중간 1 fail 후 9개 전부 실행을 통과했고, truth fixture 16개가 통과했다. 실제 G1은 revision `bd6a97145db03fe2ccb2fa46e453f84f1e1ca575`, tree `387ec115bc285d76614774f65aef20149c40c87f4ffb8641284e8b202dd61619`, build `c8591dbf5dc04bdbf2a0d7da36450a9f7721bcf93ef832ac2af691052bba95e7`에서 8/9 pass였다. 유일한 실패는 아직 없는 `recovery_commit_crash` target이며 truth gate는 `status=fail`과 해당 row를 이유로 exit 1 거부했다. workspace fmt check도 통과했다.
- STATE: G1은 아직 false이고 durable cutover는 금지된다. evidence는 ignored artifact `artifacts/g1/evidence.json`에 남겼으며 `.gitignore`는 `/artifacts/g1/`만 제외한다.
- NEXT: Section-05의 recovery-point commit adapter와 checkpoint별 crash reconciliation을 TDD로 구현해 `g1.checkpoint_crash_reconciliation`을 실제 통과시키고, 같은 source tree에서 G1 9/9를 재수집한다.

#### Cycle 64 — published Manifest crash reconciliation and G1 closure

- ISSUE: startup reconciler는 만료된 backup claim을 artifact 조사 없이 `subject_evidence_required`로만 보냈고, complete Recovery Point projection과 Backup Run/Work/Attempt terminal 전이가 서로 다른 transaction 경계라 Manifest publish 뒤 DB crash를 자동 수렴시킬 수 없었다.
- RED: `recovery_commit_crash` target을 먼저 추가했다. Manifest publish 후 첫 재시작과 projection 작성, Run 연결, Work terminal staging의 세 DB checkpoint에서 실패를 주입하고, 부분 projection/terminal state 0과 다음 재시작의 정확히 한 번 성공을 요구했으며 `VerifiedRecoveryPointSource`와 origin-aware commit API 부재로 compile-fail했다.
- FIX: complete Recovery Point spec에 occurrence/Backup Run origin을 추가하고, signed source가 제공한 동일 ID·origin·engine을 검증한다. catalog projection insert/required-Part completeness/Snapshot 작성, `backup_runs.recovery_point_id` 연결, Backup Run·Occurrence·Work·Attempt 성공, allocation/spool release를 하나의 `BEGIN IMMEDIATE` transaction에서 수행한다.
- RECONCILE: startup은 기록 process의 PID/start-time/Job 증거를 먼저 검사한다. process가 살아 있으면 commit하지 않고 Attention을 유지하며, gone/exited가 확인되고 complete Manifest가 있을 때만 만료된 committing fence와 origin을 다시 대조해 projection을 복구한다. transaction이 이미 성공했으면 claim이 terminal이라 재실행 후보가 되지 않는다.
- IDEMPOTENCY: 같은 Recovery Point가 catalog rebuild로 먼저 들어온 경우 checksum, signing key, origin, source, 상태 축과 row cardinality가 일치할 때만 기존 projection을 인정한다. 같은 ID의 다른 truth는 `CATALOG_PROJECTION_INPUT_INVALID`로 거부한다.
- VERIFY: fault checkpoint 3개는 모두 전체 rollback 후 재시작 한 번에 Work/Attempt/Run/Occurrence `succeeded`, Recovery Point/Snapshot 1개로 수렴했다. catalog 48 passed/0 failed, runtime 18 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check가 통과했다.
- G1: 실제 9항목 collector가 revision `bd6a97145db03fe2ccb2fa46e453f84f1e1ca575`, build `78c1fc9126d0f14f9f93bb3ce93c5fe68737ad3b3c00b094ac90200f4af50d27`에서 9/9 pass했고 truth checker가 `durableGate=true`를 확인했다. 이 로그 반영으로 tree digest가 바뀌므로 동일 collector/checker를 즉시 재실행해 final current-tree evidence를 갱신한다.
- STATE: V2-R04와 G1 Durable Core는 proved다. 실행 권한은 별도 section-03 cutover가 수행되기 전까지 계속 `legacy_direct`/`rehearsal_only`다.
- NEXT: Section-05의 provider-neutral multi-part Manifest v2, required/best-effort Replica truth, repair/rebuild/retention quality gate를 순서대로 완성한다.

#### Cycle 65 — provider-neutral artifact contract and additive set Manifest v2

- ISSUE: v1 Manifest는 한 spool payload의 dump flag와 checksum만 서명해 Recovery Point origin, 여러 Part, native tool hash, destination requirement/reader 역할을 복원 가능한 한 집합으로 인증할 수 없었다.
- RED: provider-neutral Artifact contract, canonical set Manifest golden vector, Part/Replica truth 변조, unknown field/version, deterministic object prefix 테스트가 새 타입 부재로 compile-fail했다.
- CORE: `dbbackup-core`에 `ArtifactSpec`, `PartSpec`, `ArtifactPartRole`, `ConsistencyGrade`를 추가했다. nil/중복 ID, 중복 logical name, 잘못된 format, required database dump 부재를 stable code로 거부하며 engine provider가 storage 구현을 알지 않고 필요한 Part 집합을 선언할 수 있다.
- MANIFEST: additive `SetManifestV2`는 schema 2, Recovery Point/instance/occurrence/Backup Run/Target/source identity, engine/consistency, commit time, signing key ID, native tool name/version/SHA-256, 정렬된 sanitized options, 모든 encoded Part checksum·크기·format·wrap generation·object key, 정렬된 Replica requirement/reader/immutability/Manifest key를 고정 필드 순서 JSON으로 서명한다.
- FAIL-CLOSED: v2와 모든 하위 wire struct는 unknown field를 거부하고 schema 2 외 버전을 거부한다. Part와 Replica는 deterministic `v2/{occurrence}/{target}/{recovery-point}` prefix 및 정렬·유일성을 강제한다. 제공된 pinned verifying key와 envelope key가 다르거나 어떤 복원 사실이 변조돼도 `MANIFEST_V2_SIGNATURE_INVALID`다.
- COMPATIBILITY: 기존 `Manifest`/`SignedManifest` 타입과 v1 write/read/fan-out API는 변경하지 않고 v2 타입을 병렬 추가했다. v1 pipeline E2E 5개가 그대로 통과했다.
- VERIFY: core 3 passed/0 failed, pipeline 8 passed/0 failed, 두 package all-target clippy `-D warnings`, workspace fmt check 통과. canonical JSON 전체 바이트 golden vector와 고정 Ed25519 key 변조 검사가 통과했다.
- NEXT: v2 signed set Manifest를 payload-last local fan-out과 reader round-trip에 실제 연결하고, required/best-effort 결과에서 completeness·durability·compliance·eligibility 네 축을 계산한다.

#### Cycle 66 — payload-first/Manifest-last replica commit와 별도 Reader 검증

- ISSUE: 서명된 set Manifest v2가 실제 Destination writer와 연결되지 않았고, writer 자격증명으로 업로드한 객체를 별도 Reader 권한으로 재검증한 뒤에만 commit marker를 게시한다는 규칙도 실행 경로에 없었다.
- RED: 두 Part를 로컬 목적지에 게시한 뒤 별도 Reader가 바이트 수·BLAKE3를 확인하고 마지막에만 set Manifest가 존재해야 하는 테스트와, Reader가 payload를 변조해 관측하면 payload orphan만 남고 Manifest는 게시되지 않아야 하는 fault 테스트가 공개 API 부재로 compile-fail했다.
- FIX: `DestinationReaderV2`와 bounded Manifest read, streaming object probe를 writer와 분리했다. `publish_set_manifest_v2`는 서명·pinned key, Part source ID 집합, Replica endpoint fingerprint 집합, source file, bounded parallelism을 fan-out 전에 fail-closed 검증한다.
- COMMIT: 각 목적지는 모든 encoded Part를 순서대로 commit하고 별도 Reader가 signed evidence의 ciphertext bytes/BLAKE3를 확인한 경우에만 signed set Manifest를 마지막 객체로 게시한다. Manifest도 Reader로 4 MiB 이내에서 다시 읽어 strict JSON parse, pinned signature, 원본 envelope 동일성을 검증한다. 실패 목적지는 `Unavailable`과 stable error code를 반환하며 이미 게시된 payload는 commit된 Recovery Point가 아닌 진단 가능한 orphan으로 남는다.
- CONCURRENCY: 목적지 fan-out은 `max_parallel_destinations` 기본 4와 `buffer_unordered`로 제한했다. 결과는 fingerprint 순으로 정렬하며 required/best-effort 역할, payload/Manifest receipt, 시도 횟수, Reader 상태를 분리해 보존한다.
- VERIFY: `manifest_v2` 5 passed/0 failed, 기존 pipeline E2E 5 passed/0 failed, pipeline 전체 10 passed/0 failed, `cargo clippy -p dbbackup-pipeline --all-targets -- -D warnings`, `cargo fmt --all -- --check` 통과.
- NEXT: 실제 publish outcome과 Part/Replica 상태만으로 completeness·durability·compliance·restore eligibility 4축을 계산하는 단일 truth projector를 TDD로 추가하고 catalog projection의 수동 축 입력을 제거한다.

#### Cycle 67 — evidence-derived Recovery Point four-axis projection

- ISSUE: `CompleteRecoveryPointSpec` 호출자가 completeness·durability·compliance·eligibility 네 값을 임의로 주입할 수 있었고 catalog insert도 모든 Replica를 required/local/committed/eligible로 하드코딩해 실제 fan-out 부분 실패와 다른 Snapshot을 만들 수 있었다.
- RED: required NAS 실패와 best-effort Local 성공, required 성공과 best-effort 실패, writer 성공과 Reader unavailable 세 조합의 pure truth test가 Replica state/Reader 타입과 `SnapshotAxes::calculate` 부재로 compile-fail했다.
- CALCULATOR: required Part 전체를 가진 committed+eligible Replica를 실제 Stored Object 관계로 계산한다. 모든 required Replica가 적격이면 healthy/compliant/eligible, 일부 적격 사본만 있으면 degraded/non-compliant/conditional, 적격 사본이 없으면 incomplete/unavailable/non-compliant/blocked다. best-effort 실패는 required 성공을 강등하지 않는다.
- INPUT: 수동 `SnapshotAxes::new`와 constructor 인자를 제거했다. catalog insert는 실제 destination kind, required/best-effort, Replica state, reader status, commit 시각을 저장하고 required Part present count도 committed+eligible Replica의 payload만 센다.
- BRIDGE: `CompleteRecoveryPointSpec::from_verified_publish`가 pinned signature를 독립 검증하고 Manifest Part/Replica와 실제 `ReplicaPublishOutcomeV2`의 정확한 fingerprint·requirement·receipt cardinality·byte count를 대조한다. deterministic Replica/Object ID, payload와 set Manifest object, origin/run/engine/consistency를 생성한 뒤에만 complete projection 입력을 허용한다.
- VERIFY: 새 replica truth 4 passed/0 failed, catalog 전체 48 passed/0 failed, recovery commit crash 2 passed/0 failed, catalog/runtime all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: 기존 encoded artifact를 읽는 repair 계획과 source/destination preflight를 pure contract로 만들고 durable `replica_repair` Operation Work에 연결한다.

#### Cycle 68 — encoded-object Replica repair와 durable operation

- ISSUE: required Destination 복구가 원본 DB dump 재실행 없이 기존 encoded object를 재사용하는 실행 경로와 durable audit subject가 없었다. 기존 원격 source는 Manifest 전체 읽기만 제공해 대용량 repair의 메모리 상한도 없었다.
- RED: 적격 source의 signed Manifest와 encoded payload만으로 빈 target을 복구하고 Manifest를 마지막에 commit하는 테스트, source payload가 서명 checksum과 다르면 target 최종 객체가 0이어야 하는 fault 테스트가 repair API 부재로 compile-fail했다. 별도 catalog 테스트는 deterministic `replica_repair` Operation Work API 부재로 compile-fail했다.
- REPAIR: `repair_replica_v2`는 source와 target fingerprint가 signed Manifest에 있고 서로 다른지 확인하고, source set Manifest를 별도 Reader로 다시 읽어 pinned signature와 원본 envelope를 대조한다. 각 encoded Part는 source Reader에서 target의 uncommitted writer로 복사하면서 바이트 수/BLAKE3를 계산하며, 일치할 때만 payload를 commit하고 target Reader를 재검증한다. 모든 Part 성공 뒤에만 set Manifest를 게시한다.
- BOUNDS: repair API에는 BackupSource나 dump plan 입력이 없다. Local/S3/SSH Reader는 1 MiB chunk로 payload를 전달하며 signed ciphertext size를 hard limit으로 사용한다. S3는 OpenDAL `stat`과 range Reader, SSH는 SFTP metadata와 Tokio `AsyncRead`를 사용하고 Manifest 전체 읽기는 4 MiB로 제한한다.
- DURABILITY: `ReplicaRepairWorkSpec`은 Recovery Point+source+target에서 deterministic Operation Run/Work ID를 만들고 schema-versioned request JSON을 `replica_repair`/`recovery_point` subject로 원자 enqueue한다. 동일 요청 replay는 row를 늘리지 않으며 source=target은 거부되고 `backup_run_id`는 항상 NULL이라 재덤프와 분리된다.
- VERIFY: repair 2 passed/0 failed, pipeline 전체 12 passed/0 failed, catalog runtime 5 passed/0 failed, S3 14 passed/0 failed/2 ignored, SSH 8 passed/0 failed/2 ignored, 네 package all-target clippy `-D warnings`, workspace fmt check 통과. OpenDAL/russh-sftp의 현재 reader/stat/AsyncRead 계약은 Context7의 upstream 문서로 확인했다.
- NEXT: retention evaluator와 destructive executor를 분리하고 dry-run/hold/Object Lock/last-compliant/active-source/partial-delete matrix를 TDD로 구현한다.

#### Cycle 69 — pure retention dry-run safety matrix

- ISSUE: retention 정책 만료와 실제 삭제 가능성을 같은 boolean으로 취급하면 pinned, legal hold, Object Lock, 마지막 compliant 사본, 실행 중 restore/repair source를 자동 삭제할 수 있었다. tombstone과 grace가 없는 즉시 삭제도 차단할 계약이 없었다.
- RED: 여섯 보존 사유를 각각 입력한 dry-run은 모두 `Preserve`여야 하고, 안전한 만료 후보도 tombstone 전·grace 중·grace 후가 각각 다른 disposition이어야 하는 테스트가 retention API 부재로 compile-fail했다.
- FIX: `evaluate_retention_v2`를 side-effect 없는 pure evaluator로 추가했다. 입력 검증 뒤 정책 미만료, pinned, legal hold, active Object Lock 또는 lock evidence 부재, server append-only, last compliant Recovery Point, active restore, active repair를 deterministic reason 목록으로 반환한다.
- LIFECYCLE: blocker가 없는 만료 후보도 즉시 delete가 아니라 `TombstoneRequired`다. tombstone이 있으면 checked grace deadline 전에는 `WaitForGrace`, 이후에만 `DeleteEligible`이며 마지막 compliant 후보는 `would_break_compliance=true`를 별도로 노출한다.
- VERIFY: retention matrix 3 passed/0 failed, pipeline 전체 15 passed/0 failed, pipeline all-target clippy `-D warnings`, workspace fmt check 통과.
- NEXT: delete-operator 전용 trait, confirmation-bound durable tombstone Operation, 객체별 partial delete observation과 retry 상태를 구현한다.

#### Cycle 70 — confirmation-bound retention delete와 객체별 정산

- ISSUE: dry-run 결과가 실제 destructive Work에 결합되지 않았고, writer와 분리된 delete authority 및 partial delete 뒤 성공 객체·재시도·Object Lock·Attention을 함께 보존하는 정산 경계가 없었다.
- RED: Manifest-first 삭제 순서와 객체별 결과, 승인 digest 불일치, confirmation binding rollback, transient partial delete, Object Lock 보존, stale fence, retry convergence 테스트를 먼저 추가해 공개 API 부재와 동작 차이를 확인했다.
- EXECUTOR: `DestinationDeleteV2`를 write/read와 분리하고 `LocalDeleteOperatorV2`를 안전한 상대 key와 idempotent missing-file 삭제로 구현했다. confirmed plan만 실행하며 set Manifest를 먼저 삭제하고 `deleted`, `retained_by_lock`, `retry_required`, `failed_terminal`을 객체별로 기록한다.
- AUTHORIZATION: `retention.delete` IPC는 dry-run digest와 one-time confirmation의 SID/action/Recovery Point/Destination/payload binding을 한 transaction에서 소비한 뒤 schema-versioned `retention_delete` Operation Run과 단일 Work Item을 enqueue한다. binding 불일치 시 ledger, token, Operation, Work가 모두 rollback된다.
- SETTLEMENT: current Attempt fence와 Operation request를 다시 검증한 뒤 실제 report의 key/role 집합이 catalog Replica와 정확히 일치할 때만 성공 객체를 `deleted`로 반영한다. Replica와 Snapshot 4축을 현재 Stored Object truth로 재계산하고, transient 실패는 같은 transaction에서 Work를 재큐잉하며 result JSON, Activity, Attention을 남긴다. retry 성공은 남은 객체를 삭제하고 Attention을 resolve하며, Object Lock은 일반 실패가 아닌 보존 evidence와 partial success로 남긴다.
- SAFETY: stale claim은 projection mutation 0건으로 거부되며, 이미 삭제된 객체는 retry에서 `deleted` observation만 허용한다. 모든 객체가 삭제되면 Replica는 `missing/unavailable`, 일부만 삭제되면 `degraded/unavailable`로 전환된다.
- VERIFY: retention pipeline 5 passed/0 failed, catalog 전체 test 통과, 신규 retention settlement 4 passed/0 failed, IPC confirmation 4 passed/0 failed, runtime regression 5 passed/0 failed, pipeline/catalog all-target clippy `-D warnings`, workspace fmt check, `git diff --check`를 통과했다.
- NEXT: Catalog rebuild v2 dry-run의 accepted/rejected/unknown-key 진실값과 실제 Destination Reader 기반 L1 round-trip evidence를 완성해 Section-05 quality gate를 닫는다.

#### Cycle 71 — signed Manifest v2 rebuild dry-run과 atomic projection replace

- ISSUE: 기존 rebuild는 v1 Manifest와 단일 verifier만 처리해 v2 signing key ID별 신뢰 여부, source Destination 선언, 실제 payload Reader 증거를 구분하지 못했고 unknown key를 tamper와 같은 거부 묶음으로 숨겼다.
- RED: valid, unknown-key, signature-tampered, payload-corrupt 후보를 한 inventory에 넣고 dry-run 분류, mutation 0건, accepted-only replace, Control State digest 불변, empty replace 거부를 먼저 실패시켰다.
- CLASSIFY: `prepare_catalog_rebuild_v2`는 bounded Reader로 최대 4 MiB set Manifest를 읽고 strict JSON/schema와 canonical envelope를 확인한다. 구조적으로 유효하지만 keyring에 signing key ID가 없으면 `UnknownKey`, pinned signature 불일치·Destination 미선언·Manifest key 불일치·payload 부재/불일치·projection 불능은 구체적인 rejected reason으로 분리한다.
- READER TRUTH: accepted 후보는 source Destination이 signed commitment에 선언되어 있고 모든 encoded Part가 실제 Reader의 byte count와 BLAKE3에 일치할 때만 생성된다. 관찰하지 않은 다른 Replica는 committed로 합성하지 않고 `missing/unavailable` outcome으로 projection에 전달한다.
- APPLY: `CatalogRebuildPlanV2`는 accepted spec만 내부에 보유하며 `Merge` 또는 `Replace`로 한 `BEGIN IMMEDIATE` transaction에서 반영한다. Replace는 accepted 0건이면 catalog wipe를 거부하고, rebuildable catalog table만 교체해 Operation/Work 등 Control State를 건드리지 않는다.
- VERIFY: 신규 v2 rebuild 2 passed/0 failed, catalog 전체 test 통과, pipeline/catalog all-target clippy `-D warnings`, workspace fmt check, `git diff --check`를 통과했다.
- NEXT: 실제 Destination Reader에서 signed set Manifest와 encoded payload를 읽어 decrypt/decompress/hash까지 수행하는 L1 v2 round-trip evidence를 구현한다.

#### Cycle 72 — Destination Reader 기반 L1 v2 round-trip

- ISSUE: 기존 L1은 v1 Manifest와 로컬 spool path에 결합되어 signed multi-part v2 Replica가 원격 Reader에서 실제로 복원 가능한지 증명하지 못했다.
- RED: 실제 pipeline으로 plaintext를 zstd+ChaCha20Poly1305 STREAM encoding하고 Local Destination에 v2 payload/Manifest를 publish한 뒤 Reader-only L1 성공, destination 1-byte corruption, wrong DEK 실패 테스트를 먼저 추가했다.
- GATE: `verify_l1_from_reader_v2`는 bounded Reader로 canonical signed set Manifest를 다시 읽고 pinned Ed25519 signature, source Destination commitment, exact Manifest object key를 검증한 뒤에만 DEK provider를 호출한다.
- ROUND-TRIP: 각 encoded Part를 Reader에서 격리 TempDir의 Local writer로 streaming copy하고 Reader probe뿐 아니라 staging file 자체의 byte count와 BLAKE3를 다시 계산한다. signed ciphertext 증거가 일치할 때만 DEK를 resolve하고 STREAM decrypt와 zstd decode를 수행하며 평문은 메모리에 보관하지 않고 chunk별 BLAKE3와 byte count만 계산한다.
- BOUNDS: 호출자가 Part별 ciphertext/plaintext 상한을 명시하며 signed size가 이를 넘으면 I/O·복호화 전에 거부한다. 복호화 후 zstd 출력도 chunk마다 상한을 확인해 decompression expansion을 제한하고 staging 파일은 검증 종료 시 자동 삭제한다.
- EVIDENCE: 성공 report는 Recovery Point, Destination fingerprint, Manifest key, 검증 시각, Part별 실제 ciphertext/plaintext byte count와 BLAKE3만 담고 L2/L3를 암시하지 않는다.
- VERIFY: 신규 L1 v2 2 passed/0 failed, verify package 전체 21 passed/0 failed/2 ignored, verify/pipeline/catalog all-target clippy `-D warnings`, workspace fmt check, `git diff --check`를 통과했다.
- NEXT: catalog rebuild를 specialized durable Run/Work와 fenced completion 및 후보별 ingest audit에 결합해 Section-05의 durable audit acceptance를 닫는다.

#### Cycle 73 — durable catalog rebuild Run/Work와 후보 감사 원자 정산

- ISSUE: v2 rebuild 판정과 projection apply는 메모리 plan 및 단발 transaction만 제공해 `catalog_rebuild_runs`, `catalog_rebuild` Work, current Attempt fence, 후보별 `catalog_manifest_ingest` 감사와 terminal 상태를 하나의 durable 경계로 남기지 못했다.
- RED: accepted·unknown-key·signature-tampered 후보를 준비해 specialized rebuild Run/Work를 enqueue·claim·start한 뒤 atomic settle, Run/Work/Attempt 성공, 감사 결과 3종, terminal claim replay 차단을 요구한 테스트가 공개 타입과 repository API 부재로 compile-fail했다.
- FIX: source kind와 dry-run/merge/replace mode를 갖는 `CatalogRebuildWorkSpec` 및 멱등 enqueue를 추가했다. lifecycle은 rebuild Run의 queued→running→terminal을 Work/Attempt와 함께 전이하며 claim row가 dedicated rebuild subject를 보존한다.
- SETTLEMENT: 현재 fence와 persisted mode/status를 재검증한 단일 `BEGIN IMMEDIATE` transaction에서 선택적으로 rebuildable projection을 교체하고 accepted spec만 ingest한다. 실제 Manifest bytes의 BLAKE3를 후보 evidence에 보존하고 accepted/invalid-signature/unknown-key/read/schema/evidence mismatch를 후보별 감사 row로 기록한 뒤 Run count와 Work/Attempt를 함께 성공시킨다. Dry-run은 projection mutation 0건이며 stale terminal replay는 `WORK_ITEM_FENCE_STALE`로 차단된다.
- VERIFY: focused durable rebuild 1 passed, rebuild v2 3 passed, catalog package 전체 62 passed/0 failed, catalog all-target clippy `-D warnings`, workspace fmt check, `git diff --check`를 통과했다.
- NEXT: section-05 acceptance와 현재 adapter 구현을 대조해 원격 Destination delete/Object Lock 또는 quality-gate의 다음 실제 결손 하나를 고른다.

#### Cycle 74 — version-aware Stored Object identity

- ISSUE: S3 versioned/Object Lock bucket에서 같은 key의 여러 version은 서로 다른 물리 객체인데 retention confirmation, delete observation, catalog projection/rebuild/settlement가 key만 식별자로 사용해 잘못된 version을 삭제 처리할 수 있었다. version 없는 DELETE는 versioned bucket에서 delete marker만 만들 수 있어 영구 삭제 증거도 될 수 없다.
- RED: `RetentionObjectV2::versioned`와 `DeleteObjectObservationV2.object_version`을 요구한 pipeline 테스트가 해당 API 부재로 compile-fail했고, projection 테스트는 receipt version을 저장할 API와 조회 결과가 없어 실패했다.
- FIX: retention 객체·confirmation digest·delete trait/report를 `(object_key, object_version)` identity로 확장하고 Local operator는 지원하지 않는 version을 fail-closed 처리했다. Destination upload/probe receipt에 version을 추가했으며 S3 writer/reader는 OpenDAL metadata의 version ID를 보존하고 SSH/Local은 `None`을 명시한다. projection/rebuild는 version을 저장하며 retention settlement는 key+version의 완전한 집합과 exact SQL predicate를 검증한다.
- REGRESSION: 동일 replica에 같은 `payload/object.bin` key의 `version-a`와 `version-b`를 함께 넣고 전자만 deleted, 후자는 retry/present로 남는 저장소 정산 테스트를 추가해 두 version이 혼동되지 않음을 고정했다.
- VERIFY: focused pipeline version identity 1 passed, focused projection version persistence 1 passed, focused settlement identity 1 passed. pipeline/catalog/S3/SSH 패키지 전체 테스트가 통과했고 S3·SSH live credential 테스트 4개만 기존대로 ignored였다. 네 패키지 all-target clippy `-D warnings`, workspace fmt check, `git diff --check`도 통과했다.
- NEXT: append-only writer와 분리된 S3 delete authority를 구현하고 exact `versionId`가 없으면 delete marker를 성공으로 오인하지 않도록 fail-closed 처리한다.

#### Cycle 75 — exact-version S3 delete authority와 Object Lock 판별

- ISSUE: S3 adapter는 append-only writer와 reader만 있고 실제 retention delete authority가 없었다. versioned bucket에 version 없는 DELETE를 보내면 보호된 version을 제거하지 않고 delete marker만 만들며, 모든 403을 Object Lock으로 분류하면 IAM 오설정을 보존 증거로 오인한다.
- RED: 별도 `S3DeleteOperatorV2`를 요구한 테스트가 공개 타입 부재로 compile-fail했다. 첫 구현 뒤 Memory backend는 `delete_with(...).version(...)` 인수를 실제로 받았음을 `Unsupported(version)`으로 드러내며 versionless fallback을 차단하는 추가 RED가 됐다.
- AUTHORITY: writer 설정과 공유하지 않는 `S3DeleteCfg` 및 delete-only IAM policy를 추가했다. 정책은 exact version 삭제와 retention/legal-hold 조회만 허용하고 Put, versionless Delete, lock 변경, Governance bypass를 명시적으로 거부한다.
- EXECUTION: 모든 S3 삭제는 안전한 relative key와 1~1024-byte version ID를 요구하고 OpenDAL `delete_with(path).version(version)`만 호출한다. version 누락은 네트워크 요청 전 terminal failure, NotFound는 idempotent success, rate-limit/temporary 오류만 retryable이며 unsupported와 일반 권한 거부는 terminal이다.
- LOCK TRUTH: production operator는 exact version의 `GetObjectLegalHold`와 `GetObjectRetention`을 SigV4로 preflight한다. legal hold ON 또는 미래 retain-until만 `RetentionLocked`로 분류한다. DELETE 403 뒤 재조회가 unlocked이면 Object Lock이 아닌 terminal IAM failure로 보존해 false preservation을 막는다.
- VERIFY: deterministic HTTP S3 fixture가 exact `versionId` query, 204 성공, 404 replay, active retention에서 DELETE 0건, generic 403 비-lock 분류를 검증해 5 passed/0 failed했다. S3 package는 unit 15, delete integration 5, reader integration 1 passed/0 failed/2 existing live ignored였고 all-target clippy `-D warnings`, workspace fmt check, `git diff --check`를 통과했다.
- NEXT: SSH/SFTP delete-only credential과 host-key-pinned operator를 writer/reader에서 분리하고 idempotent remove 및 transient 분류를 고정한다.

#### Cycle 76 — host-key-pinned SSH delete authority

- ISSUE: SSH adapter도 writer와 reader만 있고 retention Work가 사용할 별도 destructive authority가 없었다. 일반 문자열 오류로 SFTP 삭제 결과를 처리하면 missing replay, 권한 거부, 연결 손실을 동일하게 취급할 수 있었다.
- RED: `SshDeleteCfg`와 `SshDeleteOperatorV2`를 요구하고 versioned/unsafe identity가 연결 전에 거부되는지 확인한 integration test가 공개 타입 부재로 compile-fail했다.
- AUTHORITY: host/user/auth/host-key fingerprint/absolute remote root를 독립 보유하는 delete config를 추가했다. 비어 있거나 control character가 있는 필드, port 0, 비정규 SHA256 fingerprint, relative/traversal remote root, 빈 인증 자료는 생성 단계에서 거부한다.
- EXECUTION: strict pinned host-key의 새 SSH/SFTP session으로 delete Work마다 `remove_file`을 수행한다. 상대 key만 remote root 아래에 결합하고 SFTP에 없는 object version은 terminal로 차단한다. fake backend를 공개 production 우회로 만들지 않고 module-local contract로만 주입했다.
- CLASSIFICATION: SFTP `NoSuchFile`은 idempotent success, `NoConnection`/`ConnectionLost`/`Failure`/timeout/limit/I/O는 bounded retry, PermissionDenied/unsupported/bad protocol은 terminal이다. SSH connection timeout·disconnect 계열만 retry하고 unknown host key/auth/key/config 문제는 terminal로 남긴다.
- VERIFY: module unit 10 passed와 delete config/identity integration 2 passed, reader path integration 1 passed/0 failed였으며 기존 외부 sshd live test 2개만 ignored였다. SSH all-target clippy `-D warnings`, workspace fmt check, `git diff --check`를 통과했다.
- NEXT: Section-05 quality/acceptance 항목을 실제 코드와 다시 대조해 durable retention execution이 adapter 선택과 secret role까지 end-to-end 배선됐는지 검증하고 남은 단일 결손을 닫는다.

#### Cycle 77 — exact-object retention authorization의 durable binding

- ISSUE: retention Operation에는 dry-run digest와 Recovery Point/Destination만 저장되어 승인 당시의 정확한 `(key, version, role)` 집합과 안전성 평가를 재시작한 Worker가 복원할 수 없었다. tombstone 시각을 소비 시각으로 사용하고 과거 tombstone을 거부하는 시간 조건도 실제 grace lifecycle과 반대였다.
- RED: confirmed authorization JSON round-trip과 object-version tamper 거부 테스트가 공개 durable authorization 타입 부재로 compile-fail했다. Catalog 테스트는 새 authorization 기반 enqueue 생성자 부재로 compile-fail했으며, 동일 물리 객체를 서로 다른 role로 중복 승인하는 테스트는 기존 인접 비교를 우회해 실제 실패했다.
- AUTHORIZATION: `RetentionDeleteAuthorizationV2`가 schema version, 전체 safety input/decision, 정렬된 object identity/role 집합과 canonical BLAKE3 digest를 직렬화한다. 로드 시 eligibility와 exact digest를 재검증하고 token의 digest/Recovery Point/Destination에 다시 결합해야만 `ConfirmedRetentionDeleteV2`를 복원한다. 동일 key/version의 role 중복은 전역 identity 집합 검사로 거부한다.
- DURABILITY: `RetentionDeleteSpec`은 임의 문자열 대신 검증된 authorization만 받는다. IPC transaction은 authorization 전체를 `operation_runs.request_json.authorization`에 구조화 JSON으로 저장하고, top-level binding과 confirmation token을 같은 digest/Recovery Point/Destination으로 소비한다. 과거 tombstone은 허용하되 미래 tombstone은 거부하며 token 소비, Operation/Work 생성, ledger 완료 시각은 요청 수락 시각을 사용한다.
- VERIFY: pipeline retention 8 passed/0 failed, IPC confirmation 4 passed/0 failed, pipeline package 20 passed/0 failed, catalog package 전체 통과, 두 패키지 all-target clippy `-D warnings`, workspace fmt check, `git diff --check`를 통과했다.
- NEXT: claimed retention Work에서 durable authorization을 읽고 Destination별 delete-only operator를 resolve해 실행한 뒤 fenced catalog settlement까지 연결하는 실제 Worker를 구현한다.

#### Cycle 78 — durable retention Worker와 객체별 destructive fence

- ISSUE: durable authorization, S3/SSH/Local delete operator, catalog settlement가 각각 존재했지만 claimed Work를 authorization load→operator resolve→exact delete→fenced settlement으로 연결하는 실행 경로가 없었다.
- RED: 서비스 재시작 뒤 큐에서 claim한 Work가 persisted authorization의 exact version 집합을 Manifest-first로 삭제하고 Catalog/Run/Work를 함께 성공시키는 통합 테스트가 Worker/resolver API 부재로 compile-fail했다. 객체마다 heartbeat를 요구한 확장 RED는 lease config 인자 부재로 compile-fail했다.
- LOAD BINDING: Catalog가 current claim/fence/lease, Operation kind/subject/state, structured authorization, top-level digest/RP/Destination/tombstone, consumed confirmation token, succeeded IPC ledger와 이 Work Item 결과 참조를 한 transaction에서 재검증한 뒤 confirmed delete context만 반환한다.
- WORKER: `RetentionDeleteWorkerV2`가 delete-only operator resolver를 통해 Destination authority를 얻고 exact authorized object set을 실행한 뒤 기존 atomic settlement에 report를 전달한다. credential/config resolution 실패도 모든 승인 객체에 retryable/terminal observation으로 투영할 수 있어 Work가 무응답 running 상태로 남지 않는다.
- FENCE: 각 원격 delete 직전에 `renew_destructive_step`이 current attempt/fencing token/lease와 cancellation을 `BEGIN IMMEDIATE`에서 확인하고 lease를 연장한다. cancellation이 Manifest 삭제와 payload 삭제 사이에 도착한 통합 테스트에서 payload 요청은 Destination에 도달하지 않고 Manifest만 deleted projection, Work/Attempt/Operation은 cancelled로 정산됐다.
- VERIFY: retention Worker integration 2 passed/0 failed, runtime package 20 passed/0 failed, catalog package 전체 통과, IPC confirmation 4 및 retention settlement 5 passed/0 failed, runtime/catalog/pipeline all-target clippy `-D warnings`, workspace fmt check, `git diff --check`를 통과했다.
- NEXT: production resolver가 Destination record와 role별 Secret Store binding을 읽어 Local/S3/SSH delete operator만 생성하도록 service/runtime composition에 연결하고 writer/reader/delete credential 분리를 실제 fixture로 증명한다.

#### Cycle 79 — delete-only credential resolver와 승인 시점 Secret version pin

- ISSUE: production retention Worker가 Destination record와 Secret Store를 읽어 delete operator를 만드는 resolver가 없었고, 실행 시점의 current binding을 사용하면 자격증명 회전이 이미 승인·큐 등록된 삭제의 권한 의미를 바꿀 수 있었다.
- RED: Local/S3/SSH resolver 및 role binding API를 요구하는 Windows integration test가 공개 타입 부재로 compile-fail했다. 이어 두 Secret Ref 버전의 승인 digest가 같다는 RED가 승인 문서에 credential version이 빠져 있음을 증명했다.
- CATALOG: `DestinationSpec`의 S3/SSH 생성·검증, `DestinationSecretRoleV2`, role별 binding과 delete binding 조회를 추가했다. Destination fingerprint는 지원 kind와 canonical Destination UUID로 재구성하며 삭제 권한은 `delete_operator` role만 조회한다.
- RESOLVER: `ServiceRetentionDeleteResolverV2`가 Catalog의 Destination metadata와 DPAPI Secret Store의 정확한 pinned Secret Ref를 읽어 Local/S3/SSH delete operator만 생성한다. Secret purpose도 `RetentionDelete`로 검사하므로 writer/reader credential 폴백은 불가능하고, SSH host-key pinning과 S3/SSH strict endpoint parsing을 유지한다.
- VERSION PIN: delete Secret Ref UUID를 canonical authorization digest, confirmation token 의미, Operation request와 claimed Work에 함께 저장한다. Worker는 current binding이 아니라 이 승인 시점 버전을 resolver에 전달하며, current binding 회전 뒤 retired old version도 해당 Work에서 복호화되고 임의 writer Secret Ref는 purpose mismatch로 거부된다.
- SECRET HYGIENE: SSH에 이어 S3 delete config와 admin credential 문자열도 `Zeroizing<String>`으로 보관하고 drop 시 원문을 지운다.
- VERIFY: pipeline retention 9 passed/0 failed, S3 unit 15/delete integration 5/reader integration 1 passed/0 failed(외부 live 2 ignored), service 전체, runtime 전체, catalog 전체가 통과했다. pipeline/S3/service/runtime/catalog all-target clippy `-D warnings`, workspace fmt check, `git diff --check`도 통과했다.
- NEXT: Section-05 acceptance checklist를 계획 문서와 다시 대조해 R05 증명 여부를 판정하고, 남아 있는 첫 단일 결손을 RED로 고정한다.

#### Cycle 80 — Catalog-backed retention tombstone과 실제 grace

- ISSUE: delete authorization의 `tombstoned_at_ms`가 Catalog에 먼저 기록된 사실과 연결되지 않아 호출자가 과거 시각을 넣어 grace를 우회할 수 있었다. tombstone 뒤 Stored Object 집합이 바뀌어도 기존 승인 객체만 삭제 Work로 enqueue할 수 있었다.
- RED: Catalog tombstone 없이 과거 시각 authorization을 제출하거나 실제 grace 종료 전 제출하는 테스트가 tombstone API 부재로 compile-fail했다. tombstone 뒤 verification object를 추가한 확장 RED는 기존 enqueue가 그대로 성공해 객체 drift 결손을 재현했다.
- TOMBSTONE: 완전하고 서명이 유효한 Recovery Point의 committed/degraded Replica에서 현재 Stored Object `(key, version, role)` 집합을 Catalog가 직접 읽고 정렬해 `retention_delete` Operation의 `created` 상태에 durable tombstone으로 기록한다. 같은 ID의 replay는 객체 집합·시각·grace·대상과 Work 부재가 모두 같을 때만 멱등이다.
- ENQUEUE GATE: 동일 `BEGIN IMMEDIATE` transaction에서 staged Operation/subject/request/state/created time, 현재 Catalog 객체 집합, authorization 객체 집합, tombstone/grace, authorization 평가 시각과 실제 IPC 수락 시각을 대조한다. 실제 `tombstone + grace` 전이거나 어떤 객체 drift라도 있으면 confirmation/ledger/Work mutation 전에 `RETENTION_TOMBSTONE_INVALID`로 거부한다.
- AUDIT: 검증된 tombstone 원문을 최종 Operation request에도 중첩 보존하고 기존 Operation을 `created→queued`로 전환한다. 클라이언트가 새 Operation을 바로 삽입하는 경로는 제거됐다.
- VERIFY: IPC confirmation integration 7 passed/0 failed, runtime retention Worker 2 passed/0 failed, catalog/runtime/pipeline package 전체가 통과했다. catalog/runtime all-target clippy `-D warnings`, workspace fmt check, `git diff --check`도 통과했다.
- NEXT: Section-05 acceptance의 `repair/rebuild/retention durable Operation`을 다시 대조해 enqueue만 존재하는 Replica Repair의 실제 claimed Work 실행·정산 결손을 RED로 고정한다.

#### Cycle 81 — durable Replica Repair Worker와 원자적 compliance 회복

- ISSUE: Replica Repair는 deterministic Operation/Work enqueue와 독립 pipeline만 존재했고, claimed Work를 source/target resolve→기존 encoded object 복사→Catalog/Operation 정산으로 잇는 실행 경로가 없었다. source unavailable 뒤 retry budget과 기존 Snapshot 불변도 입증되지 않았다.
- RED: claim된 repair가 재덤프 없이 Local source의 signed Manifest/payload를 target에 manifest-last로 복사하고 degraded/non-compliant를 healthy/compliant로 전환하는 통합 테스트가 Worker/resolver API 부재로 compile-fail했다. retryable resolver failure 3회 시나리오도 같은 경계 부재로 compile-fail했다.
- LOAD/WORKER: Catalog가 current fence/lease, `replica_repair` Operation request, complete+valid-signature Recovery Point, eligible source Replica의 required payload/set Manifest, repairable target 상태를 재검증한다. `ReplicaRepairWorkerV2`는 async resolver 결과를 pipeline에 전달하고 heartbeat 뒤 실행하며 retryable resolution/publish 오류를 bounded Work retry로 정산한다.
- ATOMIC COMMIT: 성공 정산은 signed Manifest와 pinned verifying key, persisted Manifest checksum/key/time, Part 집합, source/target membership, target requirement, reader round-trip outcome과 receipt byte/version을 다시 확인한다. 같은 transaction에서 target object projection을 실제 새 receipts로 교체하고 Replica를 committed/eligible로 바꾼 뒤 4축 Snapshot, Operation, Work, Attempt를 함께 성공시킨다.
- PROVIDER CONTRACT: 진단 중 `UploadReceipt.key`는 Local 절대 경로, S3 prefix 포함 key, SSH remote path인 물리 locator이고 Manifest key는 provider-neutral logical identity임을 확인했다. 기존 verified publish 경계와 동일하게 logical key는 signed Manifest에서 취하고 receipt는 byte/version 증거로만 사용해 잘못된 동등성 가정을 제거했다.
- FAILURE TRUTH: source resolver가 계속 unavailable이면 두 Attempt는 delayed/requeued, 세 번째는 budget exhausted/failed terminal이 되며 target object 0건과 기존 degraded/non-compliant Snapshot이 그대로 유지된다.
- VERIFY: Replica Repair Worker integration 2 passed/0 failed, pipeline 21 passed/0 failed, catalog 전체, runtime 전체가 통과했다. pipeline/catalog/runtime all-target clippy `-D warnings`, workspace fmt check, `git diff --check`도 통과했다.
- NEXT: production Repair resolver가 signed Manifest source, writer/reader role별 Secret Ref와 pinned signing key를 구성하도록 service composition에 연결하고 credential 분리·rotation을 실제 fixture로 검증한다.

#### Cycle 82 — production Replica Repair resolver와 immutable credential/key pin

- ISSUE: claimed repair는 source/target fingerprint만 보유해 resolver가 현재 Destination binding을 읽으면 rotation 뒤 대기 중 Work의 실행 자격증명이 바뀔 수 있었다. Catalog가 확정한 source Manifest key/checksum/signing-key ID를 실제 source reader와 결합하는 production service resolver도 없었다.
- RED: Windows DPAPI fixture에서 Local source→S3 target repair를 enqueue한 뒤 writer/reader를 rotation하고 현재 version을 revoke해도 이전 pinned version으로 resolve해야 하며, reader Ref를 writer slot에 넣으면 fallback 없이 purpose mismatch가 나야 하는 테스트가 resolver/module, role Ref capture, claimed Manifest identity API 부재로 compile-fail했다.
- PINNED WORK: `ReplicaRepairSecretRefsV2`가 active source-reader/target-writer/target-reader binding을 한 transaction에서 purpose까지 검증해 캡처한다. repair request schema v2는 세 Secret Ref를 저장하고 stable Operation/Work ID에도 포함하므로 rotation 전후 요청은 서로 다른 immutable 작업이며, request 변조는 deterministic Operation ID 재검증에서 거부된다. schema v1 local Work는 읽기 호환한다.
- CATALOG TRUTH: claim load는 complete+valid Recovery Point, eligible source의 required payload와 유일한 present set Manifest, repairable target을 한 transaction에서 재검증하고 source Manifest object key, persisted BLAKE3 checksum, signing-key ID를 `ClaimedReplicaRepairV2`에 결합한다.
- SERVICE RESOLVER: `ServiceReplicaRepairResolverV2`가 Local/S3/SSH endpoint를 구성하고 remote source는 RestoreReader, target writer는 BackupWriter, post-write probe는 별도 RestoreReader Secret Ref만 resolve한다. source bytes는 4 MiB 상한, Catalog checksum, RP ID, key ID, trusted public-key ring의 서명을 모두 통과해야 `ResolvedReplicaRepairV2`가 생성된다.
- ROTATION/FAIL-CLOSED: queued Work는 retired pinned v1을 계속 사용하고 revoked current v2로 fallback하지 않는다. 새 Work는 revoked current binding을 캡처하지 못하며, writer/reader substitution과 잘못된 pinned public key는 terminal error로 닫힌다.
- VERIFY: service production resolver 2 passed/0 failed, runtime repair Worker 2 passed/0 failed, catalog runtime repository 5 passed/0 failed, catalog/runtime/service package 전체가 통과했다. 세 package all-target clippy `-D warnings`, workspace fmt check, `git diff --check`도 통과했다.
- NEXT: Section-05 retention dry-run의 pinned/hold/last-compliant/active restore·repair/lock 입력이 호출자 boolean이 아니라 Catalog와 durable runtime에서 계산되는 authoritative application use case를 RED로 고정한다.

#### Cycle 83 — Catalog-authoritative retention dry-run

- ISSUE: 순수 retention evaluator는 `policy_expired`, pin/hold, last-compliant, active restore/repair, lock, tombstone과 삭제 object 집합을 호출자에게서 그대로 받아 안전 판정을 위조할 수 있었다. 서비스에는 이 입력을 한 시점의 Catalog/control/durable runtime에서 계산하는 use-case 경계가 없었다.
- RED: 공개 API가 Recovery Point, Destination fingerprint, 평가 시각, grace만 받고 GFS 만료, persisted pin/hold, 마지막 compliant RP, active Restore Run, queued Repair Operation, Object Lock 증거, tombstone grace, exact Stored Object 집합을 차례로 판정하는 통합 테스트를 먼저 추가했다. `RetentionSafetyControlSpec`, `OperationalStore::retention_planner`, plan decision/authorization 조회 API 부재로 compile-fail을 확인했다.
- CONTROL AUTHORITY: schema 변경 없이 control backup과 Catalog rebuild를 견디는 allowlisted `app_meta` document에 RP/Destination별 pin/legal-hold를 저장한다. document는 schema/version/subject/timestamp를 재검증하고 stale write와 같은 시각의 상충 write를 거부한다. dry-run 호출자는 safety boolean이나 object 목록을 전달할 수 없다.
- POLICY: complete+valid Recovery Point의 origin Backup Run, published immutable Plan Revision, exact destination membership/override와 IANA timezone을 한 transaction에서 결합한다. legacy `daily/weekly/monthly` GFS는 local day/ISO week/month별 최신 세대를 union하고 0-policy, 과대 세대 수, timezone/JSON/origin 증거 누락을 `RETENTION_AUTHORITY_INVALID`로 fail-closed한다. Asia/Seoul의 UTC 월 경계 fixture로 local-month 선택도 고정했다.
- RUNTIME SAFETY: 같은 snapshot에서 Snapshot protection projection을 읽어 source target 전체의 마지막 compliant RP를 보호하고, active Restore Run의 exact/unspecified source Replica와 schema-v1/v2 durable Repair Operation의 source fingerprint를 계산한다. malformed active repair request는 삭제 가능으로 간주하지 않고 fail-closed한다. Replica의 governance/compliance/server-append-only와 retention-until 증거도 Catalog 값만 사용한다.
- TOMBSTONE/CREDENTIAL/OBJECTS: staged retention Operation의 schema, RP/Destination, grace, 시각, 정렬된 `(key, version, role)` 집합을 현재 present Catalog objects와 다시 대조한다. remote destination은 active `retention_delete` Secret Ref만 authorization에 고정하고 Local은 Secret Ref 부재를 요구한다. `ServiceRetentionPlannerV2`가 이 repository를 서비스 application 경계로 노출한다.
- VERIFY: authoritative matrix integration 2 passed/0 failed, IANA GFS unit 1 passed/0 failed. `cargo test -p dbbackup-pipeline -p dbbackup-catalog -p dbbackup-service` 전체 통과, 세 package all-target clippy `-D warnings`, workspace fmt check, scoped `git diff --check` 통과. Windows MSVC의 기존 `linker_messages` 경고만 있었고 test failure는 0이었다.
- NEXT: service `ApplicationUseCases`/local IPC가 health/schema 조회만 노출하는 현재 경계를 RED로 고정하고, authoritative retention dry-run→pin/hold control→tombstone/confirmation 요청을 권한·idempotency ledger에 연결한다.

#### Cycle 84 — read-only retention dry-run application/IPC 경계

- ISSUE: `ApplicationCommand`와 `ServiceApplication`이 health/schema 조회만 실행해, Cycle 83의 authoritative retention planner를 로컬 IPC 클라이언트가 호출할 수 없었다. 호출자가 safety boolean이나 object 집합을 주입하지 못하고, read-only 요청이 idempotency ledger를 쓰지 않는 경계도 실제 frame으로 검증되지 않았다.
- RED: `retention.dry_run` command contract와 실제 인코딩·권한검사·dispatcher·Catalog fixture 통합 테스트를 먼저 추가했다. `ApplicationCommand::RetentionDryRun` 부재 compile-fail을 확인한 뒤, 최초 GREEN 시도에서 비실행 계획을 실행 권한서의 `to_json()`으로 직렬화해 `APPLICATION_INTERNAL`이 발생하는 설계 결함도 재현했다.
- COMMAND/IPC: `retention.dry_run`을 `Read/ReadOnly`, confirmation 없음, G1 불필요로 등록했다. owner SID/session으로 승인된 실제 request frame이 durable dispatcher를 통과하며, 성공·실패 모두 `ipc_request_ledger` row가 0건임을 확인했다.
- APPLICATION: payload를 `recoveryPointId`, `destinationFingerprint`, `gracePeriodMs` 세 필드로 deny-unknown 역직렬화하고 server clock과 `OperationalStore::retention_planner()`만 사용한다. 결과는 disposition/reasons/compliance impact/exact object set/canonical digest를 schema-versioned 응답으로 반환하며 저장소 상세 오류는 redacted `APPLICATION_INTERNAL`로 닫는다.
- AUTHORITY SEPARATION: `RetentionDeletePlanV2::preview_json()`을 추가해 `TombstoneRequired`/`WaitForGrace` 계획은 표시할 수 있게 하되, `RetentionDeleteAuthorizationV2::from_json()`과 `confirm()`의 `DeleteEligible` 검증은 완화하지 않았다. preview를 실행 권한으로 재사용하면 `RETENTION_DELETE_NOT_ELIGIBLE`로 거부되는 RED/GREEN 테스트를 고정했다.
- INPUT SAFETY: 클라이언트가 `pinned`를 위조해 추가하면 `APPLICATION_PAYLOAD_INVALID`로 거부되고 Catalog·ledger mutation은 0건이다.
- VERIFY: targeted command/application/IPC 4 passed/0 failed, preview authority-separation 1 passed/0 failed. `cargo test -p dbbackup-application -p dbbackup-pipeline -p dbbackup-catalog -p dbbackup-service`, 4-package all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 통과했다. Windows MSVC의 기존 `linker_messages` 경고 외 test failure는 0건이다.
- NEXT: pin/legal-hold control 변경을 admin typed command로 추가하고, control document 변경과 IPC idempotency ledger 완료를 하나의 Catalog transaction에서 원자화한다. tombstone/confirmation은 별도 Cycle로 유지한다.

#### Cycle 85 — atomic retention pin/legal-hold control IPC

- ISSUE: 일반 mutate dispatcher는 IPC ledger를 먼저 `running`으로 commit하고 application 변경과 terminal completion을 별도 transaction에서 수행한다. 이 경로로 pin/legal-hold를 바꾸면 crash나 completion 실패 때 control 문서와 ledger가 서로 다른 상태로 남을 수 있었다.
- RED: admin `retention.control_set` command contract와 실제 authorized frame 통합 테스트를 먼저 추가해 enum 부재 compile-fail을 확인했다. 테스트는 성공, 동일 key replay, 같은 key/다른 payload 충돌, caller `updatedAtMs` 위조 거부를 요구하고, SQLite trigger로 ledger completion만 강제 실패시켜 control/ledger 동시 rollback과 안전한 재시도까지 고정했다.
- COMMAND: `retention.control_set`을 `Admin/Mutate`, confirmation 없음, G1 불필요로 등록했다. payload는 RP/Destination/pinned/legalHold 네 필드만 deny-unknown으로 허용하며 control 시각은 client payload/`issuedAt`이 아니라 server authorization 시각으로 결정한다.
- ATOMIC TRANSACTION: `IpcRepository::set_retention_safety_control`이 command/SID/idempotency key/payload digest를 검증하고, ledger `accepted` insert→Catalog replica 확인 및 typed `app_meta` control upsert→stored success response와 terminal ledger completion을 하나의 `BEGIN IMMEDIATE`에서 처리한다. 완료 단계가 실패하면 control과 ledger row가 모두 롤백된다.
- REPLAY/CONFLICT: `(client SID hash, command, idempotency key)` replay는 저장된 success response만 반환하고 handler/control을 재실행하지 않는다. digest가 다른 payload는 `IPC_IDEMPOTENCY_CONFLICT`, 불완전하거나 비정상 stored row는 `IPC_REQUEST_INCOMPLETE`로 fail-closed한다.
- BYPASS HARDENING: production service wrapper의 직접 `set_safety_control` mutation API를 제거했다. 서비스에서 control을 바꾸는 노출 경로는 권한검사와 typed IPC transaction뿐이며, Catalog의 저수준 setter는 repository 테스트/setup 경계에 남겼다.
- VERIFY: command/application targeted 5 passed/0 failed, completion-failure injection matrix 1 passed/0 failed. `cargo test -p dbbackup-application -p dbbackup-pipeline -p dbbackup-catalog -p dbbackup-service`, 4-package all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 통과했다.
- NEXT: retention tombstone 생성을 별도 admin typed command와 atomic IPC ledger transaction으로 노출하고, Catalog가 server-accepted 시각·exact present object 집합·grace를 직접 고정하도록 한다. confirmation/delete enqueue는 다음 Cycle로 분리한다.

#### Cycle 86 — authoritative retention tombstone typed transaction

- ISSUE: 저수준 `stage_retention_tombstone`은 caller가 Operation ID와 시각을 제공하고 IPC ledger 없이 별도 commit했다. 새 IPC 경로가 이를 그대로 호출하면 pin/hold/정책/active source/lock 상태가 바뀐 뒤에도 tombstone을 만들 수 있고, client `issuedAt`으로 grace를 앞당기거나 Operation만 남기는 반쪽 commit이 가능했다.
- RED: admin `retention.tombstone` command와 실제 frame 통합 테스트를 먼저 추가해 enum 부재 compile-fail을 확인했다. pinned candidate 거부, caller timestamp/object 필드 거부, server 수락 시각, exact present object snapshot, replay/conflict, completion failure 주입 시 Operation+ledger 0건, grace 경계의 Wait/DeleteEligible 전이를 요구했다.
- SERVER TIME: `AuthorizedRequest`가 `authorize_and_decode`에 전달된 실제 server `now_ms`를 `accepted_at_ms`로 보존한다. 일반 mutate ledger와 control/tombstone typed transaction도 client `issuedAt` 대신 이 값을 사용하며, 테스트는 둘이 다른 frame에서 저장 시각이 server 값임을 단언한다.
- AUTHORITY GATE: authoritative planner 계산을 transaction 내부 helper로 추출했다. tombstone transaction은 published policy, pin/hold, last compliant, active restore/repair, Object Lock, delete credential과 현재 Catalog objects를 같은 snapshot에서 다시 계산하고 disposition이 정확히 `TombstoneRequired`일 때만 진행한다.
- ATOMIC STAGE: `IpcRepository::stage_retention_tombstone_request`가 ledger accepted insert→authority gate→server-generated Operation Run과 typed tombstone document→resource-bound stored response/ledger success를 하나의 `BEGIN IMMEDIATE`에서 처리한다. response resource와 ledger `operation_run` ref도 replay 시 상호 검증한다.
- OBJECT/GRACE TRUTH: tombstone은 caller object 목록을 받지 않고 해당 Replica의 `state='present'` 객체만 `(key, version, role)` 순서로 고정한다. `tombstoned_at_ms`와 `eligibleAfterMs`는 server accepted time과 checked grace 합으로 계산하며, 같은 key의 다른 grace는 idempotency conflict다.
- VERIFY: command/application targeted 6 passed/0 failed, tombstone authority/atomicity matrix 1 passed/0 failed. `cargo test -p dbbackup-application -p dbbackup-pipeline -p dbbackup-catalog -p dbbackup-service`, 4-package all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 통과했다.
- NEXT: one-time confirmation prepare를 실제 IPC command로 노출하고, grace 종료 후 authoritative eligible authorization과 SID/action/RP/Destination/digest를 결합한 `retention.delete` typed enqueue를 end-to-end로 연결한다.

#### Cycle 87 — authoritative one-time retention confirmation IPC

- ISSUE: 기존 `confirmation.prepare` 서비스는 임의 payload digest를 별도 transaction에 저장할 수 있었고, retention 삭제 권한을 현재 Catalog 상태에서 계산하지 않았다. 원문 토큰을 일반 IPC replay 응답에 저장하면 digest-only custody 계약도 깨지며, 이전 dry-run의 시간 의존 digest를 다음 요청에 다시 제출하게 하는 계약은 실제 호출이 불가능했다.
- RED: `confirmation_prepare_is_atomic_and_never_persists_or_replays_the_raw_token`을 먼저 추가해 typed command 부재를 `APPLICATION_COMMAND_UNKNOWN`으로 확인했고, 이어 payload에서 caller digest/resource 내부 필드를 제거한 뒤 grace 종료 전 authoritative prepare가 `APPLICATION_PAYLOAD_INVALID`로 실패하는 강화 RED를 고정했다.
- AUTHORITY: `confirmation.prepare`는 action, Recovery Point, Destination, grace, TTL만 받는다. Catalog의 `BEGIN IMMEDIATE` 안에서 server accepted time 기준 retention plan을 다시 계산하고 정확히 `DeleteEligible`일 때만 해당 authorization digest를 SID/action/RP/Destination에 결합한다.
- TOKEN CUSTODY: 32-byte 원문 토큰은 서비스 메모리에서 생성되어 최초 성공 응답에만 포함된다. Catalog에는 BLAKE3 digest와 raw-token 필드가 없는 typed replay document만 저장하며, 같은 idempotency key 재생은 동일 token ID와 authorization을 반환하되 `confirmationToken=null`, `tokenAvailable=false`로 고정했다.
- ATOMICITY/FAIL-CLOSED: ledger accepted, current authority 계산, confirmation token digest insert, redacted replay document, ledger succeeded가 하나의 transaction이다. ledger completion 실패 trigger에서 confirmation/ledger가 모두 rollback되고, grace 이전 요청·caller token 주입·payload conflict는 token/work를 남기지 않는다.
- VERIFY: focused RED→GREEN 1 passed, retention application 4 passed, command contract 3 passed, 기존 confirmation/delete repository 7 passed. `cargo test -p dbbackup-application -p dbbackup-pipeline -p dbbackup-catalog -p dbbackup-service`, 4-package all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 통과했다. Windows MSVC의 기존 `linker_messages` 경고 외 test failure는 0건이었다.
- NEXT: `retention.delete` typed IPC를 추가해 one-time token을 consume하고, 삭제 수락 시점의 authoritative plan을 다시 계산해 confirmation 시점 authorization과 동일 대상임을 검증한 뒤 Operation/Work/ledger를 하나의 transaction으로 enqueue한다.

#### Cycle 88 — authoritative retention delete enqueue

- ISSUE: 기존 delete repository 계약은 실제 IPC payload digest와 confirmation authorization digest가 같다고 가정했고 confirmation 시각과 delete 수락 시각도 동일해야 했다. 따라서 원문 token을 포함한 현실적인 typed request를 처리할 수 없었고, confirmation 이후 Catalog 안전 상태가 바뀌어도 enqueue 직전 재승인할 경계가 없었다.
- RED: 실제 시각이 다른 `confirmation.prepare`→`retention.delete` frame을 먼저 연결해 typed branch 부재를 `IPC_TYPED_TRANSACTION_REQUIRED`로 확인했다. 별도 generic confirmation 테스트는 `RetentionDelete` token을 우회 발급할 수 있음을 재현했고, current pin 변경 거부·completion failure rollback·one-time reuse 거부를 수락 조건으로 고정했다.
- CURRENT AUTHORITY: delete transaction이 server accepted time에 published policy, control, active work, immutability, credential과 present object 집합을 다시 읽고 정확히 `DeleteEligible`인지 확인한다. confirmation authorization과 current authorization은 평가 시각만 달라질 수 있고 나머지 delete scope 전체가 일치해야 한다.
- TOKEN/ATOMICITY: typed payload의 raw token은 즉시 BLAKE3 digest로 비교하며 DB·WAL·operation document에 저장하지 않는다. ledger accepted, one-time token consume, Operation created→queued, Work queued, stored response와 ledger success가 하나의 `BEGIN IMMEDIATE`에서 commit되고, completion trigger 실패 시 전부 rollback된다.
- DURABLE EVIDENCE: operation request schema v2가 confirmation authorization/digest, delete 시점 current authorization/digest, 실제 request payload digest, server `authorized_at_ms`, tombstone과 delete SecretRef를 서로 분리해 보존한다. runtime claim/settlement도 세 digest, consumed token, ledger-work binding과 stable scope를 다시 검증한다.
- REPLAY/BYPASS: 동일 idempotency replay는 current state를 재평가하지 않고 최초 저장 응답과 Work binding만 반환한다. 다른 key의 token 재사용은 uniform `CONFIRMATION_TOKEN_INVALID_OR_REPLAYED`로 거부하며 generic `ConfirmationService`는 retention delete 발급을 `CONFIRMATION_INPUT_INVALID`로 차단한다.
- VERIFY: command contract 3, pipeline retention 11, Catalog confirmation 7/delete 5, runtime worker 2, service confirmation 3/retention application 4/durable dispatch 2, ledger 1 테스트가 각각 통과했다. 최종 `cargo test -q` 5-package 전체 회귀, 5-package all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 exit 0이었다. Windows MSVC의 기존 경로 안내 `linker_messages` 외 test failure는 0건이었다.
- NEXT: section-05 acceptance matrix를 현재 revision에 다시 대조해 signed multi-part Recovery Point, replica durability/compliance, repair, retention, L1 reader 중 아직 외부 증거가 없는 가장 작은 경계를 RED로 고정한다.

#### Cycle 89 — forged complete Recovery Point 입력 경계 제거

- ISSUE: `CompleteRecoveryPointSpec::new_verified`와 사후 builder가 외부 crate에 공개되어 있었다. 호출자는 signed set Manifest나 pinned key를 전혀 검증하지 않고도 임의 Part/Replica/Object를 `signature_status='valid'` complete projection과 crash reconciliation 입력으로 만들 수 있었다.
- RED: 외부 호출자가 unchecked constructor로 임의 spec을 만드는 `compile_fail` doctest를 먼저 추가했다. 현 API에서는 코드가 정상 컴파일되어 “compile_fail인데 성공”으로 테스트가 실패했고 실제 공개 우회를 입증했다.
- FIX: unchecked constructor와 `with_required_part`/`with_origin`/`with_replica`/`with_present_object` 변조 builder를 전부 제거했다. 공개 생성 경로는 pinned Ed25519 signature, canonical set Manifest와 실제 `ReplicaPublishOutcomeV2`를 대조하는 `from_verified_publish` 하나만 남았다.
- FIXTURE MIGRATION: Catalog projection의 degraded/healthy/unreadable fixture와 runtime crash reconciliation fixture를 실제 schema-v2 Manifest 생성→Ed25519 서명→payload/Manifest receipt 관찰→pinned verification 경로로 바꿨다. incomplete fixture도 위조 spec을 repository에 넣지 않고 검증 factory 단계에서 `RECOVERY_POINT_INCOMPLETE`로 닫힌다.
- VERIFY: compile-fail doctest 1, Catalog projection 3, recovery commit crash 2가 통과했다. Catalog+runtime 전체 테스트, 두 package all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 exit 0이며 공개 unchecked 생성·변조 API 참조는 0건이다.
- NEXT: Section-05 Quality Gate 9개를 현재 revision의 정확한 test target에 매핑하고 한 번에 실행하는 fail-closed evidence gate를 추가해 V2-R05의 최종 proved 판정을 기계적으로 고정한다.

#### Cycle 90 — Section-05 current-tree evidence gate와 V2-R05 closure

- ISSUE: Section-05의 개별 기능 테스트는 존재했지만 아홉 Quality Gate와 정확한 test target의 대응, 동일 HEAD/source-tree/build 실행, 전체 필수 판정을 한 artifact로 증명하지 못했다. 일부 명령만 골라 실행하거나 ignored 결과를 성공으로 오인해도 R05를 수동으로 proved 처리할 여지가 있었다.
- RED: S05 requirements/matrix와 all-pass/one-fail collector fixture를 먼저 추가해 전용 collector 부재로 실패시켰다. 이어 CI에 contract test·collector·artifact upload가 없으면 실패하는 wiring RED도 확인했다.
- MATRIX: `s05-evidence-requirements.json`이 정확히 아홉 Quality Gate를 13개 필수 evidence ID에 매핑한다. signed boundary/Manifest Part, occurrence uniqueness, Replica truth, role-separated credential, crash rebuild, Catalog isolation, durable repair, retention policy·settlement·Worker·typed IPC, 실제 Reader L1을 각각 exact cargo target으로 고정했다.
- COLLECTOR: 기존 G1 collector를 gate ID/requirements/matrix/output 경로를 매개변수화하되 G1 기본 계약은 그대로 유지했다. S05 wrapper는 별도 PowerShell process로 exit code를 정확히 전달하고, 모든 명령을 끝까지 실행해 동일 revision/tree/build metadata와 pass/fail만 저장하며 raw stdout/stderr는 evidence JSON에 넣지 않는다.
- CI: Windows job이 S05 collector contract를 검증하고 `always()`로 실제 matrix를 전부 수집한 뒤 `artifacts/s05`를 14일 artifact로 업로드한다. 로컬 artifact 경로는 git ignore해 source tree를 오염시키지 않는다.
- VERIFY: S05 all-pass와 한 명령 exit 7 fixture, 기존 G1 all-pass/one-fail 회귀가 통과했다. 실제 S05 matrix는 13/13 pass, failure 0으로 `artifacts/s05/evidence.json`을 생성했고 doctest/Manifest 5/uniqueness 1/Replica 4/credential 2/crash 2/rebuild 3/repair 2/retention 11+5+2+4/L1 2가 모두 ignored 없이 실행됐다. `git diff --check`도 통과했다.
- STATE: V2-R05를 `proved`로 전환한다. 최종 전체 release 판정 전에는 이후 source 변경을 반영하도록 S05 collector를 다시 실행한다.
- NEXT: V2-R03 Section-03 service/IPC/Secret Quality Gate를 계획 문서와 현재 구현에 대조하고, 아직 missing인 첫 실행 가능 경계를 RED로 고정한다.

#### Cycle 91 — atomic one-way configuration cutover state

- ISSUE: service bootstrap이 항상 `legacy_direct/rehearsal_only`를 하드코딩했고 `config.cutover`는 typed transaction이 없어 실행될 수 없었다. schema의 `config_import_runs.cutover_marker`만 직접 쓰면 shadow diff, G1 build, scheduler freeze, service health, confirmation, IPC replay가 서로 무관한 채 service authority를 위조할 수 있었다.
- RED: `config_cutover_repository` 통합 테스트를 먼저 추가해 cutover spec, 영속 authority loader, typed atomic commit API 부재로 compile-fail을 확인했다. 정상 재시작/replay, 두 번째 marker 거부, token consume 뒤 late SQLite failure 전체 rollback, authority projection 변조 fail-closed를 수락 조건으로 고정했다.
- BINDING: `ConfigCutoverSpec`이 shadow import ID/checksum/relation digest, recovery bundle, legacy scheduler freeze, service health, G1 revision/source-tree/build/evidence digest, commit 시각을 canonical BLAKE3 marker 하나에 결합한다. 성공 shadow import는 skipped/ambiguous 0건과 `legacy_direct/cutoverPerformed=false`를 transaction 안에서 다시 검증한다.
- ATOMICITY: `IpcRepository::commit_config_cutover`가 ledger accepted, SID/action/import/marker/payload에 결합된 one-time confirmation consume, succeeded cutover audit row, `execution_authority.v1` projection, ledger success를 하나의 `BEGIN IMMEDIATE`에서 기록한다. 동일 idempotency replay만 최초 marker를 반환하고 다른 cutover는 token을 소비하지 않은 채 거부한다.
- FAIL-CLOSED: authority loader는 cutover row와 app_meta가 정확히 1:1인지 확인하고 canonical audit JSON, G1 binding, shadow import, confirmation consume, request ledger, terminal response와 시각을 모두 교차 검증한다. 빈 store만 legacy rehearsal로 해석하며 torn/forged/multiple state는 `CONFIG_CUTOVER_STATE_INVALID`로 거부한다.
- VERIFY: focused 3 passed/0 failed, Catalog 전체 unit/integration/doc tests가 모두 통과했다. catalog all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`도 exit 0이었다.
- NEXT: 실제 G1 evidence artifact와 현재 build identity를 검증하는 불투명 proof factory, recovery bundle/freeze/service-health 증명 문서를 추가한 뒤 `config.cutover` service typed IPC에 연결한다.

#### Cycle 92 — current-build G1 evidence proof

- ISSUE: Cycle 91의 `G1EvidenceBindingV1`은 검증 결과를 영속화할 수는 있었지만 caller가 revision/tree/build/evidence digest 문자열을 직접 만들 수 있었다. 따라서 stale artifact, 일부 명령 누락, 실패 행, command 바꿔치기나 raw output 포함 JSON도 service 경계에서 proof처럼 전달될 수 있었다.
- RED: 실제 G1 requirements/matrix로 완전한 evidence fixture를 만들고 `BuildIdentityV1`, `VerifiedG1EvidenceV1`, `G1EvidenceError` 부재 compile-fail을 확인했다. stale revision/tree/build, missing/failed/duplicate row, wrong command, durable stdout 필드를 모두 같은 fail-closed 오류로 거부하도록 고정했다.
- VERIFY FACTORY: verifier가 schema/gate/status/failure count, 정확히 9개 필수 ID와 순서, matrix의 exact display command/cargo/windows 계약, row별 exit/status/timestamp/duration, runner OS, revision/source-tree/build 일치를 전부 검사한다. unknown field를 거부하므로 stdout/stderr/environment 같은 원문 증거는 proof 입력에 들어올 수 없다.
- BUILD BINDING: `BuildIdentityV1`은 canonical 40/64자리 source revision과 64자리 source-tree/build digest만 허용한다. evidence top-level과 모든 row가 현재 identity 및 runner와 같아야만 private-field `VerifiedG1EvidenceV1`이 생성되며, 영속화용 digest는 검증된 typed document의 canonical JSON에서 다시 계산한다.
- VERIFY: focused 2 passed/0 failed, application 전체 5 passed/0 failed, application all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 exit 0이었다.
- NEXT: recovery bundle/freeze/service-health를 service-owned verifier로 확인하고, 검증된 G1 proof와 함께 one-time `confirmation.prepare`→`config.cutover` typed IPC transaction에 연결한다.

#### Cycle 93 — service-owned cutover readiness proof

- ISSUE: G1 proof만으로는 legacy 입력을 복원할 bundle이 실제 존재하는지, 기존 Task가 현재 비활성인지, cutover 직전 service가 정상 rehearsal 상태인지 증명할 수 없었다. client가 세 digest를 직접 제출하면 Cycle 91 marker의 나머지 필드가 여전히 위조 가능했다.
- RED: filesystem evidence root, live health probe, scheduler state probe를 함께 사용하는 readiness 통합 테스트를 먼저 추가해 service cutover 모듈 부재로 compile-fail을 확인했다. stale G1, bundle item 변조, enabled legacy task, stopped service, path escape를 모두 거부하도록 고정했다.
- RECOVERY BUNDLE: manifest는 legacy TOML, encrypted keystore, legacy Catalog, scheduler inventory 네 종류를 정확히 한 번씩 요구한다. 각 실제 파일의 경로 격리, 크기와 streaming BLAKE3를 다시 확인하고 scheduler inventory에는 원본 Task XML, 이전 enabled 상태, XML digest를 보존한다.
- FREEZE/LIVE STATE: freeze receipt가 bundle digest와 source checksum을 참조하고 inventory의 task/digest/before 상태와 정확히 일치하며 `afterEnabled=false`인지 검사한다. receipt만 신뢰하지 않고 service-owned `LegacySchedulerProbeV1`로 모든 Task가 현재도 disabled인지 재확인한다. live service는 schema 2/53 tables, rehearsal ready, legacy authority, claims 0, IPC ready여야 한다.
- SAFE INPUT: evidence와 bundle 경로는 canonical evidence root 아래의 상대 경로만 허용하며 탈출·누락·unknown JSON field·비정상 시각을 fail-closed 처리한다. proof에는 파일 내용이나 경로 대신 검증된 G1/bundle/freeze/live-health digest만 남긴다.
- CONFIRMATION STABILITY: readiness marker에서 server commit 시각을 제외했다. commit 시각은 transaction이 직접 기록하되 같은 frozen inputs의 prepare와 execute가 시간 차이 때문에 다른 confirmation fingerprint가 되지 않는다.
- VERIFY: readiness focused 2, Catalog cutover 3, application 5, service 전체 unit/integration 39개가 모두 통과했다. service+catalog all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 exit 0이었고 Windows MSVC의 기존 `linker_messages` 안내 외 실패는 없었다.
- NEXT: 이 불투명 readiness proof를 raw token 비저장 typed `confirmation.prepare`와 `config.cutover` dispatcher branch에 연결하고, prepare replay·payload drift·token 재사용·completion rollback을 end-to-end로 검증한다.

#### Cycle 94 — typed confirmation.prepare → config.cutover transaction

- ISSUE: `config.cutover` Catalog 커밋은 존재했지만 Service dispatcher가 destructive generic 경계에서 거부했고, `confirmation.prepare`는 retention 전용이었다. caller가 별도 confirmation row를 만들지 않고서는 검증된 readiness를 실제 one-way authority 전환에 사용할 수 없었다.
- RED: Catalog에 cutover confirmation 원자 발급·redacted replay·late completion rollback 테스트를 먼저 추가해 API 부재 compile-fail을 확인했다. 이어 실제 File readiness/ServiceHost/IPC frame을 사용하는 Service 테스트를 추가해 `with_cutover_verifier`와 typed branch 부재 compile-fail을 확인했다.
- PREPARE: Service가 G1·recovery bundle·scheduler freeze·live health를 검증한 뒤 서버 생성 256-bit token과 exact future `config.cutover` payload를 결합한다. Catalog는 prepare ledger, token digest, shadow import ID, readiness marker, future payload digest, redacted response를 하나의 `BEGIN IMMEDIATE`에서 기록한다. raw token과 실행 request는 최초 응답에만 존재하며 durable response/replay에는 저장·재노출하지 않는다.
- EXECUTE: `config.cutover`가 같은 파일·scheduler·health를 다시 검증하고 marker를 재계산한 뒤 exact SID/token/action/import/marker/payload binding을 소비한다. confirmation consume, cutover audit, authoritative app_meta, ledger completion은 기존 단일 transaction 안에서 함께 commit되며 같은 request replay만 deduplicated success가 된다.
- FAIL-CLOSED: prepare replay 중 새로 생성된 token 후보와 payload digest는 폐기하고 기존 token ID만 redacted 반환한다. bundle drift는 execute 전에 `CONFIG_CUTOVER_READINESS_INVALID`로 차단되어 token과 legacy authority를 보존한다. prepare/cutover completion SQLite trigger 실패는 ledger·token·authority 전체를 rollback한다.
- VERIFY: Catalog cutover 5/5, Service readiness/cutover 4/4, 기존 retention application 4/4가 통과했다. Service+Catalog 전체 unit/integration/doc tests, 두 package all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 exit 0이었고 Windows MSVC의 기존 `linker_messages` 안내 외 실패는 없었다.
- NEXT: Service bootstrap이 persisted authority projection을 유일한 실행 권위로 load하도록 연결하고, production 재시작의 claim enablement와 legacy direct 실행 차단, projection 손상 diagnostics-only, rollback/forward-fix 운영 경계를 검증한다.

#### Cycle 95 — persisted authority service bootstrap

- ISSUE: `HostConfig`가 caller 제공 `AuthoritySnapshot`을 그대로 신뢰했고 SCM은 항상 legacy rehearsal snapshot을 주입했다. 따라서 정상 cutover 뒤 재시작해도 service claim eligibility가 켜지지 않는 반면, 다른 caller는 빈 DB에 production authority를 임의 주입할 수 있었다.
- RED: 실제 shadow import→confirmation→cutover가 commit된 DB를 두 번 재시작해 production readiness를 요구하고, 빈 DB의 current-build G1 hint는 legacy rehearsal만 허용하며, authority projection 변조는 safe legacy diagnostics-only여야 한다는 Windows host 테스트를 먼저 추가해 새 bootstrap API 부재 compile-fail을 확인했다.
- AUTHORITY LOAD: `HostConfig`에서 execution authority/mode/marker 주입을 제거하고 pre-cutover current-build G1 boolean만 분리했다. `ServiceHost::start`는 Operational Store migration/diagnostics 뒤 `cutover().authority()`의 교차 검증된 projection만 `AuthoritySnapshot`으로 변환한다. persisted service/production/G1/marker 네 조건이 모두 참일 때만 `ProductionReady`와 claim eligibility가 활성화된다.
- FAIL-CLOSED: 빈 Store는 current-build G1 hint가 있어도 `legacy_direct/rehearsal_only`, claims disabled다. cutover row/app_meta/confirmation/ledger 중 하나라도 손상되면 service/production 값을 추정하지 않고 store를 durable dispatcher에 제공하지 않은 채 `CONFIG_CUTOVER_STATE_INVALID` diagnostics-only IPC shell로 내려간다.
- SCM: Windows SCM assembly는 더 이상 하드코딩 rehearsal snapshot을 dispatcher에 넘기지 않고, host가 영속 상태에서 도출한 동일 snapshot을 사용한다. 정상 cutover DB는 shutdown/restart를 반복해도 service authority와 production readiness를 유지한다.
- VERIFY: persisted bootstrap focused 3/3과 Service 전체 unit/integration/doc tests가 통과했다. Service all-target Clippy `-D warnings`, workspace fmt check, `git diff --check`가 모두 exit 0이었고 Windows MSVC의 기존 `linker_messages` 안내 외 실패는 없었다.
- NEXT: legacy CLI/Tauri/Task direct-run entry에 persisted authority guard를 공통 적용하고, marker 후 첫 v2 data 유무를 영속 판정해 rollback 가능 구간과 forward-fix-only 구간을 분리한다.

#### Cycle 96 — legacy direct-execution guard and monotonic recovery epoch

- ISSUE: persisted service authority가 활성화된 뒤에도 CLI/Tauri/Task의 기존 backup, verify, restore, schedule, prune 진입점이 직접 실행될 수 있었고, cutover 이후 최초 v2 쓰기가 발생했는지를 재시작 뒤에도 판정할 단조 상태가 없었다.
- RED: Catalog 통합 테스트를 먼저 추가해 cutover 직후 rollback-eligible, legacy guard 차단, 최초 IPC 성공 뒤 forward-fix-required, dirty epoch 역전 거부, 재시작 후 동일 판정을 요구했다. 구현 전 새 타입과 guard API 부재로 컴파일 실패를 확인했다.
- EPOCH: cutover transaction이 `production_epoch.v1=rollback_eligible`을 authority와 함께 기록한다. IPC terminal success와 occurrence/run/work/catalog-backup 등 핵심 운영 row의 최초 insert trigger가 같은 transaction 안에서 이를 `forward_fix_required:<cause>`로 한 번만 전이하며, SQLite trigger가 dirty 상태의 수정·삭제를 거부한다.
- FAIL-CLOSED: production authority에 epoch가 없거나 시각·형식이 authority audit과 모순되면 `CONFIG_CUTOVER_STATE_INVALID`로 거부한다. 정상 dirty epoch는 재시작 후에도 `ForwardFixRequired`로 복원된다.
- GUARD: 공통 CLI guard가 고정 ProgramData Operational Store에서 persisted authority를 확인한다. service authority이면 backup, L1 verify, restore, schedule 등록, prune 실행을 `LEGACY_DIRECT_EXECUTION_BLOCKED`로 차단하며 dry-run과 기존 Task 제거는 안전한 진단·철회 경로로 허용한다. Tauri는 동일 CLI sidecar를 사용하므로 같은 경계를 상속한다.
- VERIFY: `cargo test -p dbbackup-catalog -p dbbackup-service -p dbbackup-cli` 전체 통과(원격 실환경 fixture 2건만 명시적 ignored), `cargo clippy -p dbbackup-catalog -p dbbackup-service -p dbbackup-cli --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `git diff --check` 모두 exit 0.
- NEXT: clean epoch에서만 실행 가능한 원자적 rollback과 dirty epoch의 forward-fix 거부/runbook 경계를 구현한 뒤, real SCM cutover verifier wiring 및 section-03 증거 gate를 닫는다.

#### Cycle 97 — atomic clean rollback and forward-fix refusal

- ISSUE: Cycle 96은 cutover 이후 상태를 clean/dirty로 분류했지만 실제 authority rollback transaction이 없었다. DB 쓰기 일부만 성공하면 legacy Task와 service가 동시에 권한을 주장할 수 있고, rollback 재시도나 이후 재-cutover의 의미도 정의되지 않았다.
- RED: clean rollback의 authority/epoch 제거, audit 보존, idempotent replay, legacy guard 해제, 새 cutover 허용을 요구하는 통합 테스트를 먼저 추가했다. 별도 테스트가 첫 production IPC 완료 뒤 stable forward-fix 오류와 authority 유지, late audit-write 실패 때 전체 transaction rollback을 요구했으며 새 타입/API 부재로 컴파일 실패했다.
- TRANSACTION: `ConfigCutoverRepository::rollback_if_clean`은 `BEGIN IMMEDIATE` 안에서 canonical authority와 exact marker/recovery-bundle binding을 재검증한다. `rollback_eligible`일 때만 authority와 epoch를 제거하고 active cutover row를 `shadow/cancelled` rollback audit로 변환한다. audit에는 원 cutover 문서, rollback UUID, scheduler restore digest, 요청 SID digest, 사유와 시각을 보존한다.
- MONOTONICITY: `ForwardFixRequired`이면 `CONFIG_CUTOVER_FORWARD_FIX_REQUIRED`로 DB 변경 0건을 보장한다. marker/bundle/time mismatch는 precondition 오류, rollback UUID payload 재사용은 conflict이며, 같은 payload 재시도만 deduplicated result를 반환한다. 완료 뒤 새 confirmation과 새 marker로 재-cutover할 수 있다.
- ATOMICITY: authority/epoch 삭제 뒤 audit row 갱신에 주입한 SQLite 실패가 transaction 전체를 되돌려 service authority와 rollback-eligible epoch를 그대로 보존함을 검증했다.
- VERIFY: rollback focused 8/8, Catalog 전체 unit/integration/doc tests 통과, Catalog all-target Clippy `-D warnings`, workspace fmt check, `git diff --check` 모두 exit 0.
- NEXT: rollback repository를 owner/admin 전용 service recovery command와 scheduler restore verifier에 연결하고, real SCM의 cutover verifier 구성 및 section-03 Windows 증거 collector를 구현한다.

#### Cycle 98 — real SCM cutover verifier wiring and live authority refresh

- ISSUE: typed cutover는 통합 테스트에서 verifier를 수동 주입할 때만 동작했고 실제 SCM은 항상 verifier 없는 dispatcher를 만들었다. 더구나 cutover 후 dispatcher의 authority snapshot이 부팅 시점 legacy 값에 고정되어 같은 프로세스가 production 명령을 계속 rehearsal-only로 거부했다.
- RED: protected `service-identity.json`의 optional verifier 계약과 path escape 거부를 먼저 테스트해 getter 부재 compile-fail을 확인했다. typed cutover 뒤 같은 dispatcher의 backup route가 persisted service authority를 봐야 한다는 테스트는 실제로 `APPLICATION_REHEARSAL_ONLY`를 반환해 stale snapshot 결함을 재현했다.
- DEPLOYMENT CONTRACT: identity config의 `cutoverVerifier`가 ProgramData 설치 루트 하위 relative evidence directory와 canonical source revision/tree/build digest를 제공한다. 파일은 기존 exact protected DACL 검사를 그대로 통과해야 하며 unknown field, 절대/상위 경로, 잘못된 digest는 시작 전에 거부된다.
- SCM WIRING: 실제 SCM assembly가 embedded G1 requirements/matrix, protected build identity, service health probe, live Windows Task Scheduler probe로 `FileCutoverReadinessVerifierV1`를 구성해 dispatcher에 주입한다. 선언된 evidence root가 없거나 잘못되면 `CONFIG_CUTOVER_READINESS_INVALID`로 시작 실패하고 이미 획득한 singleton/host를 정리한다. verifier 미선언 설치는 진단 service로 시작하되 cutover typed branch는 fail-closed다.
- LIVE TASK PROBE: `schtasks.exe /Query /TN <exact task> /XML`을 shell 없이 호출하고 UTF-8/UTF-16 BOM을 명시적으로 해석한다. DBBackup namespace 외 task, 실패/stderr, 중복·모호한 Enabled 값은 상태를 추정하지 않고 `None`으로 거부한다.
- AUTHORITY REFRESH: durable dispatcher는 매 request마다 cross-validated persisted authority를 읽는다. pre-cutover에서는 protected build hint로 cutover만 허용하고, marker commit 직후에는 재시작 없이 service/production 권한을 관찰한다. 향후 clean rollback 뒤 static production snapshot이 남아도 persisted legacy가 우선되어 production 실행을 거부한다.
- VERIFY: Catalog+Service 전체 unit/integration/doc tests 통과, 두 package all-target Clippy `-D warnings`, workspace fmt check, `git diff --check` 모두 exit 0.
- NEXT: owner/admin 확인 토큰과 scheduler recovery evidence를 묶은 typed rollback service command를 구현하고 section-03 current-build evidence collector로 전체 fault matrix를 봉인한다.

#### Cycle 99 — Section 03 current-build evidence gate and zero-test false-green closure

- ISSUE: singleton, pipe ACL, replay, DPAPI, cutover 테스트는 개별 통과했지만 같은 source revision/tree/build에 묶인 Section 03 all-required 증거가 없었다. 또한 `cargo test ... -- --exact`는 대상 테스트가 사라져 0건을 실행해도 exit 0이므로 기존 수집 방식만으로는 false-green이 가능했다.
- RED: S03 collector 계약 테스트가 모든 command의 `minimumPassedTests` 부재를 즉시 검출했다. 별도 fixture는 명령이 exit 0이더라도 `0 passed`이면 gate가 실패하고 나머지 행은 계속 수집해야 한다고 요구했다.
- FIX: `s03-evidence-requirements.json`과 12행 quality matrix, thin collector, CI 수집·업로드를 추가했다. 공용 collector는 raw output을 저장하지 않으면서 libtest 결과를 관찰해 최소 통과 수 미달을 stable nonzero로 바꾼다. 동적 Chronos 로그는 실행 가능한 source tree가 아니므로 build identity 입력에서 제외했다.
- BOUNDARY: 레거시 guard 증거를 rehearsal 허용 테스트에서 실제 production cutover 뒤 `LEGACY_DIRECT_EXECUTION_BLOCKED`를 확인하는 CLI 테스트로 교체했다. 12개 행은 singleton, kernel DACL/impersonation, bounded codec, durable replay, destructive confirmation, DPAPI, typed cutover, clean rollback, SCM wiring, shutdown drain, real pipe, legacy guard를 모두 포함한다.
- VERIFY: collector pass/fail 계약 테스트 통과, `dbbackup-cli` 45 passed/0 failed/2 explicit ignored, CLI all-target Clippy `-D warnings`, workspace fmt check, `git diff --check` 모두 exit 0. 실제 S03 collector는 12/12 실행·통과, failure 0, same revision/tree/build, minimum observed pass 1, raw output field 0을 `artifacts/s03/evidence.json`에 기록한다.
- SAFETY DECISION: service IPC rollback은 DB authority와 Windows Task enablement를 한 원자 transaction으로 묶을 수 없어 dual-run 위험이 있다. rollback 실행은 service와 같은 singleton을 선점하는 offline recovery 경로로 제한한다.
- NEXT: offline singleton recovery tool이 recovery bundle/task inventory를 재검증하고 clean DB rollback 뒤에만 legacy Task를 복원하도록 TDD로 구현한다. post-commit task 복원 실패는 service가 정지된 채 재시도 가능한 incomplete 상태로 남긴다.

#### Cycle 100 — owner-only offline cutover recovery and V2-R03 closure

- ISSUE: service IPC 안에서 SQLite execution authority와 Windows Task Scheduler enablement를 하나의 transaction으로 rollback할 수 없어 crash 시 dual-run 위험이 있었다. 또한 Task XML의 task-level `Settings/Enabled`와 Trigger별 `Enabled`를 구분하지 않으면 실제 일정 definition을 거부하거나 drift를 숨길 수 있었다.
- RED: offline recovery API·공용 singleton guard 부재를 통합 테스트로 고정했고, managed throwaway Task에서 Task Scheduler가 기본 `Enabled=true` 요소를 생략하는 실제 실패를 재현했다. 이어 Trigger와 Settings가 함께 있는 production형 XML, catalog commit 직후 definition drift, zero-test collector와 PowerShell 종료 코드 오판을 각각 실패 테스트로 고정했다.
- AUTHORITY: recovery는 SCM과 동일한 고정 `default` global mutex를 먼저 획득하고, protected `service-identity.json`의 설치 owner와 정확히 일치하는 local elevated token만 허용한다. CLI에서 mutex key를 덮는 옵션은 존재하지 않으며 marker 전체를 두 번 입력해야 한다.
- EVIDENCE: protected root 아래 recovery manifest, 필수 bundle item의 크기·BLAKE3, scheduler inventory의 원본 XML·digest·현재 상태를 모두 재검증한다. 비교에서는 `Settings/Enabled`만 제외하고 Trigger enablement와 나머지 definition은 보존하며, restore 직전과 직후에도 definition을 다시 검사한다.
- ORDER/RECOVERY: clean `rollback_eligible` epoch에서 Catalog authority/audit를 먼저 원자 commit한 뒤에만 legacy Task를 복원한다. 일부 Task 실패는 service claims가 꺼진 legacy 상태로 남고 동일 rollback UUID·marker·bundle·owner·reason·timestamp 재실행만 idempotent하게 계속한다. 첫 production write 뒤에는 scheduler 접근 전 `CONFIG_CUTOVER_FORWARD_FIX_REQUIRED`로 거부한다.
- OPERATOR PATH: `dbbackup service recover-cutover`와 health의 `cutoverMarker`/`recoveryDisposition`, ADR-008, `docs/runbooks/rollback.md`를 연결했다. 수동 SQL과 독립 `schtasks /ENABLE`은 허용 절차가 아니다.
- COLLECTOR: S03 gate를 offline fault suite 5개와 managed real Task를 포함한 14개 all-required 행으로 확장했다. generic collector는 각 command의 최소 실제 pass 수를 확인하고 raw output을 저장하지 않으며, 계약 테스트는 child process의 실제 exit code를 관찰한다.
- VERIFY: Service 전체 unit/integration/doc tests와 CLI 전체 테스트가 통과했고, Service+CLI all-target Clippy `-D warnings`, workspace fmt, `git diff --check`, G1/S03/S05 collector 계약 테스트가 exit 0이다. managed Task restore는 1/1 pass 후 fixture 잔존 0건이다. 실제 S03 evidence는 14/14 pass, failure 0, identity mismatch 0, raw output field 0, build ID `a568c4a9b4f9066d8c037568dd6714229c50bed1db1f14f55dc19f1a5603645d`다.
- STATE: V2-R03을 `proved`로 전환한다. production authority의 clean rollback은 offline owner-only이며 dirty epoch는 영구 forward-fix-only다.
- NEXT: V2-R06 Section-06의 기존 PostgreSQL/MySQL/MariaDB engine과 notification 경계를 현재 코드·managed fixture·content oracle에 대조해 첫 미구현 인증 항목을 RED로 고정한다.

#### Cycle 101 — existing-engine recovery, verification, and notification certification

- ISSUE: provider contract와 Catalog capability 이름이 서로 달랐고, backup commit 뒤 verification이 durable Run/Work로 이어지지 않았으며, notification worker는 mock transport만 있었다. Docker fixture readiness는 공식 이미지의 임시 초기화 서버를 최종 서버로 오인할 수 있었다.
- RED: provider matrix 완전성·tool trust·overwrite fingerprint/replay, L2/L3 무강등, post-backup verification/outbox 원자성, verification retry budget, 실제 webhook 202/503 경계를 각각 실패 테스트로 고정했다. 최초 live 재실행에서는 PostgreSQL/MySQL 초기화 shutdown race가 실제로 재현됐다.
- CONTRACT: `ProviderCapability::REQUIRED`의 canonical 9개 이름을 schema, certification repository, scheduler, G2 collector의 단일 계약으로 통일했다. candidate는 9개 capability가 모두 없으면 publish할 수 없고, tool path/hash/trust 변경은 support를 자동 승격하지 않는다.
- DURABILITY: immutable Plan policy를 backup commit transaction에서 해석해 exact L1/L2/L3 Verification Run, kind-filtered Work Item, Activity, notification outbox를 함께 생성한다. unsupported 또는 L3 Restore Run 미바인딩은 요청 level 그대로 명시적으로 기록하며 낮은 level로 대체하지 않는다. worker는 fence와 bounded retry를 사용해 exact achieved level/evidence만 terminal commit한다.
- DELIVERY: production `WebhookNotificationDelivery`가 TLS 또는 loopback HTTP endpoint에 canonical JSON과 idempotency/fencing headers를 실제 전송한다. redirect·credential/query URL·비정상 payload/header를 process 전 차단하고 408/425/429/5xx와 transport 오류만 bounded retry한다. 실제 202/503 fixture에서 delivery 성공과 원작업 불변성을 확인했고 diagnostics의 secret shape 저장을 거부한다.
- LIVE FIXTURE: collector는 단일 mutex, 최종 init 로그, 연속 readiness를 사용한다. digest-pinned PostgreSQL 18.4, MySQL, MariaDB에서 실제 discovery→backup→native L2→create-new restore→schema/view/routine/trigger/event/blob/content oracle를 수행했다. PostgreSQL은 wrapper가 아닌 `/usr/lib/postgresql/18/bin/*` 실제 바이너리별 SHA-256을 기록하고 MariaDB는 `mariadb-dump`/`mariadb`만 사용한다.
- VERIFY: `artifacts/g2/existing-engines.json`은 build `bd707b35e6f30bb2cbefab846cd8ec4469c586df09d512fa4714f924cfa393a5`, source tree `1e12f854…de1e`, 3/3 engine, 4/4 executable contract check, failure 0, engine별 capability 9/9와 source/restored oracle hash 일치를 기록한다. 관련 6개 crate 전체 테스트, all-target Clippy `-D warnings`, workspace fmt, `git diff --check`가 통과했고 G2 컨테이너 잔존은 0개다.
- STATE: V2-R06을 `proved`로 전환한다.
- NEXT: V2-R07 Section-07 요구사항과 DESIGN.md를 읽고 현재 console IA/API/component/browser 증거의 첫 공백을 TDD로 구현한다.

#### Cycle 102 — connection-centered 9-route product UI and browser gate

- ISSUE: 기존 console은 설정·Job 중심 화면에 머물러 Connection 아래 discovered/selected Database Target, immutable Plan revision, durable Activity, Snapshot/Recovery 진입을 하나의 제품 IA와 typed service boundary로 제공하지 못했다.
- RED: 정확한 9-route navigation/back-state, keyboard-only Connection→probe→discovery→Target→Plan 진입, 100,000 Activity bounded DOM, font/theme/zoom/false-success/WCAG matrix를 Playwright로 먼저 고정했다. 기존 화면·fixture는 route/state와 접근성 계약을 충족하지 못했다.
- UI: `Overview / Connections / Databases / Destinations / Backup Plans / Activity / Snapshots / Recovery / Settings` shell과 table+inspector workspace를 구축했다. filter/cursor/selection을 URL에 보존하고 100k Activity는 strict keyset 50-row page와 bounded DOM을 사용한다. Pretendard/Space Grotesk/JetBrains Mono를 실제 load하고 DESIGN.md의 calm teal token, light/dark `color-scheme`, container-query/reduced-motion, label/focus-trap/live-region을 적용했다.
- BOUNDARY: frontend의 production data path는 Tauri `service_request` 하나이며 native tool, SQLite, config, Secret Store를 직접 열지 않는다. browser fixture는 화면 상단에서 실행 증거가 아님을 명시한다. long command는 Work Item ID를 받고 terminal 상태까지 조회하며 secret 입력은 저장 직후 지운다.
- VERIFY: Vitest 4 files/6 tests, production `tsc --noEmit && vite build`, 1024/1280/1600의 route/keyboard/100k/theme/font/zoom/copy/Axe matrix가 통과했다. 후속 Destination journey 포함 최종 Playwright는 15/15다.
- NEXT: `connection.probe`와 `discovery.run`을 mock 응답이 아닌 service-owned durable Operation Worker, native wire/CLI probe, Catalog evidence에 연결한다.

#### Cycle 103 — durable Connection probe/discovery execution

- ISSUE: service application은 Connection draft 저장과 Work enqueue까지만 수행하고 `connection_probe`/`connection_discovery`를 claim·lease·fence·실행·정산하는 production worker가 없어 UI가 terminal 근거를 받을 수 없었다.
- RED: 먼저 supported operation filter, probe projection, discovery snapshot/Target 선택, DPAPI credential custody를 요구하는 worker 통합 테스트를 추가해 실행 모듈 부재 compile-fail을 확인했다. 별도 ignored live test는 UI application save→DPAPI→durable discovery→native PostgreSQL→Target select 전체를 고정했다.
- WORKER: authority가 claims-enabled일 때만 SCM이 background worker를 시작한다. worker는 해당 두 operation kind만 claim하고 current lease/fence/cancel/retry/commit을 지키며 다른 generic Operation을 훔치지 않는다. Secret Store에서는 `DatabaseAuth` purpose만 resolve하고 결과·Activity에는 secret을 포함하지 않는다.
- PROVIDER: PostgreSQL은 Rust wire와 `postgres-native-tls`로 disabled/preferred/required/verify-ca/verify-full을 분리하고 connection future를 함께 구동한다. MySQL/MariaDB는 native client의 version/catalog 결과를 family와 대조하며 system schema를 excluded observation으로 남긴다. probe와 discovery 결과는 idempotent Catalog run 및 Connection/Target projection에 commit된다.
- LIVE VERIFY: digest-pinned PostgreSQL fixture `postgres@sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a`에서 ignored live test를 명시 실행해 1/1 통과했고 정확한 컨테이너를 제거했다. focused Catalog/Service 회귀와 전체 Service/Catalog/Application test suites가 통과했으며 Clippy `-D warnings`도 통과했다.
- NEXT: Destination wizard의 inert save/probe action을 role-separated Secret과 실제 writer/read-hash/delete evidence에 연결한다.

#### Cycle 104 — role-separated Destination probe and V2-R07 closure

- ISSUE: Destination 화면의 생성·probe 버튼이 동작하지 않았고 remote endpoint schema가 repair/retention resolver와 달랐다. writer 성공을 reader 복구 가능성으로 오인하지 않도록 실제 임시 객체와 역할별 증거가 필요했다.
- RED: Local Destination 저장→durable probe→writer commit→reader BLAKE3→delete→3개 role evidence→`ready` projection을 요구하는 통합 테스트를 먼저 추가해 `destination.probe` command 부재 compile-fail을 확인했다. keyboard-only onboarding/Axe Playwright도 추가했고 첫 실행은 보조 문구가 label accessible name에 섞여 exact label을 찾지 못해 실패했다.
- EXECUTION: `destination.probe`를 40번째 typed operate command로 추가하고 기존 Operation Worker가 해당 kind만 선택적으로 dispatch한다. Local/S3/SSH resolver는 writer, reader, optional delete Secret Ref와 purpose를 독립적으로 사용한다. unique `.selftest` object를 commit하고 reader가 byte count+BLAKE3를 검증한 뒤 delete 또는 retention-lock evidence를 기록한다. writer/reader 중 하나라도 실패하면 Destination은 `attention`, Work는 stable `DESTINATION_PROBE_FAILED`다.
- CONTRACT/UI: Local/S3/SSH의 canonical endpoint와 schema-versioned S3/SSH credential shape를 application에서 엄격 검증하고 probe/repair/retention parser를 같은 aliases·fields로 맞췄다. 3단계 wizard는 endpoint→role credentials→probe evidence를 제공하며 비밀 입력을 저장 직후 지운다. 설명은 `aria-describedby`로 분리해 exact accessible name을 유지하고 inspector 재-probe도 실제 command를 호출한다.
- VERIFY: Local role probe 1/1, Service application credential/catalog test 1/1, Service+Catalog+Application 전체 integration suites가 실패 0으로 통과했다. frontend Vitest 6/6, production build, Playwright 15/15, 관련 Rust 5-package all-target Clippy `-D warnings`가 통과했다.
- STATE: V2-R07을 `proved`로 전환한다. 최종 release 판정 전 현재 source tree로 workspace와 UI gate를 다시 실행한다.
- NEXT: V2-R08의 Snapshot inventory, restore eligibility/preflight, safe create-new/overwrite confirmation, replica repair/drill UI와 typed IPC의 첫 실제 공백을 RED로 고정한다.

#### Cycle 105 — authoritative Snapshot inventory query

- ISSUE: UI에 Snapshot inventory가 있었지만 production `snapshot.list`는 `UnknownCommand`였고, complete/incomplete 구분과 4축·서명·Part·Replica·verification evidence를 bounded Catalog query로 읽는 경계가 없었다.
- RED: complete/degraded/non-compliant/conditional Recovery Point와 unknown-key incomplete Recovery Point를 함께 넣은 Service integration test가 `snapshot.list`에서 `UnknownCommand`로 실패했다.
- FIX: opaque time+ID cursor와 최대 100행 제한을 가진 Catalog snapshot query를 추가했다. 기본 scope는 valid complete만 반환하고 diagnostics는 incomplete/orphaned/quarantined/invalid/unknown-key를 분리한다. Target Connection, engine, time, completeness, durability, compliance, 최소 verification, eligibility, signature, search 필터와 signed Manifest metadata, Part, Replica retention/reader, L1-L3 projection evidence를 직렬화한다.
- VERIFY: `cargo test -p dbbackup-service --test snapshot_application` 1 passed/0 failed. 테스트는 complete 기본 목록 1건, diagnostics 1건, 4축 독립 상태, Connection 결합, 필터 전체, Manifest/Part/Replica/L2 evidence, unknown scope fail-closed를 실제 Operational Store에서 검증했다.
- NEXT: `verification.enqueue`를 기존 durable Verification Repository/Worker에 연결하고 unsupported L3의 무강등 계약을 application boundary에서 증명한다.

#### Cycle 106 — exact-level manual verification enqueue

- ISSUE: Snapshot inspector의 L2 action은 preview에서만 성공했고 production `verification.enqueue`가 `UnknownCommand`였다. 기존 durable Verification Repository는 post-backup constructor만 공개해 manual trigger/source Replica를 명시할 수 없었다.
- RED: 인증된 PostgreSQL complete Snapshot에서 L2 manual enqueue와 unbound L3 explicit unsupported를 요구한 같은 Service integration test가 `UnknownCommand`로 실패했다.
- FIX: manual Verification Work/unsupported spec을 추가하고 application command를 배선했다. 서비스가 complete+valid Manifest와 committed reader-eligible Replica를 다시 확인하고 current Engine certification의 정확한 `verify_l1`/`verify_l2`/`restore_l3` capability를 조회한다. L1/L2는 exact level Work를 enqueue하고, Restore Run 없는 L3는 `VERIFICATION_RESTORE_RUN_REQUIRED` 상태를 level 3 그대로 기록하며 낮은 level Work를 만들지 않는다.
- VERIFY: focused test 1 passed/0 failed. 실제 DB에서 L2 Run이 `requested_level=2`, `trigger_kind=manual`, `state=queued`이고 Work ID가 존재함을 확인했다. L3는 Work ID NULL·state unsupported이며 하위 level row 0건, incomplete unknown-key source는 `APPLICATION_PAYLOAD_INVALID`로 상태 변경 전에 거부됐다.
- NEXT: Recovery target fingerprint와 tool/permission/connection/version/existence/policy를 service-authoritative하게 계산하는 typed `restore.preflight` command를 RED로 추가한다.

#### Cycle 107 — service-authoritative restore preflight

- ISSUE: Recovery UI가 Snapshot 객체만 보고 preflight를 브라우저에서 합성해 target Connection, trusted tool, Secret binding, spool, server/version, target existence drift를 권위 있게 차단할 수 없었다.
- RED: typed `RestorePreflight` command를 참조하는 integration test가 enum 부재로 compile-fail했다. 테스트는 create-new 성공, existing-name 차단, overwrite 존재성, process 0회, server probe drift 뒤 fingerprint/version 변경을 요구했다.
- FIX: read-only `restore.preflight` command와 Catalog target-facts query를 추가했다. 서비스는 signed complete source와 reader-eligible Replica, current restore+validation certification, trusted signed tool inventory, observed spool headroom, active database-auth Secret Ref, 5분 이내 passed Connection probe, server version, latest successful discovery의 target existence, same-engine/system-database policy를 각각 pass/warning/fail로 계산한다.
- BINDING: endpoint/connection string은 응답하지 않는다. target fingerprint는 Connection ID+engine+normalized name+observed server version에, preflight version은 source ingest/Replica/target fingerprint/mode/probe/spool/existence/eligibility에 BLAKE3로 결합한다. 증거 drift 시 이전 prepare 결과를 재사용할 수 없다.
- VERIFY: focused Service test 1 passed/0 failed. create-new은 9개 check가 pass/warning이고 eligible, 같은 이름 충돌은 target_existence failed/eligible false, overwrite exact target은 eligible, 실패 preflight 뒤 `process_instances=0`, server 17.4→17.5 probe drift 뒤 두 digest 모두 변경됨을 실제 SQLite 상태로 확인했다.
- NEXT: preflight version을 submit 직전에 재계산하고 create-new Restore Run+Work를 원자 enqueue하는 repository/application contract를 구현한다.

#### Cycle 108 — create-new Restore Run and Work enqueue

- ISSUE: `restore.enqueue_safe`는 production application에서 `UnknownCommand`였고, preflight result를 source/Replica/target/fingerprint와 결합해 Restore Run+Work를 원자 생성하는 repository 경계가 없었다.
- RED: server evidence drift 전 preflight version은 거부하고 최신 version만 create-new Restore Run/Work를 만들도록 확장한 focused test가 stale 요청에서 `APPLICATION_COMMAND_UNKNOWN`으로 실패했다.
- FIX: `SafeRestoreSpec`과 Recovery Repository를 추가했다. eligibility snapshot의 eligible/mode/preflight/target/source binding을 구조적으로 검증하고, transaction 안에서 complete+valid+eligible/conditional source, exact committed reader Replica, ready same-engine target Connection을 다시 확인한 뒤 `restore_runs`와 `work_items(kind=restore)`를 함께 insert한다.
- APPLICATION: submit 시 `restore.preflight`를 즉시 다시 실행해 version과 eligible을 비교한다. 최신 결과 전체와 required verification level을 immutable eligibility snapshot으로 저장하고 create-new Restore/Work ID와 Activity cursor를 반환한다.
- VERIFY: focused Service test 1 passed/0 failed. drift 전 version은 상태 변경 없이 `APPLICATION_PAYLOAD_INVALID`, 최신 version은 `conflict_policy=create_new`, `state=queued`, exact target name/source Replica인 Restore Run 1건과 연결된 Restore Work 1건을 생성했다.
- NEXT: production IPC의 `TypedTransactionRequired` 공백을 닫아 SID/idempotency/payload와 Restore Run/Work/response completion을 단일 transaction으로 묶는다.

#### Cycle 109 — production safe-restore IPC atomicity

- ISSUE: application-level create-new enqueue는 동작했지만 production dispatcher에는 SID·idempotency ledger·preflight 재검사·Restore Run·Work Item·응답 완료를 하나의 원자적 경계로 묶는 증거가 없었다.
- RED: 잘못된 Recovery Point, 동일 키 재실행, 동일 키의 다른 payload를 포함한 저장소 테스트와 실제 production/G1 dispatcher 테스트를 추가했다.
- FIX: `enqueue_safe_restore_request`가 IMMEDIATE transaction 안에서 ledger accepted, authoritative source/Replica/target 재검사, Restore Run/Work insert, ledger succeeded를 함께 커밋한다. dispatcher는 완료된 요청을 preflight보다 먼저 재생하고, 신규 요청만 service-authoritative preflight를 다시 수행한다. 동시 재생 응답 검증은 새로 생성한 로컬 ID가 아니라 저장된 Restore Run/Work 형식에 결합하도록 수정했다.
- VERIFY: `cargo test -p dbbackup-catalog --test ipc_safe_restore_repository` 1 passed/0 failed. `cargo test -p dbbackup-service --test restore_ipc_dispatch` 1 passed/0 failed. 실패 경로의 ledger/Restore/Work는 0건, 성공·재생·충돌 뒤에는 각각 정확히 1건이며 process instance는 0건이었다. 재생 시 preflight use case 호출 횟수도 1에서 증가하지 않았다.
- NEXT: durable restore worker가 typed Restore context를 claim하고 실제 restore/validation/cleanup 단계를 fencing·cancel·retry 계약 안에서 실행하도록 구현한다.

#### Cycle 110 — durable Restore Worker and false-success barrier

- ISSUE: `WorkItemKind::Restore`는 schema에 있었지만 Runtime Repository가 Restore Run subject를 load/start/validate/commit/terminal 정산하지 못했고, 시작된 복구의 lease 만료는 안전한 cleanup evidence를 만들지 못했다.
- RED: native exit 0+oracle pass, native exit 0+oracle mismatch, target 생성 뒤 cancel, 실행 중 lease expiry/crash 네 시나리오를 Restore Worker integration test로 고정했다.
- FIX: authoritative Restore claim context가 submit 당시 preflight binding과 현재 Recovery Point/Replica, target Connection·Secret·probe·server, discovery existence, spool, current certification, trusted signed tool을 다시 계산하고 fingerprint/version drift를 차단한다. Restore Worker는 restore와 validation을 별도 phase로 실행하고 validation pass 뒤에만 commit한다. 증거 객체는 credential/token/connection-string 형태를 거부한다.
- SAFETY: oracle mismatch와 target 생성 뒤 cancel은 `cleanup_required`·`targetPreserved=true`로 정산한다. 시작된 Restore claim의 lease가 만료되면 자동 재시도·자동 삭제하지 않고 `RESTORE_CRASH_CLEANUP_REQUIRED` evidence와 함께 terminal 처리한다. 시작 전 만료만 재queue 가능하다.
- VERIFY: `cargo test -p dbbackup-runtime --test restore_worker` 4 passed/0 failed. `cargo test -p dbbackup-runtime --tests` 전체 통과, `cargo test -p dbbackup-catalog --test runtime_repository` 5 passed/0 failed. exit 0/oracle fail의 succeeded row는 0건이었다.
- NEXT: PostgreSQL/MySQL/MariaDB의 trusted native tool/reader/Secret resolver를 Restore Executor에 연결하고 실제 create-new restore/oracle fixture를 같은 Work/Restore Run에 귀속한다.

#### Cycle 111 — actual create-new Restore Run and content oracle

- ISSUE: G2 Docker fixture는 세 엔진의 실제 backup/restore를 증명했지만 durable Restore Run/Work, fencing, process evidence, validation terminal과 분리돼 있어 G3-B의 실제 create-new journey를 입증하지 못했다.
- RED: pinned PostgreSQL G2 custom dump를 Restore Worker에 태운 첫 실행에서 native restore exit 0 뒤 oracle digest가 G2 digest와 달랐다. Worker는 성공을 만들지 않고 Restore Run을 `cleanup_required`와 `RESTORE_ORACLE_MISMATCH`로 정산했다.
- FIX: Restore Executor에 현재 fenced claim을 전달하고, 이미 running인 in-process Work도 active process가 없을 때 순차 native phase를 등록할 수 있게 Repository를 확장했다. Docker fixture Executor는 hash-verified Docker CLI를 Windows Job Object로 실행하고 각 process start/running/exit, heartbeat, cancel, timeout을 같은 Attempt에 기록한다. G2의 deterministic oracle 계약처럼 줄 trim/filter/sort 뒤 SHA-256을 비교한다.
- VERIFY: ignored live gate `live_postgresql_create_new_restore_runs_through_durable_restore_work_and_oracle` 1 passed/0 failed. pinned image `sha256:3a82…4744a`, dump `d69a…c29a`, oracle `f232…b75`, process 8건, Restore/Work 모두 succeeded이고 `artifacts/g3-b/restore-worker-postgresql.json`에 비밀 없는 증거를 기록했다. `cargo test -p dbbackup-catalog --test runtime_repository` 6 passed, 기본 Restore Worker 4 passed/1 ignored.
- SAFETY: Docker credential은 command line·환경에 넣지 않고 격리 fixture의 trust auth만 사용했다. 첫 oracle mismatch 사례가 cleanup-required로 보존돼 exit 0 false-success barrier도 실제 프로세스에서 입증됐다.
- NEXT: owner-only overwrite prepare/execute confirmation을 authoritative preflight와 같은 atomic Restore enqueue에 연결하고 mismatch/expiry/replay process 0을 검증한다.

#### Cycle 112 — overwrite confirmation binding and zero-process rejection

- ISSUE: `restore.enqueue_overwrite`가 command registry에는 있었지만 durable dispatcher에서 `IPC_TYPED_TRANSACTION_REQUIRED`로 끝났고, 구형 repository 경로는 source Replica, authoritative preflight version, persisted response replay 계약을 보존하지 못했다.
- RED: owner가 발급한 token을 다른 승인 관리자 SID, 변경된 payload, 만료 시각, 소비 후 새 idempotency key로 제출하는 dispatcher integration matrix를 추가했다. 기존 경로에서는 prepare/execute가 연결되지 않아 실행할 수 없었다.
- FIX: overwrite prepare가 service-authoritative overwrite preflight, target identity 재입력, final source/target/cleanup summary를 요구하도록 연결했다. raw token과 exact execute payload는 첫 응답 메모리에만 반환하며 ledger에는 safe summary와 digest만 저장한다. execute는 submit 직전 preflight를 재실행하고 SID/action/resource/fingerprint/full-payload/expiry bound token을 같은 IMMEDIATE transaction에서 한 번 소비하면서 source Replica가 결합된 Restore Run/Work와 replayable response를 생성한다.
- SAFETY: SID mismatch, payload mismatch, expiry, consumed-token replay는 모두 `CONFIRMATION_TOKEN_INVALID_OR_REPLAYED`로 종료되고 transaction rollback 후 Restore Run 0, restore Work 0, process instance 0을 유지한다. 정확히 같은 IPC replay는 preflight를 다시 실행하지 않고 기존 결과만 반환한다.
- VERIFY: `cargo test -p dbbackup-catalog --test ipc_confirmation_repository` 7 passed/0 failed. `cargo test -p dbbackup-service --test restore_ipc_dispatch` 2 passed/0 failed. `cargo test -p dbbackup-service --test ipc_authorization` 4 passed/0 failed.
- NEXT: Replica repair/restore drill/catalog rebuild preview를 durable command와 공통 Activity 모델에 연결한 뒤 Snapshot/Recovery UI와 keyboard/zoom/theme/a11y gate를 완성한다.

#### Cycle 113 — atomic Replica repair request

- ISSUE: `repair.enqueue`가 generic application 경로에만 의존해 production dispatcher에서는 typed transaction이 없었고, source Replica·target Destination·현재 Secret Ref·Operation/Work/Activity/IPC response가 한 원자 경계에 결합되지 않았다.
- RED: complete+valid Snapshot의 committed reader Replica와 별도 ready Destination을 선택해 enqueue/replay/payload-conflict를 검증하는 production dispatcher 테스트를 추가했다. 기존 경로는 `IPC_TYPED_TRANSACTION_REQUIRED`로 실패했다.
- FIX: Catalog IPC Repository가 authoritative Recovery Point/Replica projection과 target Destination 상태를 재검사하고, 현재 source reader 및 target writer/reader Secret Ref를 같은 IMMEDIATE transaction에서 resolve/pin한다. deterministic Operation Run/Work Item, `REPLICA_REPAIR_QUEUED` Activity, replay-safe IPC completion을 함께 생성하며 이미 healthy한 동일 target과 source=target을 거부한다.
- VERIFY: focused production dispatcher test 1 passed/0 failed, `cargo check -p dbbackup-catalog -p dbbackup-service` 통과. 정확한 replay는 같은 response를 반환하고 변경된 target payload는 `IPC_IDEMPOTENCY_CONFLICT`; 성공 뒤 repair ledger/Operation/Work/Activity가 각각 1건, process instance는 0건이었다.
- NEXT: restore drill을 incident restore와 분리하고 registered test target, RTO, L3 evidence를 같은 Restore 수명주기에 결합한다.

#### Cycle 114 — isolated restore drill and bound L3 evidence

- ISSUE: schema와 Verification Repository에는 `restore_drill` 목적/L3 binding이 있었지만 모든 safe restore가 `incident_recovery`로 고정돼 실제 drill Run을 만들 수 없었다. 일반 Connection과 test target 구분, Restore terminal과 L3 evidence 정산도 없었다.
- RED: production IPC가 하나의 Restore Work와 결합된 L3 Verification Run을 만들고, Restore Worker 성공 시 L3 passed/RTO/evidence를 정착시키며 별도 Verification Work를 만들지 않는 두 integration test를 추가했다. 초기 구현은 drill 목적을 수용하지 않아 실패했다.
- FIX: Connection `adapter_config_json.restoreTestTarget`을 명시적 등록 경계로 노출하고 preflight에 `restore_target_role` check를 추가했다. drill은 create-new+level 3만 허용하며 일반 Connection에서는 eligible=false다. safe enqueue는 Restore Run/Work와 deterministic L3 Verification Run을 원자 생성하고, start/terminal/cancel이 linked verification의 running/passed/failed/cancelled, achieved level, validation evidence를 같은 transaction에서 정산한다. Snapshot query에는 target/RTO/cleanup evidence가 결합된 restore drill history를 추가했다.
- VERIFY: `cargo test -p dbbackup-service --test restore_ipc_dispatch` 4 passed/0 failed. `cargo test -p dbbackup-runtime --test restore_worker` 5 passed/0 failed/1 ignored. `cargo test -p dbbackup-service --test snapshot_application` 1 passed/0 failed. `cargo check -p dbbackup-catalog -p dbbackup-service -p dbbackup-runtime` 통과.
- NEXT: `verification.enqueue` production IPC와 Catalog rebuild preview/diagnostics contract를 닫고 Recovery UI를 authoritative command shape에 연결한다.

#### Cycle 115 — atomic manual verification IPC

- ISSUE: application-level manual verification은 exact level을 보존했지만 production dispatcher의 SID·idempotency·authoritative source/Replica/capability 재검사와 Verification Run/Work/응답 완료가 하나의 transaction이라는 증거가 없었다.
- RED: L2 enqueue/replay/payload conflict와 Restore Run 없는 L3를 production dispatcher로 호출하는 integration matrix를 추가했다.
- FIX: IMMEDIATE transaction 안에서 complete+valid Recovery Point, committed reader-eligible Replica, current provider capability를 다시 확인하고 L1/L2 Verification Run과 Work, IPC ledger completion을 함께 커밋한다. L3는 `VERIFICATION_RESTORE_RUN_REQUIRED` unsupported 결과를 level 3 그대로 기록하고 낮은 level Work를 만들지 않는다.
- VERIFY: `cargo test -p dbbackup-service --test restore_ipc_dispatch` 5 passed/0 failed. exact replay는 같은 응답을 반환하고 변경 payload는 idempotency conflict, unsupported L3 뒤에는 process instance와 하위-level Work가 0건이었다.
- NEXT: trusted signing key를 사용하는 read-only Catalog rebuild preview와 v2 Manifest source discovery를 service boundary에 연결한다.

#### Cycle 116 — read-only Catalog rebuild preview

- ISSUE: Snapshot diagnostics가 unknown-key·malformed·duplicate Manifest를 설명할 수 있었지만 운영자가 Catalog 변경 없이 Local/S3/SSH source를 스캔하고 분류하는 production command가 없었다.
- RED: 실제 Local v2 `set-manifest.json`, malformed envelope, 빈 Catalog projection을 사용한 Service integration test를 추가했고 previewer/command wiring 부재로 실패했다.
- FIX: identity config에 최대 32개의 검증된 Ed25519 trusted signing key를 추가하고, Local/S3/SSH의 reader credential로 `prepare_catalog_rebuild_v2`를 실행하는 `catalog.rebuild_preview`를 배선했다. source listing은 legacy `.manifest.json`과 v2 `set-manifest.json`을 모두 찾으며 accepted/rejected/duplicate/unknown-key를 분리한다.
- SAFETY: preview는 Operational Store를 수정하지 않으며 remote writer credential로 폴백하지 않는다. duplicate key ID, 잘못된 base64/길이/Ed25519 key는 서비스 시작 전에 거부한다.
- VERIFY: Catalog preview 1/1, identity config 3/3, S3 source 1/1, SSH source 1/1, Catalog rebuild v2 3/3 통과. 테스트 뒤 Snapshot projection이 계속 비어 있어 read-only 경계를 확인했다.
- NEXT: Snapshot/Recovery 화면을 typed production command와 authoritative preflight/Work lifecycle에 연결하고 접근성·브라우저 safety gate를 실행한다.

#### Cycle 117 — evidence-driven Snapshot and Recovery UX

- ISSUE: 기존 Recovery 화면은 사용자가 Snapshot ID를 직접 입력하고 브라우저 fixture가 preflight를 합성했으며, overwrite token·durable Work resume·diagnostics/repair/drill/catalog preview를 section-08 계약대로 제공하지 못했다.
- RED: Snapshot 선택 기반 create-new, service preflight와 재확인, overwrite focus/token custody, restore-test target drill, exact L3 unsupported, unknown/duplicate catalog preview, 200% zoom/WCAG를 Playwright로 먼저 고정했다.
- UI: Snapshot inventory와 diagnostics를 분리하고 backend filter, Manifest/Part/Replica/retention/verification/drill evidence를 표시한다. Recovery는 선택된 Snapshot·Replica·target에서 authoritative preflight를 얻고 create-new를 기본값으로 삼는다. owner overwrite는 exact target 재입력과 최종 summary를 요구하며 raw token/request는 `useRef`에만 잠시 보관하고 즉시 지운다. Work ID는 URL로 resume/poll/cancel하고 validation/cleanup timeline을 표시한다.
- SAFETY: incomplete/unknown-key 진단 행에는 restore action이 없고 rebuild preview만 제공한다. repair는 source Replica와 다른 target Destination을 명시한다. L3는 registered restore-test target의 drill만 실행하며 지원되지 않으면 성공으로 낮추지 않는다. Dialog focus trap/return, inspector scroll region, danger contrast, theme/option/zoom을 교정했다.
- VERIFY: frontend Vitest 5 files/8 tests, production `tsc --noEmit && vite build`, Chromium 1024/1280/1600 Playwright 24/24 통과. Service catalog preview 1/1, restore IPC 5/5, identity 3/3, Snapshot application 1/1, Runtime Restore Worker 5 passed/1 ignored, Catalog rebuild v2 3/3 및 관련 package check가 통과했다.
- STATE: V2-R08을 `proved`로 전환한다. 실제 PostgreSQL create-new Restore/Oracle의 G3-B 증거는 Cycle 111의 `artifacts/g3-b/restore-worker-postgresql.json`에 귀속된다.
- NEXT: V2-R09 SQLite online backup/create-new/overwrite와 SQL Server BACPAC candidate provider를 공통 certification/Engine Matrix에 연결한다.

#### Cycle 118 — Wave-1 provider contract and false-promotion RED

- ISSUE: core Engine과 provider certification 계약에 SQL Server/SQLite가 없고, tool trust는 외부 실행 파일만 표현해 embedded SQLite dependency를 정직하게 인증할 수 없었다. certification 평가 순서상 필수 capability가 비어도 `limited`가 먼저 반환될 수 있었다.
- RED: 정확한 9-capability completion, 고정 dependency checksum, SQL Server candidate의 supported 승격 0, DACPAC data-backup claim 0을 요구하는 provider 테스트를 추가해 기존 API 부재와 잘못된 평가 순서를 드러냈다.
- FIX: `Engine::{Sqlserver,Sqlite}`, `ToolTrust::LockedDependency`, `embedded://` identity를 추가하고, missing capability/untrusted tool은 limitation보다 먼저 candidate가 되도록 평가 순서를 고쳤다. exhaustive engine mapping과 legacy CLI의 service-profile 전용 오류도 함께 닫았다.
- VERIFY: core/application package tests와 workspace compile 경계가 통과했다.
- NEXT: SQLite Online Backup API와 파일 identity/atomic restore를 구현한다.

#### Cycle 119 — SQLite online backup and atomic restore

- ISSUE: live SQLite를 파일 복사하면 WAL/동시 writer 상태에서 불완전 artifact가 생기고, overwrite 중 고장 나면 원본을 잃을 수 있었다.
- RED: WAL active writer, exclusive lock timeout, 출력 budget 부족, cancellation, symlink/reparse, create-new restore, identity drift, replace fault, 실제 Windows atomic overwrite를 포함한 conformance matrix를 먼저 고정했다.
- FIX: exact `rusqlite 0.31.0` Online Backup API를 bounded page step/retry/cancel/free-space budget으로 실행한다. staged DB의 `integrity_check`, `foreign_key_check`, read-only scalar oracle와 fsync 뒤 source/target file identity를 재검사한다. create-new는 write-through move, overwrite는 Windows `ReplaceFileW`로 원본 ACL/속성을 보존하며 실패 시 원본을 남긴다.
- VERIFY: `cargo test -p dbbackup-source-sqlite --test conformance` 8 passed/0 failed. active uncommitted writer의 행은 복구본에 섞이지 않았고, lock/space/cancel/replace-fault에서 partial final artifact 또는 원본 손상은 0건이었다.
- NEXT: production Connection probe/discovery와 common stream pipeline에 SQLite를 연결한다.

#### Cycle 120 — SQLite service integration and certification

- ISSUE: provider 단위 backup은 있어도 서비스 Connection Profile이 file transport를 probe/discover하지 못했고, 지원 상태를 현재 certification evidence에서 projection하는 API가 없었다.
- RED: 실제 SQLite fixture를 service-owned operation worker가 probe/discover하고 stable file identity를 반환하는 테스트와, 인증 9행/후보/미구현 엔진을 함께 projection하는 Catalog 테스트를 추가했다.
- FIX: file transport SQLite inspector와 단일 file target discovery를 서비스에 연결했다. SQLite source는 common `BackupSource`/`DumpArtifact::Stream` 경로를 사용하며 exact registry checksum을 tool trust로 기록한다. Catalog와 `engine.matrix` application command는 모든 9개 엔진의 current capability를 persisted certification에서 계산하고 evidence 없는 엔진을 unavailable로 합성한다.
- VERIFY: SQLite common-pipeline 포함 conformance 8/8, service native SQLite 1/1, Catalog Engine Matrix 1/1, service application Engine Matrix 1/1 통과.
- NEXT: SQL Server는 실제 evidence 전 승격할 수 없는 BACPAC candidate 경계를 구현한다.

#### Cycle 121 — SQL Server BACPAC candidate and live-runner boundary

- ISSUE: SQL Server 지원을 넓게 선언하면 BACPAC에 없는 login/role/Agent job/permission/key와 일관성 한계를 숨기며, SqlPackage password argv와 tool drift 위험도 생긴다.
- RED: BACPAC-only typed command, DACPAC data-backup false, exact version/hash/publisher 불일치 시 launcher 호출 0, incomplete certification의 candidate 유지와 필수 limitation을 테스트로 고정했다.
- FIX: Windows integrated authentication만 쓰는 SqlPackage Export/Import command와 exact identity policy를 구현했다. G4 runner는 명시적 `-RunLiveSqlServer`와 server, version, SHA-256, Authenticode publisher가 모두 있을 때만 임의 이름의 격리 DB를 생성해 export/import/content oracle 후 자신이 만든 DB만 정리한다. TLS trust bypass와 password 입력은 없다.
- VERIFY: SQL Server candidate contract 3/3과 G4 runner contract test 통과. 현재 환경은 `sqlpackage`/SQL Server fixture가 없어 live process 0건이며 `artifacts/g4/sqlserver.json`은 `candidate`, tool preflight만 passed, 나머지 capability는 not_run이다.
- PARKED: 실제 SQL Server BACPAC export/import/oracle — 사유: 외부 접근 부재 및 SqlPackage/SQL Server 사용 조건 승인 경계 — 진행 상태: 안전한 라이브 runner, 정확한 identity gate, fixture/oracle/cleanup, limitation/UI 차단까지 구현 완료.
- NEXT: persisted Engine Matrix를 Plan preflight UI에 연결하고 managed G4 evidence를 발행한다.

#### Cycle 122 — evidence-driven Engine Matrix and G4 publisher

- ISSUE: Plan wizard가 SQL Server를 하드코딩된 `limited`로 취급하고 그 외 엔진은 근거 없이 모두 supported로 가정해 candidate Plan을 게시할 수 있었다.
- RED: SQL candidate blocked, SQLite supported, row 없음 unknown의 단위 계약과 실제 browser Plan journey를 추가했다. 1024/1280 browser에서 fixed rail이 dialog를 덮는 포인터 결함도 재현됐다.
- FIX: Backup Plans가 `engine.matrix`를 함께 읽고 선택 engine의 `backup`과 정확한 `verify_l1/verify_l2/restore_l3` 행만 평가한다. candidate/experimental/unknown/expired/suspended는 publish를 차단하고 limitation/evidence를 표시한다. Dialog는 document body portal로 이동해 모든 viewport에서 rail stacking context를 벗어난다. release truth는 SQLite supported와 SQL Server experimental 상태로 갱신했다.
- VERIFY: Engine Matrix Vitest 3/3, Playwright 1024/1280/1600 3/3, frontend production build, service/catalog focused tests 통과. `scripts/test-engine-wave1.ps1`은 6/6 checks와 failureCount 0을 기록했고 build `bcd5de22…00bddd`에서 SQLite supported, SQL Server candidate를 발행했다.
- STATE: V2-R09의 실행 가능한 SQLite와 SQL Server candidate 범위는 완료했다. 실제 SQL Server live promotion만 외부 fixture 결정으로 주차한다.
- NEXT: V2-R10 확장·설치·업데이트·관측·지원 번들·rollback·release rehearsal을 구현한다.

#### Cycle 123 — transactional update, downgrade, and rollout stop

- ISSUE: section-10이 요구하는 stage-by-stage update journal, crash recovery, minimum-reader compatibility, provider promotion, downgrade/rollout stop의 실행 계약이 없었다.
- RED: 14개 update stage와 각 fault point의 recovery disposition, minimum-reader 위반, candidate provider의 조기 승격을 실패 행렬로 먼저 고정했다.
- FIX: prepare부터 postflight/commit까지 14-stage journal과 resume/rollback/forward-only 판정, minimum-reader/cutover guard, provider promotion gate, cohort rollout stop 정책을 구현했다. drain·update commit은 service control 경로와 결합했다.
- VERIFY: update fault matrix, minimum-reader/rollout, diagnostics/drain focused suites가 모두 통과했고 최종 G5에서 해당 3개 check가 exit 0으로 재실행됐다.
- NEXT: 운영 진단, 지원 번들, telemetry 보존 경계를 구현한다.

#### Cycle 124 — bounded diagnostics, support bundle, and telemetry archive

- ISSUE: 운영자가 장애를 설명할 진단 snapshot과 비밀정보가 제거된 지원 번들, bounded telemetry archive가 없었고 장기 실행이 저장소를 무제한 점유할 수 있었다.
- RED: 필수 문서 allowlist, secret canary 제거, 비동기 operation 상태, 보존 상한과 archive rotation을 계약 테스트로 고정했다.
- FIX: 최근 7일/최대 500개 진단 범위와 5개 allowlisted 문서만 포함하는 support bundle을 만들고, secret canary를 차단했다. telemetry는 30일/최대 5,000건 및 시간 단위 archive로 제한했으며 bundle 생성은 durable operation으로 노출했다.
- VERIFY: support canary, support operation, telemetry archive suites가 통과했고 최종 G5의 대응 3개 check도 exit 0이었다.
- NEXT: Windows installer와 release supply-chain gate를 닫는다.

#### Cycle 125 — Windows installer, MSRV, and supply-chain truth

- ISSUE: 서비스 설치·수리·제거, ACL·SID·서명 검증, 원자 교체, 고정 Rust 버전과 SBOM/audit가 하나의 출시 계약으로 증명되지 않았다.
- RED: installer contract, release policy, unsigned fixture fail-closed, pinned toolchain/action, SBOM parser를 검증하는 테스트를 추가했다.
- FIX: install/repair/uninstall 스크립트에 서비스 계정·ACL·SID·서명 정책과 atomic replace/flush 계약을 연결했다. MSRV 1.91.0과 release 1.97.0을 명시하고 CI action을 commit SHA로 고정했으며 CycloneDX, cargo-audit, cargo-deny, npm audit gate를 추가했다.
- VERIFY: Rust 1.91.0 all-target locked check, 현재 toolchain workspace clippy `-D warnings`, 20개 Rust SBOM과 167-component npm SBOM parse, cargo audit/deny와 npm production audit가 통과했다. 허용된 upstream 경고는 성공으로 숨기지 않고 증거에 남겼다.
- NEXT: 실제 Windows 개발 fixture에서 전체 설치 수명주기를 실행한다.

#### Cycle 126 — actual Windows lifecycle and SCM drain correction

- ISSUE: 상승 권한 Windows fixture의 첫 update에서 서비스가 `STOP_PENDING`에 고착됐다. 운영 drain을 SCM shutdown으로 보고해 IPC가 끊기는 상태 모델 오류였다.
- RED: operational drain 중 SCM 상태와 accepted control을 검증하는 `scm_contract` 회귀 테스트가 기존 매핑에서 실패했다.
- FIX: 작업 claim만 중단하는 `Draining`은 IPC가 살아 있는 SCM `Running`으로 보고하고, 실제 Stop/Preshutdown 신호 뒤에만 별도 shutdown-pending 상태를 보고하도록 분리했다.
- VERIFY: 개발 fixture에서 clean install, status/diagnostics, repair, 비동기 support bundle, 14개 update stage, control backup/rehearsal, uninstall 후 운영 저장소·4개 control backup·1개 support bundle 보존을 확인했다. 검증 뒤 이 실행이 만든 `C:\Program Files\DbBackup`, `C:\ProgramData\DbBackup`과 임시 package만 제거했으며 service/process가 남지 않았다.
- NEXT: release workflow의 clean-fixture 주장과 증거 신선도를 공격한다.

#### Cycle 127 — signed-fixture policy fail-closed

- ISSUE: release workflow가 G1 이전의 정상적인 signed clean fixture에서도 `claimsEnabled=true`를 기대해 rehearsal-only 상태를 production-ready로 오판할 수 있었다.
- RED: G1 없는 signed fixture는 `rehearsal_ready`, `claimsEnabled=false`여야 한다는 release-policy 회귀 행을 추가했다.
- FIX: workflow assertion을 rehearsal 경계에 맞추고 production Authenticode, RFC3161 timestamp, signed clean/update fixture가 없으면 해당 승격 증거를 `not_run`으로 유지하도록 했다.
- VERIFY: release-policy 테스트와 G5 release check가 통과했다. 로컬 unsigned lifecycle 성공은 signed clean-VM 증거를 대신하지 않는다.
- NEXT: G0 evidence가 현재 source tree와 동일하다는 보장을 추가한다.

#### Cycle 128 — G0 process and source-identity false-green closure

- ISSUE: Windows PowerShell 5.1 child에서 `param()` 기본식의 `$PSScriptRoot`가 비어 fresh G0 collector가 즉시 종료됐지만, 과거 evidence가 남아 재사용될 수 있었다. G5도 G0를 현재 source tree digest에 결합하지 않았다.
- RED: child-process 기본 root, digest propagation, stale G0 tree, missing identity property 행을 추가해 실패를 재현했다.
- FIX: repository root를 script body에서 resolve하고 tracked+untracked nonignored source의 canonical digest를 G0 root/각 command에 기록했다. G0 truth와 managed G5가 현재 digest 및 모든 identity property를 검증하며 schema 누락은 안정적인 `EVIDENCE_SCHEMA_INVALID`로 실패한다.
- VERIFY: collector 3/3, G0 truth 17/17, release-matrix 회귀 suite가 통과했고 fresh G0의 6개 command가 동일 digest로 새로 발행됐다.
- NEXT: frontend managed gate의 깨끗한 재현성을 확인한다.

#### Cycle 129 — isolated Playwright server and clean frontend gate

- ISSUE: 이전 Playwright 실행이 남긴 이 workspace의 Vite process가 Rollup DLL을 점유해 `npm ci`가 `EPERM`으로 실패했고 G4 UI/browser check도 연쇄 실패했다.
- RED: workspace process tree를 확인해 7시간 이상 남은 정확한 Vite orphan 네 개와 lock을 특정했다.
- FIX: 해당 orphan PID만 종료하고 Playwright의 `reuseExistingServer`를 false로 고정해 테스트가 자신이 시작한 server만 사용하고 회수하도록 했다. unrelated process와 기존 Docker workload는 건드리지 않았다.
- VERIFY: clean `npm ci` 0 vulnerabilities, Vitest 11/11, production build, Playwright 27/27 통과. 종료 뒤 이 workspace의 Vite process는 0개였다.
- NEXT: 변경 없는 동일 tree에서 전체 managed G0–G5를 한 번에 재발행한다.

#### Cycle 130 — final managed G0–G5 evidence and completion boundary

- RUN: `scripts/test-release-matrix.ps1 -RunManaged -RequireUpstreamEvidence`를 변경 없는 tree에서 실행했다.
- IDENTITY: revision `bd6a97145db03fe2ccb2fa46e453f84f1e1ca575`, source tree digest `e1e19e2c9402807f5d4db1172dfed3ccabb9dbfead4c0079c9e83e9969d076d6`, build ID `f60bf75e8e3cc8a93a47599344204460f3efabffab0715e0f0f05137aaf2012e`.
- VERIFY: managed 7/7와 G5 8/8이 모두 exit 0이다. G0 6/6, G1 9 evidence rows, G2 PostgreSQL/MySQL/MariaDB 3/3와 contract 4/4, G3 remote/PostgreSQL restore, G4 6/6, G5 update/support/telemetry/installer/release 8/8이 동일 source identity로 통과했다. 직접 G0 truth도 `evidenceGate=true`, `durableGate=true`를 반환했다.
- HYGIENE: `cargo fmt --all -- --check`, `git diff --check`가 통과했고 DbBackup service와 workspace Vite process는 0개다. 테스트 fixture container는 정리했으며 사전 존재한 unrelated workload는 보존했다.
- STATE: V2-R10은 `proved`다. 전체 계약은 R01–R08/R10 proved, R09의 SQLite 및 SQL Server candidate 구현 proved/SQL Server live BACPAC만 parked다.
- PARKED: `promotionStatus=parked`는 코드 실패가 아니다. production Authenticode, RFC3161 timestamp, signed clean/update Windows fixture, owner rollout-stop metric 승인과 실제 SQL Server fixture는 외부 자격증명·인프라·제품 결정이 필요하며 Owner Decision Brief에 인계했다.
- DONE: 실행 가능한 인스코프 구현과 검증을 모두 소진했고 최종 증거는 `artifacts/g5/release-matrix.json`에 귀속된다.

---

## Archived Run: 원격 복구 경로 완결

## Run Metadata

- Started: 2026-07-23
- Project: `D:\git\dbbackupsystem`
- Branch: `main`
- Engine: native goal + direct Chronos loop
- Scope: 현재 작업 트리의 원격 manifest source 구현을 보존하면서 콘솔 reader 신원 등록, 재현 가능한 S3/SSH 복구 드릴, 전체 품질 게이트까지 마감

## Completion Contract

| Requirement | Pass evidence | State |
|---|---|---|
| R1. 콘솔에서 S3/SSH writer와 분리된 reader 신원을 입력하고 비밀값을 암호화해 저장한다. | Tauri 설정 편집 단위 테스트의 RED/GREEN 기록, `npm run build`, 저장된 TOML 라운드트립 | proved |
| R2. S3와 SSH 목적지의 매니페스트·암호문을 원격 reader 신원으로 직접 읽어 catalog rebuild/verify/restore 경로에 공급한다. | 실제 MinIO/OpenSSH 격리 인프라에서 S3 1 passed + SSH 1 passed | proved |
| R3. 명시한 원격 목적지 오류가 로컬 spool로 조용히 폴백하지 않고, 고정된 서명 공개키로 검증한다. | CLI·catalog·verify 회귀 테스트 및 라이브 reader write-deny/pinned-signature 단언 | proved |
| R4. Rust와 콘솔의 전체 정적·동적 검증 게이트가 현재 작업 트리에서 통과한다. | fmt, diff-check, clippy `-D warnings`, 201 passed/0 failed, 콘솔 build, cargo-deny/audit/npm audit 종료 코드 0 | proved |
| R5. 운영자가 reader 설정과 원격 복구 드릴을 재현할 수 있다. | 예제 설정·README·`scripts/test-remote-recovery.ps1`의 S3/SSH 결합 실행 통과 | proved |

## Verification Gates

1. `cargo fmt --all -- --check`
2. `git diff --check`
3. `cargo clippy --workspace --all-targets -- -D warnings`
4. `cargo test --workspace`
5. `npm run build` in `apps/console`
6. 원격 S3/SSH 복구 드릴 테스트
7. 설치되어 있고 실행 가능한 경우 `cargo deny check`와 `cargo audit`

## Cycle Log

### Phase 0 — 2026-07-23

- READ: `codemap/index.md` → `apps-console.md`, `MEMORY.md`, `memory/architecture.md`, `README.md`, `DESIGN.md`, 설계 문서와 현재 diff.
- FIND: 백엔드 `ReaderIdentity`와 원격 `ManifestSource`는 구현됐지만 콘솔 `NewDestination`/등록 화면은 reader 필드를 노출하지 않는다. 라이브 S3/SSH 복구 소스는 아직 재현 가능한 인프라 게이트 증거가 부족하다.
- NEXT: R1을 첫 단일 이슈로 삼아 실패 테스트부터 구현한다.

### Cycle 1 — S3 reader 저장 계약

- FIND: `NewDestination`에 reader 입력이 없고 저장 시 `reader: None`으로 고정됐다.
- RED: `cargo test -p dbbackup-console add_destination_s3_encrypts_separate_reader_identity --no-run`이 `reader`/`NewReaderIdentity` 부재로 실패했다.
- FIX: 중첩 `NewReaderIdentity` 입력과 S3 reader Access Key/Secret의 KEK 기반 EncField 저장을 추가했다.
- VERIFY: `add_destination_s3_encrypts_separate_reader_identity` 1 passed.
- NEXT: SSH reader 개인키/비밀번호 저장 계약.

### Cycle 2 — SSH reader 저장 계약

- FIND: SSH reader 사용자·개인키·암호문·비밀번호가 콘솔 저장 경계에서 소실됐다.
- RED: 개인키 테스트는 `reader 저장` 단언에서 실패했고, 비밀번호 테스트는 reader 인증 거부로 실패했다.
- FIX: 개인키 또는 비밀번호 중 하나만 허용하고 passphrase/password만 EncField로 암호화하도록 구현했다.
- VERIFY: `cargo test -p dbbackup-console add_destination_ssh_encrypts_reader` 결과 2 passed.
- NEXT: 복구 reader 없는 원격 목적지 생성 차단.

### Cycle 3 — 복구 불가능한 GUI 설정 차단

- FIND: GUI 커맨드가 S3/SSH reader 없이도 목적지를 저장하고, Object Lock checksum을 누락하며, schema validator가 거부할 설정도 디스크에 기록했다.
- RED: reader 없는 S3·SSH 거부 테스트, endpoint 누락 schema 테스트, Object Lock checksum 단언이 각각 기대한 이유로 실패했다.
- FIX: 원격 reader 필수화, Object Lock 시 SHA-256 checksum 고정, 저장 전 전체 config validation을 추가했다.
- VERIFY: S3 관련 3 tests passed, schema-invalid 테스트 1 passed, 각 SSH reader 테스트 passed.
- NEXT: React 입력 화면과 Tauri payload 연결.

### Cycle 4 — reader 권한 경계 UI

- FIND: React 폼과 TypeScript API에 reader 타입·필드·master-password 조건이 없었다.
- FIX: S3 writer/reader와 SSH writer/reader를 별도 fieldset으로 구성하고, SSH key/password 선택, reader 비밀 payload, 컨테이너 쿼리, `:has()` focus 경계를 추가했다.
- VERIFY: `npm run build` 통과. Playwright에서 S3 전체 필드 입력→버튼 활성화→mock 등록 성공, SSH key/password 전환, 1440px/768px 무가로오버플로, Space Grotesk/JetBrains Mono 및 Pretendard face 로드, 콘솔 오류 0을 확인했다.
- NEXT: 실제 S3/SSH 프로토콜 기반 원격 복구 드릴.

### Cycle 5 — OpenDAL 실제 HTTP 초기화

- FIND: 첫 MinIO 드릴에서 S3 write가 `default HTTP transport is not installed`로 실패했다. `default-features=false`가 ctor 자동 등록을 끈 상태였다.
- FIX: S3 writer와 reader 독립 생성 경로 모두에서 안전하고 멱등인 `opendal::install_default()`를 호출했다.
- VERIFY: 같은 MinIO 드릴이 네트워크 write를 통과해 reader list 단계까지 진행했다.
- NEXT: MinIO reader 정책을 실제 최소권한으로 고정.

### Cycle 6 — S3 read-only 복구 드릴

- FIND: MinIO 내장 readonly 정책이 이 fixture의 ListBucket 요청을 403으로 거부해 명시 정책 증거가 필요했다.
- FIX: bucket List/Get/GetBucketLocation만 허용하는 전용 IAM policy fixture를 만들고 reader 사용자에 부착했다.
- VERIFY: 실제 S3 API에서 writer 업로드, reader catalog rebuild, pinned-signature gate, L1, decrypt-only 복원 모두 통과했고 reader write probe는 거부됐다.
- NEXT: SSH/SFTP 동일 폐곡선.

### Cycle 7 — SSH read-only 복구 드릴

- FIND: 격리 SFTP fixture의 공유 볼륨 소유권과 비동기 host-key 생성 시점 때문에 초기 harness가 실패했다.
- FIX: writer UID/GID 소유권과 writer 소유 0755 디렉터리 경계, host-key 생성 retry를 명시하고 digest 고정 이미지를 사용했다.
- VERIFY: 실제 SSH/SFTP에서 별도 writer/reader 계정으로 catalog rebuild, pinned-signature gate, L1, decrypt-only 복원이 통과했고 reader write probe가 거부됐다. 결합 실행 `scripts/test-remote-recovery.ps1`은 S3 1 passed + SSH 1 passed, 잔여 컨테이너·볼륨 0.
- NEXT: 전체 workspace/정적/보안 게이트.

### Cycle 8 — SSH RSA 취약 의존성 제거

- FIND: `cargo audit`이 russh 선택 기능으로 유입된 `rsa 0.10.0-rc.18`의 RUSTSEC-2023-0071을 보고했다.
- FIX: SSH RSA 기능을 제거하고 Ed25519/ECDSA만 허용했으며, audit/deny/CI의 과거 RSA 예외도 삭제했다.
- VERIFY: `rsa`가 Cargo 그래프에서 사라졌고 실제 SSH/SFTP 복구 드릴이 다시 통과했다.
- NEXT: 전체 테스트의 비결정적 실패 제거.

### Cycle 9 — 콘솔 테스트 임시 경로 격리

- FIND: 프로세스 ID만 사용한 테스트 디렉터리가 이전 강제 종료의 잔여 경로와 충돌해 목적지 중복 실패를 만들었다.
- FIX: 콘솔 설정·키스토어 테스트를 `tempfile::TempDir` 기반 난수 경로와 자동 정리로 전환했다.
- VERIFY: 문제 테스트 연속 2회 통과 후 전체 워크스페이스에서도 통과했다.
- NEXT: 실제 CI 공급망 게이트 실행.

### Cycle 10 — 공급망 게이트 복구

- FIND: 설치 후 실행한 `cargo deny check`이 yanked OpenDAL, 0.57 하향 시 quick-xml 취약점, 내부 크레이트 라이선스/경로 정책 누락을 탐지했다.
- FIX: OpenDAL v0.58.0의 Apache 공식 해제 커밋을 rev로 고정하고 허용 Git 출처를 단일화했다. 모든 내부 크레이트가 MIT·비배포 정책을 상속하도록 했고, Tauri의 수정 불가능한 전이 미유지 경고만 비차단으로 분류했다.
- VERIFY: `cargo deny check` 결과 advisories/bans/licenses/sources 모두 ok, `cargo audit` 취약점 0, `npm audit` 취약점 0.
- NEXT: 최종 실행 게이트.

### Cycle 11 — 최종 게이트

- VERIFY: `cargo fmt --all -- --check`, `git diff --check`, `cargo clippy --workspace --all-targets -- -D warnings` 통과.
- VERIFY: `cargo test --workspace` 201 passed / 0 failed / 10 ignored. 범위 내 ignored 2건은 라이브 스크립트에서 S3 1 passed + SSH 1 passed.
- VERIFY: `npm run build` 통과. Playwright 검증은 reader 등록, 반응형, 폰트 로드, 콘솔 오류 0을 확인했다.
- RESULT: R1–R5 proved, parked 0, remaining 0.
