# Section 08 — Snapshot Inventory and Recovery Workspace

## Background

복원은 Snapshot ID나 connection string을 수동 입력하는 명령창이 아니라, 증거가 있는 복구
지점에서 시작하는 guided workspace여야 한다. 사용자는 “백업 성공”이 아니라 어떤 Part와
Replica가 있고 언제 어떤 수준으로 검증됐는지 본 뒤 새 DB 복원 또는 고위험 overwrite를
선택한다.

## Requirements

### Snapshot inventory

- Target/Connection/engine/time/completeness/durability/compliance/verification/eligibility로
  검색·필터한다.
- complete Recovery Point만 기본 목록에 포함하고 incomplete/orphan은 diagnostics filter로
  분리한다.
- inspector에 signed Manifest, Part, Replica, reader eligibility, immutability/retention,
  L1/L2/L3 evidence와 freshness를 표시한다.
- `complete + eligible Replica`이지만 required가 부족한 경우 `degraded/non_compliant`를 동시에
  보여주고 restore action은 정책에 따라 eligible/conditional로 구분한다.
- Manifest ingest rejected/unknown key는 restore blocked와 명확한 repair/support action을 준다.

### Recovery workspace

- Snapshot inspector에서 진입하고 source Snapshot/Replica를 자동 전달한다.
- 기본 mode는 새 Database/file이며 overwrite는 R-OWNER만 가능하다.
- target Connection과 name/path를 선택하면 service가 authoritative fingerprint와 eligibility를
  계산한다.
- preflight는 tool, space, permission, connection, server/version, target existence,
  engine policy를 단계별로 보여준다.
- overwrite는 resource identity 재입력, final summary, SID/action/resource/payload/expiry bound
  confirmation을 요구한다.
- Work Item 이후 Attempt/phase/progress/validation/cleanup evidence를 timeline으로 보여준다.
- native restore exit 0 뒤 validation 실패를 success로 렌더링하지 않는다.

### Repair, verify, and drill

- Replica repair는 적격 source와 target Destination을 선택하고 영향/preflight 뒤 enqueue한다.
- manual L1/L2/L3는 capability를 보여주고 unsupported level을 silent downgrade하지 않는다.
- restore drill은 production target과 분리된 registered test target에 실행하고 RTO/evidence를
  Snapshot과 연결한다.
- Catalog rebuild preview는 accepted/rejected/duplicate/unknown key를 먼저 보여준다.

### Safety and accessibility

- token/secret/source connection string을 frontend storage, URL, Activity copy에 넣지 않는다.
- restore cancel/실패 후 생성 target을 자동 삭제하지 않고 cleanup required를 표시한다.
- destructive confirmation modal은 keyboard trap/return, target name accessible description,
  reduced motion, screen reader summary를 제공한다.
- error state에서도 source Snapshot과 target을 혼동하지 않도록 두 영역을 고정 배치한다.

## Dependencies

- **Requires**: section-06 real restore/certification, section-07 UI foundation
- **Blocks**: section-10 launch/expansion
- **Parallel**: section-09 new engine provider와 병렬 가능; UI는 Engine Matrix capability를
  데이터로 소비한다.

## Flow Diagram Nodes

`snapshot-restore.mmd`의 UI/safety node를 소유한다.

| Node | 구현 |
|---|---|
| `selectSnapshot` | eligible source를 inventory에서 선택 |
| `chooseMode` | verify/create-new/overwrite |
| `overwrite` | destructive branch 결정 |
| `issueToken` | service prepare-confirmation 호출과 final summary |
| `confirmIdentity` | 입력 identity와 bound resource 확인 |
| `stopped` | source/target/evidence를 보존한 안전한 중단 상태 |

backend eligibility/preflight/execute/validation node는 section-06 command를 사용한다.

## Reference Libraries

| Library | 기준 | 용도 |
|---|---|---|
| React/TypeScript | lockfile | inventory/workspace |
| Tauri v2 client | lockfile/공식 문서 | IPC |
| virtualizer가 필요하면 | 구현 전 공식 문서와 bundle/a11y 검토 | 대규모 Snapshot |
| Playwright | test pin | recovery E2E/visual/a11y |

root `DESIGN.md`와 section-07 primitives를 재사용하고 별도 Recovery 전용 디자인 시스템을
만들지 않는다.

## Implementation Details

### 1. Snapshot page

InventoryTable과 inspector를 사용해 네 상태 축을 각각 column/filter로 제공한다. 단일 종합
초록 badge를 만들지 않는다. selected row는 Manifest/Part/Replica/verification query를
lazy load하고 stale/ingest source를 표시한다.

### 2. Diagnostics and actions

incomplete/orphan/rejected Manifest는 기본 scope 밖 diagnostics tab에서 원인, age, object,
cleanup/repair/rebuild action을 제공한다. destructive cleanup은 retention confirmation contract를
사용한다.

### 3. Recovery state machine

workspace local state는 Snapshot ref, Replica ref, mode, target mapping, prepare result,
confirmation, Work Item ref로 명시한다. 단계 이동마다 backend authoritative data를 다시 읽고
prepare result version/fingerprint가 바뀌면 이전 confirmation을 폐기한다.

