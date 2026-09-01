# Section 01 — Truth Baseline and Release Gates

> 선행 섹션이 없는 최초 게이트다. 현재 제품의 코드 경로, 공개 문구, 테스트 증적을 일치시키고
> Durable Core가 검증되기 전에는 configuration cutover를 rehearsal로만 제한한다. 이 문서의
> 작업이 끝나도 production execution authority는 기존 CLI와 job별 Windows Task Scheduler에
> 남아 있다.

## Background

현재 제품에는 실제로 재사용할 가치가 높은 Rust 백업 코어가 있다. PostgreSQL,
MySQL/MariaDB 논리 덤프, Local/S3/SSH fan-out, 백업별 DEK, 서명 Manifest, L1 검증,
decrypt-only 복원, SQLite catalog와 Task Scheduler 등록 경로가 존재한다. 그러나 사용자에게
보이는 약속과 실제 실행 증적 사이에는 다음과 같은 false-green이 있다.

- `test-connection`은 명령만 있고 항상 미구현 오류로 종료한다.
- `Job.verify`와 retention 설정은 저장되지만 backup 완료 뒤 자동으로 시행되지 않는다.
- CLI에서 L2/L3를 요청해도 실제로는 L1만 실행한 뒤 성공할 수 있다.
- 실제 retention delete는 미구현이며 `prune --dry-run`만 동작한다.
- Tauri는 job에 설정된 모든 Destination을 실제 업로드 증적처럼 합성하고, config의
  불변성 설정을 실제 검증된 불변 보관으로 표시한다.
- 실패한 `runs` 기록보다 `backups`의 마지막 성공만 읽으면 최근 실패가 Dashboard에서
  보이지 않을 수 있다.
- Dashboard의 “모든 백업이 안전합니다”, “서명·검증되어 오프사이트에 보관됩니다” 문구는
  Protection Compliance, required Replica, 자동 verification과 RPO를 계산하지 않은 상태에서
  범위를 넘어선 주장이다.
- Tauri의 restore 성공 표시는 현재 실제 동작인 decrypt-only 완료와 실제 DB restore/content
  validation을 구분하지 않는다.
- `#[ignore]`인 live L2/L3/S3/SSH/engine 테스트는 기본 `cargo test --workspace` 성공에
  포함되지 않지만, README의 “구현 완료” 문구는 그 차이를 충분히 드러내지 않는다.
- Tauri, CLI와 job별 Task Scheduler가 직접 `restoreplane.exe`와 native dump process를 시작한다.
  `max_parallel_dumps`는 설정에서 `PipelineCfg`로 복사될 뿐 실제 dispatcher permit이 아니다.

따라서 구현 시작 판정은 `CONDITIONAL GO`다. 이 섹션의 목적은 기능을 많이 추가하는 것이
아니라, 현재 보장 범위를 기계 판독 가능한 claims matrix와 재현 가능한 evidence로 고정하고
과대 표시를 제거하는 것이다. Windows service, Operational Store, durable queue, fencing,
Process Supervisor, Reconciliation은 후속 섹션이 구현한다. 이 섹션은 그 기능이 존재하는 것처럼
stub이나 feature flag를 만들지 않는다.

승인된 설계 경계는 다음과 같다.

- ADR-001: 최종 execution authority는 독립 Windows service singleton이다.
- ADR-002: 제어 상태는 Operational Store, 비밀은 Secret Store, 복구 artifact 사실은 signed
  Manifest가 각각 소유한다.
- ADR-003: 실행은 at-least-once Attempt와 fenced commit을 사용한다.
- ADR-004: complete Recovery Point, Replica Durability, Protection Compliance는 서로 다른
  상태 축이다.
- ADR-005: TOML에서 Operational Store로의 전환은 one-way cutover이며 dual-write하지 않는다.
- ADR-006: service가 Secret Store를 소유하며 Operational Store에는 Secret Ref만 둔다.
- ADR-007: local IPC는 SID 기반 인증과 command 등급별 권한 검사를 사용한다.

## Requirements

### 1. Truth source와 상태 어휘

- `TRUTH-01` `release/gates/g0-claims.json`을 현재 공개 capability claim의 유일한
  machine-readable 정본으로 둔다. README, SECURITY, Tauri 문구는 이 파일보다 강한 보장을
  주장할 수 없다.
- `TRUTH-02` claim 상태는 정확히 다음 네 값만 사용한다.

  | 상태 | 의미 | UI 허용 표현 |
  |---|---|---|
  | `supported` | 실제 실행 경로와 요구되는 자동/managed evidence가 모두 통과 | 기능 단위 “지원됨”; Protection Compliance와 동일시 금지 |
  | `experimental` | 실행 경로는 있으나 live/restore/release evidence가 부족 | “실험적”, “추가 검증 필요” |
  | `configured_only` | 값을 저장·표시하지만 자동 시행 경로가 없음 | “정책 저장됨”, “자동 시행 안 됨” |
  | `unavailable` | 실행 진입점이 없거나 명시적으로 거부 | “지원 안 함”, action 비활성화 |

- `TRUTH-03` evidence 상태는 `automated_pass`, `managed_pass`, `not_run`, `known_gap`,
  `not_applicable`만 사용한다. `#[ignore]` 테스트는 항상 `not_run`이며 pass 수에 더하지 않는다.
