# Section 10 — Expansion Waves and Production Launch

## Background

v2의 성공 기준은 DB 로고 수가 아니라 연결한 DB가 실제로 보호되고 복원 가능함을 증명하는
것이다. 기존 엔진, 제품 UI, SQL Server/SQLite가 gate를 통과한 뒤에만 추가 엔진·목적지와
원격 운영 경계를 연다. 이 섹션은 core를 범용 SaaS로 미리 일반화하지 않고 설치, update,
rollback, 관측, support, 단계적 출시까지 제품을 닫는다.

## Requirements

### Expansion policy

- 새 engine/destination은 공통 provider/adapter conformance와 실제 restore/reader fixture를
  통과해야 한다.
- MongoDB와 Firebird는 다음 후보이며 vendor dump/restore tool을 사용한다.
- Redis/Valkey export는 restore runbook, consistency 의미, 실제 testbed 전까지
  `experimental`이며 기본 Plan 선택 대상이 아니다.
- S3-compatible/Object Lock, SMB/NAS, Azure Blob, rclone 계열은 writer/read/delete/list/
  immutability conformance를 각각 검증한다.
- 지원 상태와 limitation은 Engine/Adapter Matrix와 claims matrix에서 같은 build evidence로
  승격한다.

### Explicitly deferred boundaries

- initial v2 local desktop에는 public HTTP API와 조직 RBAC가 없다.
- Remote Agent는 identity/provisioning, mutual authentication, offline queue, clock,
  upgrade compatibility, split-brain/fencing, secret scope, support bundle 위협 모델을 다룬
  별도 ADR과 protocol version 뒤에만 시작한다.
- local IPC DTO를 그대로 network에 노출하지 않는다.
- backup content browser는 민감 데이터/대용량 query/권한 모델 전까지 제외한다.

### Installer and service lifecycle

- installer는 service account/SID, pipe/Secret/Operational Store/spool ACL을 검증한다.
- install/repair/uninstall은 backup artifact와 operator control backup을 임의 삭제하지 않는다.
- update는 새 claim 중지→running drain 또는 checkpoint/cancel policy→online control backup→
  binary/schema compatibility check→service restart→Reconciliation 순서다.
- minimum reader version과 cutover marker가 old binary downgrade를 차단한다.
- schema migration 실패는 write mode를 열지 않고 diagnostics와 recovery action을 제공한다.

### Observability and support

- service/queue/lease/permit/spool/WAL/tool/destination/notification health를 local diagnostics로
  제공한다.
- support bundle은 선택한 기간/resource의 redacted config metadata, Activity, version/hash,
  schema/migration, Engine Matrix, counters를 포함하고 secret canary scan을 통과한다.
- metric/log volume은 bounded retention과 archive를 사용하며 raw stdout/stderr/secret을 넣지 않는다.
- release evidence는 revision/build/OS/tool/fixture와 gate 결과를 machine-readable로 보존한다.

### Staged rollout

1. internal dogfood: Local+PG/MySQL/MariaDB
2. controlled beta: existing engines+S3/SSH, migration rehearsal
3. opt-in v2 cutover: G0~G3 evidence와 rollback rehearsal
4. Wave 1: SQL Server candidate/SQLite supported 범위
5. general availability: install/update/support/SLO gate
6. expansion engine/destination은 독립 feature claim으로 rollout

각 단계에 stop condition, rollback/forward-fix, data migration boundary를 정의한다.

## Dependencies

- **Requires**: section-08 G3-B, section-09 G4
- **Blocks**: 없음; 이후 Agent/API는 별도 계획
- **병렬**: 추가 provider별 개발은 conformance suite 안에서 병렬 가능하지만 launch gate는
  모든 필수 evidence를 직렬 집계한다.

## Flow Diagram Nodes

핵심 v2 흐름의 새 node를 만들지 않는다. 모든 새 engine/destination은 F01~F05를 그대로
통과해야 한다. installer/update는 F04 Reconciliation, migration은 F06 cutover 불변조건을
재사용한다. Agent/API가 승인되면 현재 local flow와 섞지 않고 별도 diagram/ADR을 추가한다.

## Reference Libraries and Platforms

| 대상 | 원칙 |
|---|---|
| Windows installer/service update tool | 구현 시 공식 signing/SCM/rollback 문서 확인 |
| MongoDB Database Tools | server/tool compatibility 공식 matrix 확인 |
| Firebird backup/restore tools | 공식 tool/format/restore 문서 확인 |
| Redis/Valkey export tools | consistency와 restore semantics 공식 문서 확인 |
| Destination SDK | 공식 versioning/retry/immutability API 확인 |
| code signing/release actions | tag가 아니라 검토한 commit SHA pin |

## Implementation Details

### 1. Adapter certification kit

provider/adapter fixture runner, result schema, evidence publisher, claims promotion checker를
하나의 kit로 만든다. 새 crate가 존재하는 것만으로 UI supported option이 되지 않게 한다.

### 2. Packaging

signed binaries, native tool inventory, license/SBOM, default config, service registration, ACL,
upgrade/uninstall scripts를 release artifact로 만든다. bundled tool은 hash/signature와 공급 출처를
기록하고 system/operator tool은 별도 trust policy를 적용한다.

### 3. Upgrade and rollback

