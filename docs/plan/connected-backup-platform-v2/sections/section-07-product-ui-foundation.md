# Section 07 — Connected Backup Product UI Foundation

## Background

현재 콘솔은 setup, Database/Destination/Job 등록, run panel, restore, schedule 화면이 있지만
server connection과 개별 DB의 관계, durable Activity, Snapshot/Replica 상태가 제품의 중심으로
보이지 않는다. v2 UI는 Databasement의 연결 중심 정보 구조를 기준선으로 삼되 시각적 복제는
하지 않는다. root `DESIGN.md`의 calm teal security console과 실제 운영 밀도를 유지한다.

고정 navigation은 다음 순서다.

```text
Overview
Connections
Databases
Destinations
Backup Plans
Activity
Snapshots
Recovery
Settings
```

이 섹션은 공통 shell과 Overview~Activity까지 완성한다. Snapshot/Recovery의 전문 workspace는
section-08이 구현한다.

## Requirements

### Application data boundary

- React는 Tauri thin wrapper를 통해 section-03 local IPC client만 호출한다.
- native tool, config file, SQLite, Secret Store를 frontend에서 직접 접근하지 않는다.
- long command는 Work Item ID를 받고 Activity sequence/cursor로 진행을 갱신한다.
- stable error code를 user action 중심 copy로 매핑하고 debug 문자열을 표시하지 않는다.
- filter/cursor/selection은 route 전환과 뒤로가기에서 복원한다.

### Information architecture

- Connection inventory와 그 아래 discovered/selected Database Target을 명확히 구분한다.
- Databases는 unprotected, drift, capability, RPO/verification 상태로 탐색할 수 있다.
- Destination은 writer/reader/delete probe와 Plan 사용 영향, 실제 capability를 보여준다.
- Backup Plan wizard는 Target→Destination role→Schedule/RPO→Retention/Verification→Preflight
  순서이며 한 단계에 하나의 결정만 요구한다.
- Activity는 queued/running/failed를 숨기지 않고 wait reason, lease/phase, retry/cancel,
  관련 객체를 보여준다.
- Overview는 최근 성공 하나로 정상이라 단정하지 않고 Attention, 최신 실패, RPO,
  verification freshness, service/queue health를 분리한다.

### Visual and accessibility

- root `DESIGN.md`와 `design-system.md`가 token 정본이다. 신규 색은 token과 `oklch()`를 사용한다.
- Pretendard를 한글 기본, Space Grotesk heading, JetBrains Mono technical value로 실제 load하고
  사용하는 weight만 포함한다.
- 균일한 카드 그리드 대신 inventory table+inspector와 비대칭 운영 밀도를 사용한다.
- 모든 input/select/checkbox는 programmatic label/name/description/error를 가진다.
- keyboard row navigation, focus trap/return, live region, table header/sort/selection을 제공한다.
- root `color-scheme: light dark`, container/text/select option/placeholder/autofill/border/
  scrollbar가 theme token을 따른다.
- page viewport는 1024/1280/1600, 200% zoom을 통과한다. component response는 container query를
  우선하고 reduced motion을 존중한다.
- 상태는 색만으로 구분하지 않고 text와 evidence freshness를 함께 표시한다.

### Scale and truth

- Activity 100,000건에서 keyset pagination/virtualization과 bounded DOM을 사용한다.
- loading/empty/error/stale/partial/permission 상태를 모든 page archetype에 구현한다.
- 메뉴 badge는 총 개수가 아니라 사용자가 개입해야 하는 Attention 수만 표시한다.
- `configured`, `unknown`, `experimental`, `degraded`, `non_compliant`를 success로 칠하지 않는다.

## Dependencies

- **Requires**: section-02 domain/query, section-03 application IPC
- **Blocks**: section-08 Snapshot/Recovery UI
- **Parallel**: section-06 engine certification과 공통 response types가 고정된 뒤 병렬 가능

## Flow Diagram Nodes

`connection-onboarding.mmd`의 UI node를 소유한다.

| Node | UI responsibility |
|---|---|
| `start`, `enterProfile` | engine/endpoint/auth 단계형 입력 |
| `showAction`, `endFailed` | probe 오류 원인·재시도, 비활성 draft 보존 |
| `chooseTargets` | discovered DB 중 보호 대상 선택 |
| `profileReady` | Connection detail과 다음 Plan action |

service/store node는 section-02/03을 호출한다. wizard cancel은 진행 중 probe를 명시적으로
취소하되 이미 저장된 draft/secret lifecycle을 application command로 정리한다.

## Reference Libraries

| Library | 기준 | 용도 |
|---|---|---|
| React | 현행 18.x lockfile | component/state |
| TypeScript | 현행 5.x lockfile | IPC discriminated types |
| Tauri v2 JS API | lockfile/공식 문서 확인 | thin invoke/subscription |
| Vite | lockfile | build |
| CSS baseline APIs | 구현 시 MDN 확인 | container query, `:has`, view transition |
| Playwright | test toolchain pin | E2E/visual/a11y |

새 component library를 자동 도입하지 않는다. 필요한 table, drawer, badge, stepper를 현재 token과
접근성 요구에 맞춰 작은 primitives로 만든다.

## Implementation Details

### 1. Shell and routing

`App.tsx`의 route/navigation을 새 IA로 바꾸고 service status를 global shell에 둔다. nav badge,
global stale/offline banner, command palette 없이 명확한 primary navigation을 우선한다.
old Jobs/Schedule/Restore route는 새 Plan/Activity/Recovery로 redirect하거나 migration 안내 후
제거한다.

### 2. IPC query/mutation layer