- `TRUTH-04` `supported` 승격에는 최소 하나의 non-ignored 자동 테스트가 필요하다. 외부 DB,
  native tool, 원격 Destination 또는 실제 restore를 포함하는 capability는 같은 source revision의
  managed evidence도 필요하다.
- `TRUTH-05` 엔진의 “지원”은 backup artifact 생성만으로 승격하지 않는다. probe, tool/server
  matrix, backup, L1/L2/L3, 실제 restore, content oracle 중 해당 Support Contract가 요구하는
  행이 모두 통과해야 한다.
- `TRUTH-06` Dashboard의 초록색 상태는 현재 기록된 개별 evidence만 뜻한다. current Protection
  Compliance가 구현되기 전에는 “안전”, “보호됨”, “정상 보호”라는 종합 상태를 만들지 않는다.

`g0-claims.json`은 UTF-8 JSON이며 최상위 구조를 다음과 같이 고정한다.

```json
{
  "schemaVersion": 1,
  "productVersion": "0.1.0",
  "executionAuthority": "legacy_direct",
  "cutoverMode": "rehearsal_only",
  "capabilities": []
}
```

각 capability 항목은 `id`, `claim`, `automatedEvidence`, `managedEvidence`,
`publicSummary`, `ownerSection`을 반드시 가진다. 배열에는 최소한 아래 항목과 초기값이 있어야
한다.

| Capability ID | 초기 claim | 자동 증적 | managed 증적 | 후속 소유 섹션 |
|---|---|---|---|---|
| `connection.probe` | `unavailable` | `known_gap` | `not_run` | section-06 |
| `database.discovery_ui` | `unavailable` | `known_gap` | `not_run` | section-06, section-07 |
| `backup.postgresql` | `experimental` | `automated_pass` | `not_run` | section-06 |
| `backup.mysql` | `experimental` | `automated_pass` | `not_run` | section-06 |
| `backup.mariadb` | `experimental` | `automated_pass` | `not_run` | section-06 |
| `destination.local` | `experimental` | `automated_pass` | `not_run` | section-05, section-06 |
| `destination.s3` | `experimental` | `automated_pass` | `not_run` | section-05, section-06 |
| `destination.ssh` | `experimental` | `automated_pass` | `not_run` | section-05, section-06 |
| `verification.l1` | `supported` | `automated_pass` | `not_applicable` | section-06 |
| `verification.l2` | `unavailable` | `known_gap` | `not_run` | section-06 |
| `verification.l3` | `unavailable` | `known_gap` | `not_run` | section-06 |
| `verification.automatic` | `configured_only` | `known_gap` | `not_run` | section-06 |
| `restore.decrypt_only` | `experimental` | `automated_pass` | `not_run` | section-06 |
| `restore.database_target` | `unavailable` | `known_gap` | `not_run` | section-06, section-08 |
| `retention.policy` | `configured_only` | `automated_pass` | `not_run` | section-06 |
| `retention.execution` | `unavailable` | `known_gap` | `not_run` | section-06 |
| `scheduler.legacy_task` | `experimental` | `automated_pass` | `not_run` | section-03, section-04 |
| `dispatcher.durable` | `unavailable` | `known_gap` | `not_run` | section-04 |
| `concurrency.max_parallel_dumps` | `configured_only` | `known_gap` | `not_run` | section-04 |
| `dashboard.protection_compliance` | `unavailable` | `known_gap` | `not_run` | section-05, section-08 |
| `notification.durable_delivery` | `unavailable` | `known_gap` | `not_run` | section-06 |
| `tauri.release_bundle` | `experimental` | `known_gap` | `not_run` | section-10 |
| `engine.sqlserver` | `unavailable` | `not_applicable` | `not_run` | section-09 |
| `engine.sqlite` | `unavailable` | `not_applicable` | `not_run` | section-09 |

`publicSummary`는 한국어 한 문장이고 claim보다 강한 표현을 포함하지 않는다. 예를 들어
`verification.automatic`은 “검증 정책은 저장되지만 백업 후 자동 실행은 아직 연결되지 않음”으로
기록한다.

### 2. 현재 실행 경로 inventory

- `TRUTH-07` `docs/release/g0-direct-execution-inventory.md`에 production process 시작점을
  전부 기록한다. 최소 inventory는 다음과 같다.

  | 시작 주체 | 코드 위치 | 현재 동작 | G1 이후 목표 |
  |---|---|---|---|
  | Tauri | `apps/console/src-tauri/src/run.rs::spawn_stream` | `restoreplane.exe` sidecar 직접 실행 | authenticated local IPC client |
  | CLI manual | `crates/restoreplane-cli/src/commands/run.rs::run` | 같은 process에서 job 순차 실행 | service queue enqueue |
  | CLI worker | `crates/restoreplane-cli/src/commands/run.rs::execute_job` | native dump와 fan-out 실행 | service-owned Process Supervisor |
  | Task Scheduler | `crates/restoreplane-cli/src/commands/schedule.rs` | job마다 task 등록 | service start/wake |
  | Task XML | `crates/restoreplane-scheduler/src/xml.rs::render_task_xml` | `restoreplane run --job ... --scheduled` | service wake command |
  | Verify/restore | `apps/console/src-tauri/src/run.rs`, CLI command 모듈 | sidecar와 외부 tool 직접 실행 | service Work Item |

