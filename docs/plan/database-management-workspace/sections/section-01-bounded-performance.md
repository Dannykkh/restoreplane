# Section 01: Bounded Windows/WebView Performance Proof

## Background

P2 is the risk that full result loading, serialization, queues and DOM rendering make the Workbench unbounded or unresponsive. Keep Tauri/React/Rust. Before feature expansion, measure the existing Windows/WebView stack with an isolated synthetic native producer and virtualized benchmark surface. This is an independent Wave 1 assumption spike, not the production DB driver, query engine or schema browser.
Planning supplies no measured performance claim; all budgets below are proposed thresholds until raw evidence exists.

## Requirements

- Run a real Tauri window on named Windows hardware/WebView2, not Chromium-only Playwright, jsdom or a browser mock.
- Deterministically generate 100,000 logical rows × 100 typed columns lazily in native code; do not preload ten million cells anywhere.
- Include NULL/empty, exact integer/decimal strings, Unicode/Korean text, bytes and temporal metadata; preserve value hashes through the benchmark transport.
- Fix seed, value-size distribution, viewport/DPI, overscan, build flags, warm-up and interaction script in a hashed sample manifest.
- Record 200 timestamped keyboard/scroll/cancel interactions per measured run; proposed protocol is one warm-up and five measured runs without silently discarding outliers.
- Capture input-response p95, frame durations/gaps, idle and peak process-tree working set, queue/credit/cache bytes and mounted DOM counts as raw samples.
- Bound encoded batch at 256KiB, outstanding credit at 8MiB, retained frontend pages at 32MiB and native result buffers at 64MiB; inline cells at 64KiB.
- Frame credit is reserved before enqueue/serialization; duplicate/stale ACK releases zero, future ACK rejects, and terminal/control capacity remains available.
- Delay, suppress or corrupt ACK; inject oversized cell/frame and producer cancellation. Stop/pause safely without accumulating queued rows.
- Benchmark exports contain synthetic values only. No live DB, service secrets, target mutations, production navigation or new engine dependencies.

## Dependencies

- Requires: none. Existing repository/toolchain is the baseline, not a dependency on another implementation section.
- Blocks: section-04-schema-explorer, which consumes measured budgets and the reference bounded-render workload; section-05 later repeats relevant measurements after its integration.
- May run in parallel with section-02-sessions-and-credentials; it cannot import that section's unfinished runtime.

## Module Contract

- **Provides:** `BenchScenario {name,seed,rows,columns,valueDistribution,viewport,overscan,ackPolicy,limits}` and `MeasurementRecord {runId,environment,samples,counters,M1,M2,M3,artifactHashes}`.
- **Provides:** named executable scenarios `workbench.baseline`, `workbench.bounds`, `workbench.slow_consumer`, `workbench.metrics`, with nonzero exit on violated bounds or missing mandatory samples.
- **Consumes:** existing Tauri/WebView launch infrastructure, React rendering and OS process counters; no production driver/session/journal/secret API or future-section module.
- **Owns:** `apps/console/bench/workbench-performance.ts`, `scripts/measure-workbench.ps1` and synthetic run output under `artifacts/workbench-performance/<run-id>/`.
- **Composition Point:** isolated benchmark route/build flag in the existing Console harness; the implementation lead alone registers benchmark entry/native command in shared App/Tauri/build files. Benchmark registration is absent from normal shipped navigation/builds.
- The script may generate a test-only native producer/harness in its private staging directory and include its generated source/hash in the run artifact. It never becomes `restoreplane-workbench` or edits production driver ownership.
- No public REST/MCP/application command is added. If implementation needs one, revise its contract and `ipc-spec.md` before adding it; a hidden benchmark command is not a production API shortcut.

## Flow Diagram Nodes

- Diagram: `flow-diagrams/browse-stream.mmd`; exercise `credit`, `wait`, `send`, `ack`, `apply`, `expired`, `stop` using synthetic frames.
- Credit unavailable pauses; deadline reached interrupts; invalid ACK rejects; terminal is independently deliverable. No query or target mutation occurs.