### 4. Preflight and confirmation

preflight result를 pass/fail/warning/unknown으로 렌더링하고 실패 항목에 수정 경로를 제공한다.
overwrite final step에서 source와 target summary, destructive effect, cleanup policy를 읽고
target identity를 재입력한다. raw token은 해당 submit memory에서만 사용하고 즉시 폐기한다.

### 5. Restore timeline

Work Item/Attempt/Activity를 phase timeline으로 합성하되 backend state를 바꾸지 않는다.
queued reason, running process, validation, terminal/cleanup required를 구분한다. 앱 재시작 시
Restore Run ID로 workspace를 복구한다.

## Test Scenarios

### Normal

| ID | 행동 | 기대 |
|---|---|---|
| S08-N01 | complete/protected/L3 Snapshot 선택 | 모든 evidence와 create-new action |
| S08-N02 | degraded/non-compliant Local Replica 선택 | 경고 유지, 정책상 복원 가능 |
| S08-N03 | create-new restore | token 없이 preflight→Work Item→validated success |
| S08-N04 | overwrite restore | identity/token 일치한 한 요청만 실행 |
| S08-N05 | Replica repair | source/target preflight와 progress/compliance 갱신 |

### Error/Security

| ID | 행동 | 기대 |
|---|---|---|
| S08-E01 | ineligible/invalid Manifest Snapshot | restore action 차단 |
| S08-E02 | preflight space/tool/permission 실패 | process 0회와 수정 action |
| S08-E03 | token 다른 SID/target/payload/expired | 상태 변경 0, stable error |
| S08-E04 | consumed token replay | 실행 0, 감사 event |
| S08-E05 | restore exit 0/oracle fail | 실패/cleanup required, success 없음 |
| S08-E06 | unsupported L3 enqueue | explicit unsupported, L1 downgrade 없음 |

### Edge/Fault

| ID | 행동 | 기대 |
|---|---|---|
| S08-X01 | prepare 뒤 target fingerprint 변경 | confirmation 폐기와 재-preflight |
| S08-X02 | 앱 종료/재시작 중 restore | service 작업 유지, Run ID로 resume |
| S08-X03 | restore cancel/crash | target 자동 삭제 없음, cleanup evidence |
| S08-X04 | Catalog rebuild 중 목록 조회 | stale/refresh 상태, source confusion 없음 |
| S08-X05 | keyboard-only/200% zoom | full recovery journey 통과 |

## Implementation Strategy

### Red

four-axis projection, blocked source, create-new, overwrite token variants, validation failure,
resume/cancel, accessibility E2E를 먼저 작성한다.

### Green

Snapshot inventory/inspector, Recovery state machine, preflight/confirmation/timeline을 section-07
primitives와 IPC command로 구현한다.

### Refactor

backend command state와 UI step state를 분리하고 Snapshot evidence, confirmation summary,
Activity timeline mapping을 pure selector로 만든다.

## Quality Gate G3-B

- [ ] incomplete/invalid Snapshot의 restore action 0
- [ ] degraded와 non-compliant를 protected로 표시한 사례 0
- [ ] create-new 실제 restore/oracle E2E 통과
- [ ] overwrite token mismatch/replay process 시작 0
- [ ] process 성공/validation 실패 false-success 0
- [ ] cancel/crash 뒤 cleanup evidence
- [ ] keyboard/zoom/theme/a11y matrix 통과
- [ ] Snapshot ID/connection string 수동 입력 없는 journey

## Risk and Rollback

| Risk | Mitigation | Rollback |
|---|---|---|
| source/target 혼동 | 고정 summary와 fingerprint | overwrite action disable |
| token frontend 잔존 | memory-only/즉시 폐기 | token revoke/session reset |
| false recovery success | validation terminal 필수 | success projection 재계산 |
| 오래된 eligibility | submit 직전 authoritative recheck | prepare 재실행 |
| 복잡한 wizard | one decision/step와 resumable Run | create-new만 유지 |

## Acceptance Criteria

- [ ] 사용자가 Snapshot evidence를 보고 안전한 source를 선택한다.
- [ ] 새 DB 복원이 기본이고 overwrite에는 강한 마찰이 있다.
- [ ] 복원 실패와 정리 필요 상태가 숨겨지지 않는다.
- [ ] verify/repair/rebuild/restore가 같은 Activity 모델로 추적된다.
- [ ] 실제 복원 가능성이 UI의 중심 가치로 보인다.

## Files to Create/Modify

```text
apps/console/src/screens/{Snapshots,Recovery}.tsx
apps/console/src/components/{SnapshotInspector,ReplicaTable,RecoveryWorkspace,PreflightResult,ConfirmationSummary,RestoreTimeline}.tsx
apps/console/src/lib/{snapshot,recovery}.ts
apps/console/tests/{snapshots,recovery-create-new,recovery-overwrite,recovery-resume}.spec.ts
apps/console/src-tauri/src/commands.rs
```