- 각 행은 process owner, singleton 범위, overlap 범위, cancellation 범위, secret 전달 방식,
  stdout/stderr 소비 방식, 현재 rollback 방법을 포함한다.
- 코드 경로가 추가되면 claims 변경과 같은 PR에서 inventory를 갱신해야 한다.
- Phase 0 동안 기존 직접 경로는 제거하지 않는다. 대신 `executionAuthority=legacy_direct`를
  공개하고 G1 전환 대상임을 명시한다.

### 3. False-green 제거

- `TRUTH-08` `verify --level 2` 또는 `--level 3`은 L1로 조용히 낮추지 않는다. external
  runtime을 시작하기 전에 stable error code `VERIFY_LEVEL_NOT_EXECUTED`와 non-zero exit로
  종료한다. `--level 1`과 level 생략만 L1을 실행한다.
- `TRUTH-09` Dashboard는 `backups`의 마지막 성공과 `runs`의 마지막 Attempt를 별도로 읽는다.
  마지막 Attempt가 실패·부분 성공·실행 중이면 과거 성공 backup이 있어도 해당 상태를 숨기지
  않는다.
- `TRUTH-10` artifact badge는 `artifacts` 테이블의 실제 row만 사용한다. Plan에 Destination이
  설정됐다는 이유로 `uploaded`, `committed`, 불변성 또는 retention evidence를 합성하지 않는다.
  현재 DB row에서 알 수 없는 immutability와 retention은 `unknown`으로 표시한다.
- `TRUTH-11` Tauri에서 verification level과 retention은 “요청 정책 저장됨, 자동 시행 미배선”으로
  표시한다. 해당 설정을 근거로 완료 badge를 만들지 않는다.
- `TRUTH-12` 현재 restore UI는 decrypt-only만 실행 가능하게 제한한다. 성공 문구는 “평문 덤프
  복호화 완료”이며 “복원 검증 통과”가 아니다. 실제 DB restore action은 section-06과
  section-08이 eligibility와 Restore Safety Policy를 구현할 때까지 비활성화한다.
- `TRUTH-13` README와 SECURITY는 실제 코드 경로와 claims matrix를 반영한다. “정기 검증한다”,
  “실제 복원을 보장”, “구현 완료(검증됨)”처럼 managed evidence가 없는 문구를 현재 상태 설명으로
  사용하지 않는다.
- `TRUTH-14` 연결·Destination 등록 성공은 config/Secret 저장 성공이다. live probe 성공으로
  표현하지 않는다.
- `TRUTH-15` legacy schedule 등록 성공은 Windows task 등록 성공이다. durable queue, occurrence
  ledger 또는 RPO 준수 성공으로 표현하지 않는다.

### 4. Evidence 수집과 CI 판정

- `TRUTH-16` `scripts/collect-g0-evidence.ps1`은 다음 명령을 실행하고 각 command, 시작/종료
  시각, exit code, source revision, runner OS를 `artifacts/g0/evidence.json`에 기록한다.
  stdout/stderr 원문 전체와 환경변수 값은 JSON에 넣지 않는다.

  ```powershell
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  cargo test --workspace -- --ignored --list
  npm ci --prefix apps/console
  npm run build --prefix apps/console
  ```

- ignored 목록은 별도 `ignoredTests` 배열과 `status: "not_run"`으로 저장한다. 목록 수를
  `cargo test --workspace`의 pass 수에 더하지 않는다.
- live testbed는 별도 workflow가 실행한 경우에만 `managed_pass`를 만들 수 있다. evidence에는
  engine server version, client tool version/hash, OS, container image digest, 실행 시각,
  fixture ID, content oracle 결과가 모두 있어야 한다.
- `TRUTH-17` `scripts/check-g0-truth.ps1`은 claims schema, 허용 enum, 중복 capability ID,
  promotion rule, ADR-001~007 존재와 `상태: 승인됨`, direct execution inventory의 필수 경로,
  금지된 과대 문구, G0 이전 신규 engine source crate 부재를 검사한다.
- 금지 문구의 최소 집합은 `모든 백업이 안전합니다`, `최근 백업은 서명·검증되어 오프사이트에
  보관됩니다`, `복원 검증 통과`다. claims와 evidence가 강화되기 전에는 철자만 바꾼 동의어로
  우회하지 않는다.
- `TRUTH-18` `.github/workflows/ci.yml`의 Windows job은 Rust gate 뒤 frontend build와 두
  PowerShell script를 실행하고 `artifacts/g0`를 CI artifact로 업로드한다.
- CI의 `cargo test --workspace` 성공만으로 engine, L2/L3, remote destination, Tauri release
  bundle을 supported로 승격하지 않는다.
- evidence 생성 중 일부 command가 실패해도 가능한 나머지 결과를 기록한 뒤 최종 exit code를
  non-zero로 반환해 진단 artifact를 보존한다.

### 5. Durable gate와 cutover 금지 조건