`lib/api.ts`를 typed command client로 교체하고 cursor page, Work Item result, stable error,
resource ref를 공통 type으로 둔다. query cache는 stale timestamp를 보존하고 mutation은
idempotency key를 생성·재사용한다. destructive token을 일반 cache/local storage에 저장하지
않는다.

### 3. Common components

- InventoryTable: filter/sort/keyset/keyboard/selection
- Inspector: selected resource, evidence, related links
- GuidedWorkspace: step, inline probe/preflight, cancel/retry
- StatusBadge: state text, reason, freshness
- CapabilityMatrix: supported/limited/experimental/unsupported
- ActivityTimeline: phase, Attempt, wait reason, retryability
- Empty/Error/Stale states와 ConfirmationSummary

### 4. Feature pages

- **Overview**: protection summary strip, Attention list, queue/permit panel, Target protection table
- **Connections**: inventory, add/edit wizard, latest probe/discovery, Target children
- **Databases**: protection/capability/drift inventory와 Plan entry
- **Destinations**: role별 probe/capability/impact
- **Backup Plans**: immutable revision list와 guided create/revise
- **Activity**: virtualized list, structured timeline/log, cancel/retry, related navigation
- **Settings foundation**: runtime permit, tool, notification, control backup sections

### 5. Copy and migration

“모든 백업이 안전”, “설정됨=업로드됨”, “복호화=복원 성공” 같은 copy를 금지한다. 기존 route와
config를 발견하면 v2 migration 상태와 실제 사용 가능한 action을 표시한다.

## Test Scenarios

### Normal

| ID | 행동 | 기대 |
|---|---|---|
| S07-N01 | Connection 등록→probe→DB 선택 | 한 흐름에서 Connection detail과 Target 생성 |
| S07-N02 | unprotected DB에서 Plan 생성 | Target preselected와 capability 표시 |
| S07-N03 | queued Activity 선택 | wait permit/next time/related objects |
| S07-N04 | route 이동 후 뒤로가기 | filter/cursor/selection 복원 |
| S07-N05 | light/dark 전환 | 모든 container/text/form option 정상 대비 |

### Error/Edge

| ID | 행동 | 기대 |
|---|---|---|
| S07-E01 | service offline | stale read-only와 재연결, false healthy 없음 |
| S07-E02 | probe auth 실패 | secret echo 없이 field/action error |
| S07-E03 | capability mismatch | Plan activation 차단과 limitation |
| S07-E04 | 100k Activity | bounded DOM, keyset, latency budget |
| S07-E05 | 긴 한글/영문 DB 이름 | truncate+accessible full label, layout 유지 |
| S07-E06 | keyboard-only/200% zoom | 핵심 5 journey와 focus 통과 |
| S07-E07 | subscription disconnect | last sequence 이후 재개, 작업 취소 없음 |

## Implementation Strategy

### Red

접근성 tree, route state, loading/error/stale, 100k Activity, dark/light trap, copy guard,
Connection onboarding E2E를 먼저 실패시킨다.

### Green

shell, typed IPC, common primitives, Connections/Databases/Destinations/Plans/Activity를 구현한다.
기존 기능은 새 application command가 준비된 순서로 strangler 전환한다.

### Refactor

페이지별 중복 fetch/form/status logic을 hooks/primitives로 정리하되 거대한 generic form/table을
만들지 않는다. React profiler와 render count로 virtualization/query update를 조정한다.

## Quality Gate G3-A

- [ ] first connection→Target 선택→Plan 진입이 keyboard-only로 가능
- [ ] input/select/checkbox programmatic label 누락 0
- [ ] 9 navigation과 back-state 정상
- [ ] 100k Activity 성능/DOM budget 통과
- [ ] dark/light/200%/1024~1600 visual matrix 통과
- [ ] 실제 font face load 확인
- [ ] configured/unknown/degraded false-success copy 0
- [ ] frontend build/typecheck/Playwright/a11y 통과

## Risk and Rollback

| Risk | Mitigation | Rollback |
|---|---|---|
| 대규모 UI 일괄 교체 | route별 strangler와 contract test | 아직 전환 안 된 route 유지 |
| IPC type drift | generated/golden DTO test | incompatible command 차단 |
| virtualized table a11y | keyboard/focus fixture | page-size fallback |
| 디자인 drift | DESIGN.md token lint/visual baseline | token 위반 patch revert |
| stale 상태 false green | freshness 필수 field | read-only/stale banner |

## Acceptance Criteria

- [ ] 사용자가 서버와 그 안의 여러 DB를 한눈에 이해한다.
- [ ] 연결에서 첫 보호 계획까지 중앙값 5분 목표를 측정할 수 있다.
- [ ] queue wait와 실패 원인을 로그 해석 없이 알 수 있다.
- [ ] 모든 상태가 실제 backend evidence 범위를 넘지 않는다.
- [ ] UI가 Databasement 수준의 제품 흐름을 제공하면서 프로젝트 디자인 정본을 유지한다.

## Files to Create/Modify

```text
apps/console/src/App.tsx
apps/console/src/lib/{api,types,queries,errors}.ts
apps/console/src/components/{InventoryTable,Inspector,GuidedWorkspace,StatusBadge,CapabilityMatrix,ActivityTimeline}.tsx
apps/console/src/screens/{Overview,Connections,Databases,Destinations,BackupPlans,Activity,Settings}.tsx
apps/console/src/styles/{tokens,shell,components}.css
apps/console/tests/{onboarding,navigation,activity,theme,accessibility}.spec.ts
apps/console/src-tauri/src/commands.rs
```
