# Section 04 — Durable Dispatcher and Process Supervisor

## Background

현재 CLI job loop는 한 process에서 순차 실행하고 fan-out만 병렬이며, job별 Task Scheduler는
서로 다른 process를 동시에 시작할 수 있다. `max_parallel_dumps`는 실제 permit으로 소비되지
않는다. Rust thread 수가 문제가 아니라 native dump, CPU encode, spool disk, source host,
Destination network가 무제한으로 겹치는 것이 위험하다.

이 섹션은 모든 장기 작업을 persistent queue로 통합하고 crash 뒤 수렴 가능한 실행 코어를
만든다. G1을 통과한 뒤에만 section-03 cutover가 production authority를 service로 옮긴다.

## Requirements

### Queue and occurrence

- scheduled/catch-up occurrence는 revision+UTC due, manual은 client idempotency key로 upsert한다.
- 한 occurrence의 Target별 Backup Run은 하나이며 retry는 같은 Work Item의 새 Attempt다.
- Backup/Restore/Verification/Catalog Rebuild/Operation Run이 같은 queue 계약을 사용한다.
- due 계산은 timezone/DST, bounded latest-one catch-up, overlap coalesce-one을 결정적으로
  기록하고 missed reason을 남긴다.

### Lease and fencing

- claim은 짧은 `BEGIN IMMEDIATE`에서 attempt no와 Fencing Token을 증가시키고 lease/owner/
  service boot ID를 기록한다.
- heartbeat, checkpoint, state transition, allocation release, terminal commit은 Work Item,
  attempt, fence, owner predicate가 모두 일치해야 한다.
- lease 만료는 성공/실패가 아니라 Reconciliation 진입이다.
- stale Attempt affected row 0은 즉시 process 중단·격리·reconcile 신호다.

### Resource admission

초기 보수 기본값은 다음과 같다.

| Pool | 기본 capacity |
|---|---:|
| global backup | 2 |
| source host | 1 |
| CPU encode | 1; benchmark 후 최대 2 |
| destination | 2 |
| restore | 1 |
| database exclusive | 1 |

필요한 모든 resource key와 spool byte를 canonical 순서로 계산하고 한 transaction에서 전부
예약하거나 하나도 예약하지 않는다. 부분 permit을 보유한 채 다음 permit을 기다리지 않는다.
부족하면 wait reason과 next eligible time만 기록한다.

### Process Supervisor

- shell을 통하지 않고 검증된 absolute executable과 argument vector를 사용한다.
- executable hash/signature/tool certification을 실행 직전에 재검사한다.
- allowlist된 restricted environment만 만들고 secret을 command line에 넣지 않는다.
- stdout/stderr를 동시에 bounded drain하고 redaction된 progress/error만 Activity에 기록한다.
- Windows Job Object로 child/grandchild tree를 소유하고 breakaway를 차단한다.
- PID만 믿지 않고 process start time, service boot ID, Job Object key를 기록한다.
- cancel/timeout/SCM stop은 graceful signal→bounded wait→tree termination→pipe drain→artifact
  격리 순서다.

### Reconciliation

startup과 주기 sweep은 Operational Store, process identity, staging/spool, signed Manifest,
Destination object를 함께 비교한다.

- valid complete Manifest가 이미 있으면 같은 Recovery Point ID로 projection/terminal state 수렴
- process가 살아 있고 current owner면 monitor 재개
- owner가 다르거나 lease 만료면 이전 fence 차단
- partial artifact는 quarantine/repair/cleanup
- retryable이고 budget/deadline 안이면 새 Attempt
- 설명 불가능한 상태를 성공으로 추정하지 않고 Attention

Activity append burst가 heartbeat writer를 굶기지 않도록 priority lane과 bounded batch를
사용한다.

## Dependencies

- **Requires**: section-02 domain/store; section-03의 application/service interface와 병렬 가능
- **Blocks**: section-05 Recovery Point/storage, section-06 engine certification
- **Cutover**: section-03과 이 섹션의 G1 evidence가 같은 build에서 전부 pass한 뒤

## Flow Diagram Nodes

### `scheduled-backup-dispatch.mmd`

이 섹션은 `tick`, `loadDue`, `upsertOccurrence`, `occurrenceExists`, `noDuplicate`,
`createWork`, `reserveResources`, `deferWork`, `queueWait`, `claimAttempt`, `launchProcess`,
`heartbeat`, `processExit`, `releaseAndRetry`, `recordTerminal`, `failed`를 소유한다.