- `TRUTH-19` `release/gates/g1-evidence-requirements.json`은 G1의 필수 evidence ID를 고정한다.
  다음 항목이 모두 같은 source revision과 build ID에서 pass여야 `durableGate`가 참이다.

  1. `g1.occurrence_uniqueness`
  2. `g1.stale_fencing_rejection`
  3. `g1.process_tree_reaping`
  4. `g1.checkpoint_crash_reconciliation`
  5. `g1.spool_reservation`
  6. `g1.resource_permit_limits`
  7. `g1.required_destination_truth`
  8. `g1.single_service_execution_path`
  9. `g1.operational_store_online_backup`

- 이 JSON은 요구사항 정본이지 pass 결과 파일이 아니다. pass 결과는 CI 또는 승인된 managed
  testbed가 생성하며 repository에 수동 커밋하지 않는다.
- `TRUTH-20` G1 evidence가 없거나 revision/build가 다르거나 한 항목이라도 실패하면
  `cutoverMode`는 `rehearsal_only`다. 이 상태에서는 shadow import와 diff report만 허용하고
  scheduler freeze, production service execute enable, cutover marker 기록, old-path disable을
  금지한다.
- `TRUTH-21` G1 전에는 dual execution을 시도하지 않는다. legacy direct path가 authority이고
  service가 추가되더라도 production Work Item을 claim하지 않는다.
- `TRUTH-22` `docs/release/g0-cutover-recovery-matrix.md`는 아래 복구 결정을 명시한다.

  | 장애 시점 | 허용 조치 | 금지 조치 | 최종 authority |
  |---|---|---|---|
  | G1 evidence 전 | rehearsal 중단, legacy 유지 | scheduler freeze, marker 기록 | legacy |
  | shadow diff 실패 | shadow DB 폐기, 수정 후 재시도 | import 일부를 production으로 사용 | legacy |
  | scheduler freeze 후 marker 전 | service 중지, legacy task enablement 복원 | 두 경로 동시 enable | legacy |
  | marker 후 새 v2 데이터 전 | 검증된 recovery bundle로 marker와 scheduler 상태 원복 | 임의 DB 파일 교체 | legacy |
  | 첫 v2 control/run 데이터 후 | service authority 유지, export/import 또는 forward-fix | 구 binary downgrade | service |

### 6. 변경 경계와 범위 제한

- 현재 worktree는 사용자 변경이 있는 상태일 수 있다. 구현 시작 전에 `git rev-parse HEAD`,
  branch, `git status --short`, 변경 파일 목록을 로컬 evidence에 기록한다. 기존 변경을 reset,
  checkout, 덮어쓰기하거나 자동 commit하지 않는다.
- Phase 0 patch는 아래 명시된 파일에만 적용하고 기존 변경과 겹치면 hunk 단위로 병합한다.
- 이 섹션에서는 `Engine` variant, source/destination adapter, Windows service, Operational
  Store schema, queue table, IPC protocol을 추가하지 않는다.
- ADR의 결정을 바꿔야 하는 발견이 나오면 구현을 계속 밀어붙이지 않고 새 ADR로 supersede한 뒤
  claims matrix와 gate 요구사항을 같은 변경에서 갱신한다.

## Dependencies and Blocks

### Depends on

- 선행 구현 섹션 없음
- 기존 workspace와 Tauri 앱
- 승인된 ADR-001~007
- Windows runner와 Git

### Blocks

- `section-02-domain-and-operational-store`: G0 truth evidence와 ADR gate 없이 schema/cutover
  migration을 시작할 수 없다.
- 모든 후속 섹션: public claim promotion은 이 섹션의 claims/evidence 규칙을 따라야 한다.
- `section-09-new-engine-wave-one`: G0 완료 전 SQL Server/SQLite adapter source code를 만들 수
  없다.

### 병렬 작업 제한

이 섹션은 기준선을 바꾸므로 다른 source 구현과 병렬 진행하지 않는다. 문서 조사와 testbed 준비는
가능하지만, claims matrix와 false-green 회귀 테스트가 merge되기 전 신규 기능 코드는 시작하지
않는다.

## Flow Diagram Nodes and Branches

대상 흐름은 `flow-diagrams/config-cutover.mmd`다.

| 소유 node | 구현 책임 | 정상 branch | 오류·edge branch |
|---|---|---|---|
| `start` | `collect-g0-evidence.ps1`로 revision, worktree, test, claim 기준선 수집 | 필수 artifact가 생성되면 후속 shadow import 준비 | evidence 수집 실패 시 결과를 남기고 `stopped` |
| `durableGate` | G1 필수 ID, revision/build 일치, pass 상태를 원자적으로 평가 | 전부 참일 때만 후속 section의 freeze 단계 허용 | 누락·stale·다른 build·실패는 `rehearsalOnly` |
| `rehearsalOnly` | preview와 diff evidence만 보존하고 production state mutation 거부 | 수정된 build에서 재평가 | marker/freeze 요청은 stable error로 거부 |
| `stopped` | 오류를 기록하고 기존 execution authority를 유지 | 운영자는 원인을 수정한 뒤 `start`부터 재시도 | partial enablement가 있으면 recovery matrix에 따라 단일 authority 복원 |