## Reference Libraries

| Library / tool | Version basis | Purpose |
|---|---|---|
| Tauri / @tauri-apps/api | Existing Rust lockfile and Console lockfile; manifests currently 2 / ^2.1.1 | Real native-to-WebView test transport; consult matching official docs before implementation. |
| React / TypeScript / Vite | Existing Console lockfile; manifest React ^18.3.1 | Synthetic virtualized grid and timestamp capture; preserve existing build chain. |
| PowerShell / Windows process counters / WebView2 | Record exact executable/runtime/OS versions in environment.json | Launch, process-tree memory sampling and artifact collection. |
| Existing Playwright/Vitest | Repository lockfile versions | Script/schema/counter unit checks only; browser results do not replace Windows/WebView measurements. |

Do not add a virtualization or benchmark dependency before checking existing utilities and documenting its version/license/advisories. No DBDelve source or pins are copied.

## Implementation Details

1. `measure-workbench.ps1` resolves repository and a unique owned staging/output root, validates Windows/toolchain and captures OS build, CPU, RAM, GPU, display refresh/DPI, WebView2, Rust/npm versions, git revision, dirty state and build command/flags.
2. Generate the test-only native producer and bind a benchmark-only Channel/pull interface. Native generation yields bounded frames and awaits credit; it never materializes the complete dataset or delegates generation to JavaScript.
3. The TypeScript surface mounts only viewport rows/columns plus fixed overscan. Record configured maxima and assert mounted cells do not exceed `(visibleRows+2*rowOverscan)*(visibleColumns+2*columnOverscan)` plus separately counted headers/controls.
4. Capture raw encoded bytes, native retained bytes and JS decoded/cache estimates separately; encoded credit is not a claim about actual heap. Account for serialization copies and process-tree descendants in M2.
5. Baseline after launch/idle stabilization; timestamps use monotonic clocks with recorded native/frontend synchronization method. Input latency ends at observed UI response, not merely event handler entry. Store raw input and frame traces.
6. Bounds scenario scrolls to distant logical rows/columns and evicts/reloads pages. Validate seeded value hashes, stable selection/focus anchors and non-growing retained state across the full workload.
7. Slow-consumer cases use delayed ACK, no ACK and invalid ACK; proposed no-ACK idle deadline is 5s, declared in manifest before execution. Preserve control/terminal reserve and record deadline-to-visible-terminal latency; no silent success after interruption.
8. Reject a cell larger than 64KiB inline or a frame larger than 256KiB before queue admission. Report explicit oversize termination with bounded metadata; do not allocate arbitrary declared lengths or silently truncate exact values.
9. Expose independent Stop/cancel control while producer waits; retire stream generation and release each reservation once. Benchmark completion/cancel race reports observed terminal without inventing transaction rollback.
10. Write environment.json, dataset.json, raw-input.jsonl, frames.jsonl, memory.csv, counters.jsonl and summary.json; include sample counts, hashes, exits and failures. Missing permissions/counters/runtime produce DATA NONE/NOT RUN, not fabricated zeroes.

## Test Scenarios

| Name / QA | Input | Expected |
|---|---|---|
| bench workbench.baseline / T2 | Fixed environment/sample manifest, warm-up and five × 200 interactions | Raw baseline input/frame/memory observations and reproducible dataset hashes. |
| bench workbench.bounds / T2 | Lazy 100k × 100 typed cells; full scroll sweep | All configured byte/credit/DOM bounds hold; exact seeded hashes match; no total-result retention. |
| bench workbench.slow_consumer / T2 | Slow/no/duplicate/future ACK; oversized cell/frame; Stop while paused | Pause/reject/interrupt correctly, no credit over-release or growth; control/terminal remains responsive. |
| bench workbench.metrics / T2 | Collected raw files; missing sample; injected bound violation | Derive M1/M2/M3 and tripwire decision; missing data is not PASS, injected violations yield nonzero exit. |

## Implementation Strategy

