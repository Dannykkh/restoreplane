# Connected Backup Platform v2 Design System

> 정본: `DESIGN.md`
> 이 문서는 신규 제품 흐름에 정본 토큰을 적용하는 사용 규칙만 정의한다. 색, 글꼴, 간격,
> 라운드 값을 새로 만들지 않는다.

## Design Direction

방향은 “침착한 운영 제어실”이다. Databasement의 정보구조를 참고하되 외형을 복제하지
않는다. 기본 상태에서는 많은 DB를 조용히 관리하고, 사용자가 개입해야 하는 실패·RPO
위반·복원 위험만 날카롭게 드러낸다.

## Navigation

왼쪽 고정 내비게이션을 다음 순서로 사용한다.

1. Overview
2. Connections
3. Databases
4. Destinations
5. Backup Plans
6. Activity
7. Snapshots
8. Recovery
9. Settings

메뉴 배지는 총 개수 대신 사용자가 개입해야 하는 수만 표시한다. 예를 들어 Activity는
실패·stuck 수, Databases는 unprotected 수, Snapshots는 verification overdue 수를 표시한다.

## Page Archetypes

### Inventory Page

Connections, Databases, Destinations, Backup Plans, Snapshots에 사용한다.

- 좌측: 필터와 저장된 view
- 중앙: 밀도 높은 table/list
- 우측: 선택 항목 inspector
- 생성 동작은 별도 modal보다 page-level drawer 또는 단계형 workspace를 우선한다.
- table row 전체를 카드로 만들지 않는다.

### Guided Workspace

연결 등록, plan 생성, restore에 사용한다.

- 한 단계에서 하나의 결정만 요구한다.
- 진행 상태를 상단에 고정한다.
- probe/preflight 결과를 다음 단계 전에 인라인으로 보여준다.
- destructive restore는 대상 이름 재입력과 최종 summary를 요구한다.

### Activity Workspace

- 왼쪽에 실행 목록, 오른쪽에 단계 timeline과 구조화 로그를 둔다.
- `queued → leased → running → verifying → completed` 상태를 시간축으로 보여준다.
- 원시 로그는 접힌 보조 정보이고, 사람이 조치할 오류가 먼저 보인다.

## Overview Composition

균일한 KPI 카드 그리드를 피하고 비대칭으로 구성한다.

```text
┌─────────────────────────────────────────────────────────────────┐
│ 보호 상태 요약                         마지막 갱신 / Queue 상태 │
├──────────────────────────────────┬──────────────────────────────┤
│ 개입 필요 목록                   │ 실행 중 / 대기 중            │
│ 실패, RPO 위반, 검증 기한 초과   │ host·destination permit 이유 │
├──────────────────────────────────┴──────────────────────────────┤
│ DB별 보호 현황 표: 마지막 백업 / L3 / 다음 일정 / 목적지       │
└─────────────────────────────────────────────────────────────────┘
```

“모두 정상”은 큰 장식이 아니라 확인 가능한 근거와 함께 보여준다. 최근 실패가 있으면 최근
성공과 나란히 표시한다.

## Component Rules

### Status Badge

- 상태명과 의미를 텍스트로 포함한다.
- `success`: committed 또는 정책 내 검증 완료
- `warning`: 대기, 검증 기한 임박, RPO 임박
- `danger`: 실패, RPO 위반, 서명 거부
- `immutable`: Object Lock 또는 origin signature
- “verified” 단독 표기를 금지하고 `L1`, `L2`, `L3`를 함께 쓴다.

### Capability Matrix

엔진 상세와 plan wizard에서 공통으로 사용한다.

| Capability | 표현 |
|------------|------|
| 지원·검증됨 | 상태 배지 + 검증된 tool/version |
| 조건부 | 조건과 제한을 같은 셀에 표시 |
| 지원 안 함 | 빈칸 대신 명시적 `지원 안 함` |
| 미검증 | `실험적` 또는 `검증 필요`, plan 기본 선택 불가 |

### Forms

- 모든 input/select/checkbox는 실제 `label`과 `id/name`을 연결한다.
- 설명은 `aria-describedby`, 오류는 `aria-invalid`와 error id로 연결한다.
- secret은 표시/숨김과 저장 위치를 함께 설명한다.
- 다크·라이트 양쪽에서 `select > option`, placeholder, autofill을 검증한다.
- probe 중에는 입력을 무조건 잠그지 않고 취소와 재시도를 제공한다.

### Tables

- 기술값은 JetBrains Mono를 사용한다.
- 행 상태를 색 띠 하나로만 표현하지 않는다.
- 페이지네이션과 keyboard row navigation을 제공한다.
- 화면 폭이 줄면 container query로 우선순위가 낮은 열을 inspector로 이동한다.

## Typography and Font Loading

- 본문과 한글 UI: Pretendard
- 라틴 헤딩: Space Grotesk 600, 한글은 Pretendard로 자연스럽게 fallback
- checksum, 버전, object key, 크기, 시각: JetBrains Mono 400/500
- 실제 사용하는 weight만 로드하고 브라우저에서 font face 상태를 검증한다.

## Motion

- 새 페이지 진입 시 목록과 inspector가 한 번만 orchestrated transition한다.
- queue 상태 변경은 배지의 opacity 전환만 사용한다.
- 로그 자동 스크롤은 사용자가 아래에 있을 때만 수행한다.
- `prefers-reduced-motion`에서 모든 비필수 동작을 제거한다.
- View Transitions API는 점진적으로 적용하며 기능 의존성을 만들지 않는다.

## Responsive Behavior

데스크톱 앱의 최소 지원 폭을 정의하되, 컴포넌트는 container query로 적응한다.

- 넓음: list + inspector 동시 표시
- 중간: inspector를 overlay panel로 전환
- 좁음: filter, list, detail 순서의 단일 column
- 전역 shell 전환만 media query를 사용한다.

## Copy Rules

- “안전합니다” 대신 근거를 쓴다: `L3 복원 검증: 2일 전`.
- “연결 오류” 대신 분류를 쓴다: `인증 실패`, `TLS 인증서 불일치`.
- “지원됨” 대신 조건을 쓴다: `PostgreSQL 14–17 / pg_dump 동일 major 검증`.
- 성공률 숫자로 실패를 숨기지 않는다.

## Visual Verification Gate

- dark와 light 테마의 모든 핵심 화면 screenshot
- 1024px, 1280px, 1600px shell 확인
- keyboard-only 주요 여정
- browser accessibility tree에서 label/name 확인
- `document.fonts`로 실제 font face 로드 확인
- 실패·빈 상태·대량 데이터·긴 DB 이름·긴 오류 메시지 확인