흐름의 나머지 node인 `exportLegacy`, `shadowImport`, `validateDiff`, `freezeLegacy`,
`serviceHealthy`, `writeMarker`, `disableOldPath`, `newData`, `rollbackBundle`, `forwardFix`,
`complete`는 section-02와 section-03이 구현한다. 이 섹션은 해당 node를 실행하지 않고 진입
전제와 금지 조건만 강제한다.

구현 함수와 artifact 이름은 다음으로 고정한다.

- `scripts/check-g0-truth.ps1::Test-G0Claims`
- `scripts/check-g0-truth.ps1::Test-DurableEvidence`
- `scripts/collect-g0-evidence.ps1::Invoke-EvidenceCommand`
- generated `artifacts/g0/evidence.json`
- generated `artifacts/g0/ignored-tests.txt`
- generated `artifacts/g0/worktree.json`

## Reference Libraries and Tools

이 섹션은 runtime dependency를 추가하지 않는다.

| 도구/라이브러리 | 현재 버전/범위 | 용도 | 버전 처리 |
|---|---|---|---|
| Rust | edition 2021, `rust-version = 1.91`, release toolchain `1.97.0` | workspace test와 catalog query | OpenDAL 0.58 MSRV를 1.91로 확인하고 MSRV/current toolchain을 CI에서 각각 고정 검증 |
| `rusqlite` | `=0.31.0`, bundled | latest run과 실제 artifact evidence query | 현재 exact pin 유지 |
| PowerShell | runner 제공 버전 | JSON 검증과 evidence 수집 | 미고정, 구현 착수 시 `$PSVersionTable.PSVersion`을 evidence에 기록 |
| React | `^18.3.1` | truth label과 상태 렌더링 | `package-lock.json` 해석 버전 사용 |
| TypeScript | `^5.6.3` | Tauri response type 변경 검증 | `package-lock.json` 해석 버전 사용 |
| Vite | `^6.0.3` | frontend production build | `package-lock.json` 해석 버전 사용 |
| Tauri | Rust/CLI `2` / npm `^2.1.x` | 현재 desktop surface | exact version 미고정, 구현 착수 시 Cargo.lock/package-lock 값 확인 |
| GitHub Actions | `checkout@v4`, `rust-cache@v2`, `upload-artifact@v4`, 기타 현행 action | CI와 evidence artifact | tag가 mutable하므로 구현 착수 시 공식 release와 commit SHA를 확인해 pin |

Node.js와 npm 자체 버전도 현재 repository에서 고정돼 있지 않다. 이 섹션의 evidence에
`node --version`과 `npm --version`을 기록하며, 지원 범위 확인 전 임의의 버전을 문서에
하드코딩하지 않는다.

## Detailed Implementation

### Step 0. 사용자 변경을 보존한 기준선 수집

1. repository root와 현재 branch/HEAD를 확인한다.
2. `git status --short`의 경로와 상태만 `artifacts/g0/worktree.json`에 기록한다. diff 본문과
   untracked 파일 내용은 비밀 노출 위험 때문에 evidence에 넣지 않는다.
3. 구현자가 생성한 patch와 기존 변경을 파일별로 구분한다. 겹치는 파일은 전체 교체하지 않고
   최소 hunk로 수정한다.
4. `artifacts/g0/`는 root `.gitignore`에 추가하고 CI artifact로만 보존한다.

### Step 1. claims와 gate requirements 작성

1. `release/gates/g0-claims.json`에 본 문서의 모든 필수 capability를 정확히 기록한다.
2. `release/gates/g1-evidence-requirements.json`에 아홉 개 G1 ID와
   `sameSourceRevision=true`, `sameBuildId=true`, `allRequired=true`를 기록한다.
3. `docs/release/g0-truth-baseline.md`에 claims 표를 사람이 읽는 설명으로 투영한다. JSON과
   수동으로 다른 상태를 쓰지 않고 생성 또는 CI 비교로 drift를 막는다.
4. `docs/release/g0-direct-execution-inventory.md`와
   `docs/release/g0-cutover-recovery-matrix.md`를 작성한다.

### Step 2. Red tests로 현재 false-green 고정

1. `verify.rs`에서 level 2/3 요청이 L1 성공으로 종료하는 현상을 재현하는 단위 테스트를 먼저
   추가한다.
2. catalog test fixture에 과거 성공 backup과 더 최근 failed run을 넣고, 최신 Attempt가 실패로
   조회돼야 한다는 테스트를 추가한다.
3. 두 Destination 중 한 곳의 artifact row만 있는 fixture에서 실제 row 하나만 반환되는 테스트를
   추가한다.
4. Dashboard health 테스트에서 과거 서명/L1 pass backup이 있어도 더 최근 failed run이면
   `danger`가 되는지 확인한다.
5. truth script 테스트 fixture에 중복 ID, 허용되지 않은 enum, managed evidence 없는
   `supported`, stale revision, 과대 문구를 각각 넣고 모두 실패하는지 확인한다.

PowerShell script 테스트는 `scripts/tests/check-g0-truth.Tests.ps1`에서 script 자체를 별도
process로 호출한다. Pester dependency를 새로 추가하지 않고 exit code와 임시 fixture output을
PowerShell 기본 기능으로 단언한다.

### Step 3. CLI와 catalog를 truthful하게 변경