- Red: define sample/metric schemas and counter assertions; demonstrate missing sample and deliberately overflowing producer fail. These are measured-bound checks, not implementation-mirroring tests.
- Green: implement minimal benchmark-only native producer, credit path and virtualized surface, then run on actual named Windows/WebView hardware.
- Refactor: reduce measured copies/render work within Tauri; rerun only affected scenarios after changes. Keep original threshold evaluation and raw failed runs when tuning.

## Quality Gate

- Existing Console build/type checks and affected tests pass; benchmark-only build/route is unavailable in production packaging.
- Every scenario records its exact command and exit; generated staging paths are verified within this run's owned root before cleanup.
- Shared composition edits are integrated by the implementation lead; no source/ownership overlap with sections 02–08.
- Results distinguish benchmark synthetic workload from certified DB throughput. W1 records M1/M2; W2 benchmark boundary checks emit M3 but do not claim production permission/commit fault coverage.
- Implementation lead alone records actual AC evidence/state in checklist-status.md and W1/W2 measurements in implementation-notes.md; this section does not mark unrun criteria proved.

## Risk & Rollback

| Risk | Impact | Mitigation | Rollback |
|---|---|---|---|
| Browser mock or JS-only producer hides native copies | High | Real WebView window/native generator; capture process tree and build flags | Reject evidence and rerun correct harness. |
| No-ACK queue or decoded heap grows despite encoded credit | High | Independent byte/cache/process counters; terminal reserve | Stop feature expansion; bound copies/caches and repeat spike. |
| Input/memory tripwire misses target | High | M1 <=100ms and M2 <=256MiB are proposed continue thresholds; >250ms/>512MiB requires stop/review | Optimize/narrow within Tauri; no automatic framework migration or relaxed safety bounds. |
| Synthetic result falsely certifies real driver | High | Explicit nonproduction producer and later integration reruns | Keep capability uncertified; record evidence limitation. |

M3 allows zero duplicate-apply/credit-overflow/false-terminal boundary incidents; any incident stops affected integration. Missing metrics remain DATA NONE; two consecutive missing cycles trigger early signal. Threshold changes require the lead's recorded owner decision after evaluating the original thresholds.

## Acceptance Criteria

- [ ] AC-01-1 고정된 Windows/WebView2/hardware/build/dataset/sample manifest를 사용해 200개 입력과 frame·idle/peak process-tree memory baseline의 원시 샘플·버전·해시를 재현 가능한 실행 artifact로 남긴다 — 근거: P2 — 검증: bench workbench.baseline, T2
- [ ] AC-01-2 실제 native producer의 100k×100 workload를 탐색하면 outstanding 8MiB·frontend retained 32MiB·native result buffers 64MiB 및 viewport+overscan DOM 상한을 넘지 않고 전체 결과를 적재하지 않는다 — 근거: P2 — 검증: bench workbench.bounds, T2
- [ ] AC-01-3 slow/noACK·잘못된 ACK·oversize cell/frame을 주입하면 producer가 중지/대기/거부하고 byte 예약이 중복 해제되지 않으며 독립 Stop과 정직한 terminal이 응답하고 그 지연 원시값이 기록된다 — 근거: P2 — 검증: bench workbench.slow_consumer, T2
- [ ] AC-01-4 W1/W2가 원시 artifact에서 M1 input p95·M2 incremental process-tree working set·M3 boundary violations와 tripwire 판정을 산출해 implementation ledger에 연결하며 미측정·누락·실패를 PASS로 표시하지 않는다 — 근거: P2 — 검증: bench workbench.metrics, T2, W1/W2

## Files to Create/Modify

- `apps/console/bench/workbench-performance.ts` — benchmark-only surface, scenarios and typed counter/sample contracts.
- `scripts/measure-workbench.ps1` — Windows runner, private synthetic-native staging and raw artifact collection.
- `artifacts/workbench-performance/<run-id>/` — generated source/manifests/raw samples/summary; no real database content.
- Integration request only: existing App/Tauri/build registration files and implementation ledgers; implementation lead owns those edits, not this feature worker.