- 중복 occurrence는 enqueue 없이 종료한다.
- permit 부족은 queued/delayed 유지다.
- process 성공은 section-05의 `stageArtifacts`로 넘긴다.
- retryable exit는 모든 allocation을 반환한 뒤 backoff한다.

### `crash-reconciliation.mmd`

`boot`, `scanClaims`, `inspectProcess`, `ownerMatches`, `resumeMonitor`, `fenceOld`,
`inspectArtifacts`, `committedElsewhere`, `rebuildProjection`, `markSucceeded`, `partialFound`,
`quarantineOrRepair`, `classifyRetry`, `requeue`, `markFailed`, `reconciled` 전체를 소유한다.
DB/process/artifact 세 증거 중 하나만 보고 terminal 판정하지 않는다.
`committedElsewhere=예`만 projection 복구, partial은 quarantine, retry budget 초과는 terminal
failure다.

## Reference Libraries

| Library/API | 기준 | 용도 |
|---|---|---|
| `tokio` | workspace pin | scheduler, bounded channels, process I/O |
| `rusqlite =0.31.0` | 유지 | queue/CAS/admission |
| `windows` crate/Job Objects | 구현 시 공식 문서 확인 | process tree와 service stop |
| `cron`/현행 scheduler parser | lockfile 확인 | occurrence 계산 |
| `rand` | workspace pin | full-jitter backoff |
| `tracing` | workspace pin | structured redacted telemetry |

## Implementation Details

### 1. Runtime crate

`dbbackup-runtime`을 queue, dispatcher, lease, permits, supervisor, reconcile, spool 모듈로
분리한다. adapter 실행은 trait로 주입하고 domain/application type만 소비한다.

### 2. Scheduler and queue

service tick은 활성 revision을 읽고 occurrence insert-or-select와 Target Run/Work Item 생성을
transactionally 수행한다. manual run도 같은 path다. 한 tick의 DB transaction을 짧게 유지하고
대규모 due는 bounded batch로 나눈다.

### 3. Dispatcher

priority/deadline/available time으로 후보를 고른 뒤 resource request를 계산한다. admission과
claim을 writer actor operation으로 실행하고 commit 뒤 supervisor task를 spawn한다. dispatcher
task 자체가 blocking DB/native I/O를 수행하지 않는다.

### 4. Lease monitor

heartbeat interval은 lease보다 충분히 짧고 jitter를 둔다. DB busy나 Activity burst로 budget을
넘으면 metric/Attention을 만든다. renewal 실패는 계속 실행하지 않고 supervisor cancel과
Reconciliation을 시작한다.

### 5. Supervisor

tool registry에서 absolute path/hash를 얻어 `Command`를 구성한다. working directory와 temp/spool
path를 attempt별로 분리하고 inherited handle/env를 allowlist한다. stdout/stderr reader가 먼저
준비된 뒤 child를 시작한다. exit code, signal/timeout, bytes, phase를 process row에 남긴다.

### 6. Spool and release

예상 크기+floor를 미리 예약한다. 실제 크기가 예약을 초과하면 soft limit에서 throttle,
hard limit 전에는 안전하게 fail한다. terminal/reconcile 후 allocation과 reservation을 fence
조건으로 반환하며 cleanup 실패는 orphan Attention으로 남긴다.

### 7. Cutover evidence

G1 evidence는 occurrence uniqueness, stale fence rejection, process tree reaping, checkpoint
crash reconciliation, spool reservation, permit limits, required-destination truth,
single-service path, Operational Store online backup의 아홉 항목이다. 같은 revision/build의
all-pass만 section-03 cutover를 호출한다.

## Test Scenarios

### Normal

| ID | 입력 | 기대 |
|---|---|---|
| S04-N01 | 동일 host 16개 due, global2/host1 | 한도 준수와 eventual completion |
| S04-N02 | 같은 scheduler tick 두 번 | occurrence/Run/Work 중복 0 |
| S04-N03 | manual request retry | 같은 occurrence의 새 Attempt |
| S04-N04 | generic probe Operation Run | 같은 lease/fence/cancel 계약 |
| S04-N05 | process success | allocation 반환 후 section-05 staging |

### Error/Fault