1. `crates/restoreplane-cli/src/commands/verify.rs`에 요청 level을 검증하는 작은 pure function을
   추가한다. level 2/3이면 config, keystore, Manifest 또는 artifact를 읽기 전에
   `VERIFY_LEVEL_NOT_EXECUTED`로 실패한다.
2. L2/L3 library 코드는 삭제하지 않는다. section-06이 managed fixture와 실제 command 배선을
   완성할 때 같은 guard와 claims를 함께 승격한다.
3. `crates/restoreplane-catalog/src/query.rs`에 `LatestRunRecord`와
   `latest_run_per_job`을 추가한다. 정렬은 `started_at DESC, rowid DESC`로 결정적이어야 한다.
4. 같은 파일에 `ArtifactRecord`와 `artifacts_for_backup`을 추가하고 `artifacts`의 실제 row만
   반환한다.
5. `crates/restoreplane-catalog/src/lib.rs`에서 두 query와 record type을 export한다.
6. schema migration은 이 섹션에서 만들지 않는다. 기존 `runs`와 `artifacts` 컬럼만 읽는다.

### Step 4. Tauri projection과 copy 수정

1. `apps/console/src-tauri/src/commands.rs`에서 config 기반 `artifacts_for_job` 합성을 제거하고
   catalog의 `artifacts_for_backup` 결과를 사용한다.
2. `JobSummaryView`에 `last_attempt_status`와 `last_attempt_started_at`을 추가한다.
3. `health_of`는 마지막 Attempt 상태를 먼저 평가하고, 그 다음에 backup signature와 개별
   verification evidence를 평가한다. `success` backup만 존재한다는 이유로 Protection
   Compliance를 추론하지 않는다.
4. `apps/console/src/types.ts`와 `apps/console/src/lib/api.ts`를 Rust response shape에 맞춘다.
5. `Dashboard.tsx`에서 종합 “안전” 문구를 제거한다. 상단은 “기록된 실행 증적”을 설명하고
   최근 실패, 마지막 성공 backup, verification evidence를 별도 행으로 표시한다.
6. `Badges.tsx`는 `partial`, `configured`, `unknown`을 실패와 구분한다. `unknown`을 초록색으로
   렌더링하지 않는다.
7. `Jobs.tsx`의 verification/retention은 “정책 저장됨, 자동 시행 미배선”으로 표시한다.
8. `Restore.tsx`는 decrypt-only를 선택하지 않은 실행을 막고, 성공 시 “평문 덤프 복호화 완료”로
   표시한다.
9. `Schedule.tsx`는 현재 경로를 “legacy job별 Task Scheduler”로 표시하고 durable queue나
   RPO 보장을 주장하지 않는다.
10. `README.md`와 `SECURITY.md`의 현재 상태 표를 claims matrix에 맞추고, managed test를 실행하는
    정확한 별도 명령과 `not_run` 의미를 설명한다.

### Step 5. Evidence scripts 구현

1. `collect-g0-evidence.ps1`은 각 command를 독립 process로 실행해 exit code를 잃지 않게 한다.
   중간 실패 후에도 다음 command를 실행하고 실패 개수를 JSON에 기록하며, 하나라도 실패하면
   마지막에 exit code 1을 반환한다.
2. stdout/stderr는 콘솔과 CI log에 보내되 evidence JSON에는 command 이름, duration, exit code와
   요약만 저장한다.
3. `cargo test --workspace -- --ignored --list` 출력은 test name만 정규화해
   `ignored-tests.txt`로 저장한다. 명령은 test를 실행하지 않는다.
4. `check-g0-truth.ps1`은 claims와 evidence를 parse하고 promotion rule을 적용한다.
5. 모든 file path는 repository root 기준으로 resolve한다. `$HOME`, wildcard delete, broad
   recursive cleanup을 사용하지 않는다.
6. fixture용 임시 디렉터리는 runner temp 아래에 생성하고 test 종료 뒤 그 정확한 경로만 제거한다.

### Step 6. CI 연결과 G0 판정

1. Windows CI에서 Rust fmt/clippy/test 뒤 evidence collector와 truth checker를 실행한다.
2. frontend는 `npm ci` 후 `npm run build`를 반드시 통과한다.
3. evidence artifact는 command 실패 여부와 무관하게 `if: always()`로 업로드한다.
4. supply-chain과 secret scan job은 유지한다. 이 섹션은 기존 사용자 변경을 되돌리지 않는다.
5. G0 review는 machine result와 `docs/release/g0-truth-baseline.md`를 함께 확인한다.
6. G0가 통과하면 section-02 구현을 허용하되 `cutoverMode`는 여전히 `rehearsal_only`다. 실제
   execution cutover 허용은 section-04의 G1 evidence가 별도로 필요하다.

## Test Scenarios

### Normal

| ID | Scenario | Expected |
|---|---|---|
| `G0-N-01` | 허용 enum과 필수 field를 가진 claims JSON 검사 | exit 0, capability ID 전부 유일 |
| `G0-N-02` | `verify --level 1`로 유효 artifact 검증 | L1 실행, 실제 result에 따른 종료 |
| `G0-N-03` | 과거 성공 backup 뒤 최신 run이 failed | Dashboard는 최근 실패를 먼저 표시 |
| `G0-N-04` | S3만 committed artifact row가 있고 Local row는 없음 | S3 하나만 committed로 표시, Local 합성 없음 |
| `G0-N-05` | 기본 CI test 실행 | non-ignored 결과와 ignored `not_run` 목록이 분리 |
| `G0-N-06` | G1 evidence가 없음 | `durableGate=false`, `rehearsal_only`, legacy authority 유지 |
| `G0-N-07` | decrypt-only restore 성공 | “평문 덤프 복호화 완료”, DB restore 성공 문구 없음 |
| `G0-N-08` | frontend production build | TypeScript response shape와 copy guard 통과 |