N-1 compatible read/IPC 범위를 선언하고 migration rehearsal fixture를 유지한다. update fault를
download, verify, drain, backup, replace, schema, start, reconcile 단계별로 주입한다. cutover 이후
새 데이터가 있으면 구 binary rollback 대신 compatible new build 또는 control-store restore를
사용한다.

### 4. Diagnostics/support bundle

`service.diagnostics`와 `support-bundle.create`를 Operation Run으로 실행한다. bundle manifest에
포함/제외 목록, redaction version, checksum, 생성 build를 기록한다. canary가 하나라도 발견되면
bundle 성공을 거부한다.

### 5. SLO and launch decision

최소 지표는 due 대비 설명되지 않은 누락, occurrence 중복 RP, stale fence commit, RPO violation
time-to-detect, restore drill pass rate, queue/heartbeat freshness, repair backlog, notification
delivery, support bundle 성공이다. 절대 수치는 beta 측정 후 승인하되 safety invariant는 0건을
유지한다.

## Test Scenarios

### Installation and upgrade

| ID | 입력 | 기대 |
|---|---|---|
| S10-N01 | clean install | service/SID/ACL/pipe/store/spool health |
| S10-N02 | legacy install에서 rehearsal+cutover | dual-run 0, plan/secret mapping 완전 |
| S10-N03 | running backup 중 update | 정책대로 drain/checkpoint, restart Reconciliation |
| S10-E01 | binary signature/hash 실패 | install/update 전 차단 |
| S10-E02 | schema migration 실패 | write mode/claim 0, diagnostics와 recovery |
| S10-E03 | uninstall | artifact/control backup 보존과 명시적 선택 |

### Expansion

| ID | 입력 | 기대 |
|---|---|---|
| S10-X01 | 새 engine backup만 통과/restore 실패 | supported 승격 거부 |
| S10-X02 | Destination writer pass/reader fail | Plan 활성화/restore eligibility 거부 |
| S10-X03 | Redis export 존재/restore test 없음 | experimental 유지 |
| S10-X04 | Object Lock delete 거부 | 보존 evidence, false error/success 없음 |
| S10-X05 | Agent 기능 flag without ADR/protocol | build/release checker 거부 |

### Operations

| ID | 입력 | 기대 |
|---|---|---|
| S10-O01 | support bundle에 secret canary | bundle 성공 거부와 source 위치 redacted report |
| S10-O02 | Activity/outbox 장기 보존 | bounded archive, open Attention/ledger 보존 |
| S10-O03 | update 후 expired leases/partial artifact | startup Reconciliation 수렴 |
| S10-O04 | old binary after minimum reader bump | 시작 거부 |
| S10-O05 | staged rollout stop metric 초과 | 다음 cohort 중단, 현재 authority 보존 |

## Implementation Strategy

### Red

installer ACL, update phase fault, support canary, adapter promotion, downgrade guard, staged rollout
stop-condition test를 먼저 만든다.

### Green

packaging/service lifecycle, diagnostics/support bundle, evidence aggregator, first expansion
conformance를 구현한다. Agent/API는 구현하지 않는다.

### Refactor

release artifact metadata, provider fixture runner, rollout decision을 machine-readable schema로
통합한다. platform-specific installer code와 domain/runtime을 분리한다.

## Quality Gate and Launch

- [ ] G0~G4 필수 evidence가 같은 release revision/build에 연결
- [ ] clean install, repair, update, uninstall, cutover fault matrix 통과
- [ ] code signing, SBOM, dependency/license/security scan 통과
- [ ] service drain/restart/Reconciliation에서 orphan/duplicate commit 0
- [ ] support bundle secret canary 0
- [ ] 기존 엔진 restore drill과 Wave 1 승인 범위 통과
- [ ] staged rollout stop/rollback rehearsal 통과
- [ ] evidence 없는 provider/destination/Agent claim 0

## Risk and Rollback

| Risk | Mitigation | Rollback/Forward action |
|---|---|---|
| 지원 범위 폭증 | per-capability independent gate | 해당 provider disable/강등 |
| installer ACL 오류 | clean/multi-user fixture | service stop과 repair installer |
| update 중 실행 손실 | drain+control backup+reconcile | compatible build forward-fix |
| downgrade data 분열 | minimum reader/old guard | v2 authority 유지 |
| support bundle 유출 | allowlist/redaction/canary | bundle 폐기와 secret rotate |
| premature Agent/API | explicit ADR/protocol gate | scope 제외 유지 |

## Acceptance Criteria

- [ ] 사용자가 설치·업데이트·복구 연습을 문서화된 절차로 수행한다.
- [ ] release claim이 실제 fixture evidence보다 강하지 않다.
- [ ] 추가 엔진/목적지가 기존 flow와 safety invariant를 재사용한다.
- [ ] 원격 확장은 local v2를 불안정하게 만들지 않고 별도 승인 경계로 남는다.
- [ ] production incident에 필요한 diagnostics/support bundle이 secret 없이 생성된다.

## Files to Create/Modify

```text
installer/
scripts/{install,repair,update,uninstall,test-release-matrix}.ps1
crates/dbbackup-service/src/{update,diagnostics,support_bundle}.rs
crates/dbbackup-cli/src/commands/{diagnostics,support_bundle}.rs
crates/dbbackup-source-{mongodb,firebird}/
crates/dbbackup-dest-*/
release/{claims,evidence,rollout}.json
docs/runbooks/{install,update,rollback,incident,support-bundle}.md
.github/workflows/{ci,managed-fixtures,release}.yml
```