| ID | 입력 | 기대 |
|---|---|---|
| S04-E01 | source permit 부족 | allocation 0, wait reason |
| S04-E02 | stale fence heartbeat/commit | affected row 0, reconcile |
| S04-E03 | disk full before reservation | native process 0회 |
| S04-E04 | stderr flood | deadlock/메모리 폭증 없이 bounded drain |
| S04-E05 | child가 grandchild spawn | Job Object cancel로 tree 종료 |
| S04-E06 | executable path/hash swap | 시작 전 차단 |
| S04-E07 | service kill at leased/starting/running/committing | 상태별 결정적 수렴 |
| S04-E08 | Manifest publish 후 DB crash | duplicate RP 없이 projection 복구 |

### Edge

| ID | 입력 | 기대 |
|---|---|---|
| S04-X01 | PID 재사용 | start time/boot ID 불일치 process kill 금지 |
| S04-X02 | Activity 100k burst | heartbeat freshness budget 통과 |
| S04-X03 | retryable 오류 backlog | jitter/budget/deadline로 bounded |
| S04-X04 | cancel과 process exit 경합 | terminal 전이 하나, permit 반환 하나 |
| S04-X05 | DST 반복/절전 복귀 | ledger/missed/coalesce 결정적 |
| S04-X06 | G1 한 항목 stale build | actual cutover 거부 |

## Implementation Strategy

### Red

deterministic fake clock/process/destination으로 occurrence, CAS, admission, cancel race,
phase별 crash, PID reuse test를 먼저 실패시킨다.

### Green

persistent claim loop, 기본 permit, lease heartbeat, no-shell supervisor, startup reconciliation을
최소 구현한다. G1 전 production claim은 활성화하지 않는다.

### Refactor

runtime state machine과 I/O adapter를 분리하고 writer transaction budget, structured phase/error,
fault injection seam을 정리한다. unbounded channel/task spawn을 제거한다.

## Quality Gate G1

- [ ] occurrence당 Target Run 1, complete Recovery Point 최대 1
- [ ] stale fence heartbeat/commit 허용 0
- [ ] global/host/CPU/destination/restore permit 초과 0
- [ ] partial permit 대기와 spool overcommit 0
- [ ] child/grandchild orphan 0, PID reuse 오판 0
- [ ] 각 phase service kill 뒤 설명 가능한 수렴
- [ ] heartbeat starvation budget 통과
- [ ] 아홉 G1 evidence 동일 revision/build pass
- [ ] G1 전 service production 실행 0, G1 cutover 뒤 legacy 실행 0

## Risk and Rollback

| Risk | Mitigation | Rollback/Forward action |
|---|---|---|
| queue 상태 버그 | pure transition+CAS+fault tests | claim disable, state reconcile |
| 과도한 동시성 | 보수 default와 runtime metrics | capacity 1로 축소 |
| process orphan | Job Object와 boot/start identity | drain 후 service restart/reconcile |
| spool 고갈 | pre-reservation/floor | 새 admission 중지, 격리 cleanup |
| crash 중 이중 commit | fence+Manifest identity | projection rebuild, stale reject |
| 조기 cutover | all-pass G1 | marker 전 legacy 유지; 이후 forward-fix |

## Acceptance Criteria

- [ ] 모든 장기 작업이 persistent Work Item/Attempt로 보인다.
- [ ] Rust thread 수가 아니라 실제 resource permit으로 부하를 제한한다.
- [ ] service restart가 queue와 artifact를 설명 가능한 상태로 수렴시킨다.
- [ ] cancel/timeout이 전체 process tree와 pipe를 정리한다.
- [ ] stale worker가 terminal state나 Recovery Point를 commit하지 못한다.
- [ ] G1을 통과한 단일 build만 execution authority를 service로 전환한다.

## Files to Create/Modify

```text
crates/dbbackup-runtime/src/{lib,queue,dispatcher,lease,permits,supervisor,reconcile,spool}.rs
crates/dbbackup-runtime/tests/{occurrence,admission,fencing,supervisor,reconciliation}.rs
crates/dbbackup-catalog/src/repositories/runtime.rs
crates/dbbackup-scheduler/src/{occurrence,service_wake}.rs
crates/dbbackup-service/src/{host,executor}.rs
crates/dbbackup-cli/src/commands/run.rs
apps/console/src-tauri/src/run.rs
release/gates/g1-evidence-requirements.json
scripts/test-g1-fault-matrix.ps1
Cargo.toml
Cargo.lock
```