### Error

| ID | Scenario | Expected |
|---|---|---|
| `G0-E-01` | `verify --level 2` 또는 `3` | artifact를 읽기 전 `VERIFY_LEVEL_NOT_EXECUTED`, non-zero |
| `G0-E-02` | managed evidence 없이 engine claim을 `supported`로 변경 | truth checker 실패 |
| `G0-E-03` | claims에 중복 ID 또는 알 수 없는 상태 | truth checker 실패와 정확한 JSON path |
| `G0-E-04` | ADR 파일 누락 또는 승인 상태 변경 | G0 실패 |
| `G0-E-05` | banned false-green copy 재도입 | frontend build 뒤 truth checker 실패 |
| `G0-E-06` | collector의 cargo test 실패 | 나머지 증적 수집 후 최종 non-zero, artifact 업로드 |
| `G0-E-07` | shadow diff 실패 | production mutation 없이 `stopped`, legacy authority 유지 |
| `G0-E-08` | G1 evidence 중 한 항목 실패 | scheduler freeze와 marker 기록 금지 |

### Edge

| ID | Scenario | Expected |
|---|---|---|
| `G0-X-01` | 두 run의 timestamp가 같음 | `rowid DESC` tie-break로 최신 상태가 결정적 |
| `G0-X-02` | backup은 없고 failed run만 있음 | “백업 이력 없음”과 “최근 실행 실패”를 동시에 표시 |
| `G0-X-03` | artifact immutability/retention이 NULL | `unknown`, 초록 badge와 보호됨 문구 없음 |
| `G0-X-04` | ignored test 목록이 0건 | 빈 배열을 유효하게 기록하되 pass 수를 늘리지 않음 |
| `G0-X-05` | claims와 evidence의 source revision이 다름 | promotion과 durable gate 거부 |
| `G0-X-06` | service shell은 healthy지만 G1 queue evidence가 없음 | rehearsal만 허용 |
| `G0-X-07` | marker 이후 첫 v2 데이터가 생긴 상태에서 downgrade 요청 | 거부하고 forward-fix 절차 표시 |
| `G0-X-08` | 기존 dirty worktree와 수정 파일이 겹침 | 기존 hunk를 보존하고 새 hunk만 적용, reset 없음 |

## TDD Strategy

### Red

1. L2/L3 요청이 L1 성공으로 내려가는 테스트를 실패시킨다.
2. 최근 failed run이 과거 성공 backup 뒤에 가려지는 projection 테스트를 실패시킨다.
3. config Destination이 실제 artifact처럼 합성되는 테스트를 실패시킨다.
4. 현재 Dashboard와 README의 banned claim을 truth checker가 탐지하게 한다.
5. managed evidence 없는 `supported` promotion fixture를 실패시킨다.

### Green

1. level guard를 command 시작점에 배치해 L2/L3를 명시적으로 거부한다.
2. 기존 `runs`와 `artifacts` table을 읽는 최소 query를 추가한다.
3. Tauri response를 최신 Attempt와 실제 artifact record 기반으로 바꾼다.
4. 사용자 copy를 evidence 범위로 낮춘다.
5. claims/evidence validator와 CI upload를 연결한다.

### Refactor

1. query row mapping과 health 계산을 pure function으로 분리한다.
2. PowerShell의 command 실행, JSON 직렬화, error aggregation을 함수로 나눈다.
3. 같은 public claim을 README와 UI에 중복 하드코딩하지 않도록 정본 링크와 짧은 status
   projection만 유지한다.
4. test fixture가 production file을 덮어쓰지 않게 모든 path를 명시적으로 주입한다.

각 단계에서 `cargo fmt`, 관련 crate test, frontend build, script fixture test를 먼저 실행한 뒤
전체 workspace gate로 확대한다.

## Quality Gate

### G0-A Truth Gate

아래 조건을 모두 만족해야 section-01이 완료된다.

- `release/gates/g0-claims.json`의 schema와 promotion rule이 통과한다.
- 현재 public copy가 claims보다 강하지 않다.
- L2/L3 요청은 L1로 내려가지 않고 non-zero로 실패한다.
- 최근 failed run이 과거 성공 backup과 별도로 보인다.
- Dashboard artifact는 실제 catalog row만 표시한다.
- ignored test가 pass 집계와 분리되어 `not_run`으로 보인다.
- `cargo fmt --all -- --check` 통과
- `cargo clippy --workspace --all-targets -- -D warnings` 통과
- `cargo test --workspace` 통과
- `npm ci --prefix apps/console` 통과
- `npm run build --prefix apps/console` 통과
- `scripts/tests/check-g0-truth.Tests.ps1` 통과
- ADR-001~007이 모두 존재하고 승인 상태다.
- direct execution inventory의 여섯 필수 경로가 현재 source와 일치한다.
- SQL Server/SQLite를 포함한 신규 engine adapter source code가 없다.
- production execution authority는 `legacy_direct`, cutover는 `rehearsal_only`다.

G0-A 통과는 “v2가 production-ready”라는 뜻이 아니다. 의미는 현재 제품 약속이 증적과 일치하고
후속 설계를 시작할 수 있다는 것이다. 실제 execution cutover는 section-04의 G1을 통과한 뒤에만
가능하다.

## Risk and Rollback

| Risk | Mitigation | Rollback/Forward action |
|---|---|---|
| truth label 축소가 기능 퇴행처럼 보임 | 기존 기능은 유지하고 증적 범위만 정확히 표시 | copy를 과대로 되돌리지 않고 missing evidence를 구현해 승격 |
| 기존 dirty worktree와 충돌 | 시작/종료 file inventory, hunk 단위 patch, 자동 commit 금지 | 충돌 파일만 중단하고 사용자 변경 보존 |
| claims JSON과 UI/README drift | CI promotion/copy checker | claim 또는 구현을 같은 변경에서 맞춤 |
| PowerShell runner 차이 | PS/OS/version evidence 기록, 기본 기능만 사용 | script 오류는 release block, 수동 성공으로 대체 금지 |
| ignored test가 실수로 pass 취급 | `--ignored --list` 전용 경로와 별도 field | 잘못된 evidence artifact 폐기 후 재수집 |
| artifact row가 없어서 UI 정보가 줄어듦 | `unknown`과 “증적 없음”을 명시 | config 기반 성공 합성은 복구 수단으로 사용 금지 |
| G1 evidence 파일 위조·재사용 | revision/build 일치, generated result는 repository에 미커밋 | gate 거부 후 승인 testbed에서 재실행 |
| service shell만 보고 조기 cutover | G1 아홉 항목 all-required | rehearsal 결과 보존, legacy authority 유지 |
| marker 이후 downgrade 시 데이터 분열 | first-v2-data 이후 forward-fix only | service 유지, export/import 또는 새 binary 수정 |

Phase 0 source patch 자체는 아직 schema나 production artifact format을 바꾸지 않는다. 문제가
발생하면 CLI guard, catalog read query, Tauri projection과 CI script를 해당 patch 단위로 되돌릴
수 있다. 단, 과대 public claim을 되살리는 rollback은 허용하지 않는다. 기능을 되돌려야 한다면
claim도 더 낮은 상태로 함께 변경한다.

## Acceptance Criteria

- [ ] claims matrix가 현재 capability를 네 가지 허용 상태로 정확히 분류한다.
- [ ] README, SECURITY, Tauri가 claims matrix보다 강한 약속을 하지 않는다.
- [ ] `verify --level 2/3`이 L1 success로 종료하지 않는다.
- [ ] latest backup과 latest Attempt가 별도 projection으로 제공된다.
- [ ] 최근 실패가 과거 성공에 가려지지 않는다.
- [ ] 실제 `artifacts` row가 없는 Destination은 committed/uploaded로 표시되지 않는다.
- [ ] decrypt-only 완료와 실제 DB restore/content validation이 구분된다.
- [ ] ignored live test는 `not_run`이며 기본 test pass 수에 포함되지 않는다.
- [ ] G0 evidence JSON에 revision, OS/tool versions, command exit code와 duration이 기록된다.
- [ ] evidence JSON과 log에 환경변수 값, password, token, key material이 없다.
- [ ] ADR-001~007 존재와 승인 상태가 CI에서 검증된다.
- [ ] direct execution inventory가 Tauri, CLI, Scheduler, verify/restore 경로를 모두 포함한다.
- [ ] G1 evidence가 없으면 cutover action이 `rehearsal_only`로 멈춘다.
- [ ] 기존 user worktree 변경은 reset, overwrite, 자동 commit되지 않는다.
- [ ] G0-A의 Rust, frontend, PowerShell gate가 모두 통과한다.

## Files to Create

```text
release/gates/g0-claims.json
release/gates/g1-evidence-requirements.json
docs/release/g0-truth-baseline.md
docs/release/g0-direct-execution-inventory.md
docs/release/g0-cutover-recovery-matrix.md
scripts/check-g0-truth.ps1
scripts/collect-g0-evidence.ps1
scripts/tests/check-g0-truth.Tests.ps1
crates/restoreplane-catalog/tests/truth_projection.rs
```

## Files to Modify

```text
.github/workflows/ci.yml
.gitignore
README.md
SECURITY.md
crates/restoreplane-cli/src/commands/verify.rs
crates/restoreplane-catalog/src/query.rs
crates/restoreplane-catalog/src/lib.rs
apps/console/src-tauri/src/commands.rs
apps/console/src/types.ts
apps/console/src/lib/api.ts
apps/console/src/components/Badges.tsx
apps/console/src/screens/Dashboard.tsx
apps/console/src/screens/Jobs.tsx
apps/console/src/screens/Restore.tsx
apps/console/src/screens/Schedule.tsx
```

## Generated Files, Not Committed

```text
artifacts/g0/evidence.json
artifacts/g0/ignored-tests.txt
artifacts/g0/worktree.json
```
