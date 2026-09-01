# UX Analysis — Connected Backup Platform v2

> 검토일: 2026-07-26
> 범위: 사용자 여정, 정보구조, 사용성, 접근성, 오류 복구, 최초 사용, 대량 운영
> 기준 문서: `spec.md`, `interview.md`, `research.md`, `design-system.md`,
> `personas-and-journeys.md`, `docs/domain-dictionary.md`
> 구현 표본: `apps/console/src/App.tsx`, `screens/Setup.tsx`, `Databases.tsx`,
> `Destinations.tsx`, `Jobs.tsx`, `Schedule.tsx`, `Restore.tsx`, `Dashboard.tsx`

## Executive Verdict

제품 방향은 사용자 문제와 맞고, “침착한 운영 제어실”이라는 디자인 원칙도 적절하다.
그러나 현재 UI는 여전히 보안 실행 코어의 설정 화면에 가깝다. v2가 요구하는 핵심 흐름인
`Connection Profile → Database Target → Backup Plan → Backup Run → Snapshot → Restore Run`이
화면과 상호작용에 아직 투영되지 않았다.

출시 전에 반드시 해결해야 할 축은 다음 네 가지다.

1. 연결 등록과 DB 탐색을 분리하고 첫 백업까지 이어지는 안내 흐름을 만든다.
2. 수동 백업 ID 입력을 제거하고 Snapshot에서 Restore Eligibility를 거쳐 복원을 시작한다.
3. persistent queue와 Activity를 사용자가 이해하고 복구할 수 있는 운영 표면으로 만든다.
4. 실제 지원 capability, 오류, 상태, form 관계를 화면과 접근성 트리에 동일하게 노출한다.

## Journey Coverage

| 사용자 목표 | 설계상 목표 상태 | 현재 UI 상태 | 판정 |
|------------|----------------|-------------|------|
| 연결을 한 번 등록하고 여러 DB 선택 | 실제 probe 후 discovery, Target 다중 선택 | DB 이름까지 직접 넣어 DB 한 개씩 등록 | 차단 |
| 목적지와 일정을 묶어 계획 생성 | 단계형 workspace, capability와 preflight 선확인 | 한 화면의 긴 `Job` form, raw cron 입력 | 부분 |
| 오늘 개입할 문제 확인 | 실패·RPO·검증 기한·queue 이유 중심 Overview | 마지막 backup 기반 job 카드와 종합 문구 | 차단 |
| 실행 지연·실패 원인 판단 | persistent Activity와 단계 timeline | 컴포넌트 메모리의 일회성 원시 로그 | 차단 |
| 스냅샷을 찾아 안전하게 복원 | Snapshot 검색에서 eligibility 확인 후 Restore Run | 백업 ID와 대상 문자열 수동 입력 | 차단 |
| 장애 후 inventory 재구축 | reader 자격증명으로 catalog rebuild | 해당 사용자 여정 없음 | 차단 |
| 엔진별 지원 범위 판단 | tool/version을 포함한 capability matrix | L1/L2/L3를 공통 선택 가능 | 차단 |

## Critical Findings

### C-01. v2 도메인과 현재 정보구조가 일치하지 않는다

현재 `App.tsx`는 `대시보드`, `DB · 목적지`, `백업 실행`, `복원`, `스케줄 · 알림`의 다섯
화면만 제공한다. `Registrations.tsx`는 연결과 목적지를 한 메뉴에 탭으로 묶고,
`Jobs.tsx`는 계획 생성과 실행 미리보기를 한 화면에 섞는다. 반면 v2 명세의 핵심 운영
객체인 Connections, Databases, Backup Plans, Activity, Snapshots, Recovery는 서로 다른
수명주기와 판단 기준을 가진다.

이 상태에서는 사용자가 서버 연결, 보호할 DB, 계획, 한 번의 실행, 복원 가능한 백업을
구분할 수 없고 코드의 과적재된 `Job` 모델도 UI에서 고착된다.

필수 조치:

- 명세의 9개 영역을 그대로 복제하기보다 사용자 작업 순서에 맞춰 전역 내비게이션과
  객체별 상세 화면을 재구성한다.
- Connections와 Databases를 분리하되, 연결 상세에서 발견 DB 선택으로 자연스럽게
  이어지게 한다.
- Backup Plans는 선언, Activity는 실행, Snapshots는 커밋된 산출물로 일관되게 분리한다.
- 객체 간 이동은 이름과 관계를 유지한다. 예: Activity의 실행에서 Plan, Target,
  Snapshot으로 역추적할 수 있어야 한다.

검증 기준:

- 서로 다른 생명주기의 객체가 같은 “잡” 화면에서 수정·실행 상태를 공유하지 않는다.
- 모든 persona의 주 여정을 수동 ID나 설정 파일 없이 전역 내비게이션에서 시작할 수 있다.

### C-02. 연결 등록이 실제 사용자 목표인 “한 서버의 여러 DB 보호”를 지원하지 않는다

`Databases.tsx`는 host, port, DB 이름, 계정, 비밀번호를 한 번에 받아 DB 하나를 등록한다.
실제 probe, 서버·tool 버전 확인, 권한 진단, DB discovery, 제외 사유, 다중 Target 선택,
재탐색 diff가 없다. 이는 5분 안에 연결에서 첫 백업까지 완료한다는 목표와 혼합 DB 운영자의
핵심 여정을 직접 차단한다.

필수 조치:

- 1단계에서 Connection Profile만 입력하고 `연결 확인`으로 실제 probe를 수행한다.
- probe 결과를 DNS, 네트워크, TLS, 인증, 권한, native tool로 분해해 다음 조치를 제공한다.
- 2단계에서 발견된 DB를 검색·다중 선택하고 system/template/offline 제외 사유를 표시한다.
- 첫 저장 후 `목적지 연결` 또는 `백업 계획 만들기`로 이어지는 명확한 다음 동작을 제공한다.
- 재탐색은 추가·누락·이름 변경 후보를 diff로 보여주며 기존 Plan과 Snapshot을 자동 삭제하지
  않는다.

검증 기준:

- 한 Connection Profile에서 20개 이상의 Database Target을 한 번에 등록할 수 있다.
- 접속 실패 시 사용자가 원인을 추정하기 위해 원시 stderr를 읽지 않아도 된다.

### C-03. 현재 복원 화면은 잘못된 대상을 선택할 가능성이 높고 장애 복구 여정을 차단한다

`Restore.tsx`는 사용자가 백업 ID를 직접 입력하고, 목적지와 임의의 대상 문자열을 넣게 한다.
Snapshot 검색, 검증 수준, 서명 상태, tool/version 호환성, reader 상태, 기존 DB 충돌 정책,
preflight summary가 없다. 위험 확인도 일반 체크박스 하나뿐이다. 이 방식은 장애 복구
담당자의 가장 큰 위험인 잘못된 대상 복원을 줄이지 못한다.

필수 조치:

- 복원 진입점을 Snapshot 행과 상세 inspector의 `복원` 동작으로 제한한다.
- Restore Eligibility를 서명, L1/L2/L3, tool/version, reader, key material, 목적지 접근으로
  분해해 실행 전에 표시한다.
- 복원 대상은 등록된 Connection Profile과 discover 가능한 DB를 사용해 선택한다.
- 기본값은 새 DB 이름 또는 격리된 test target이며, overwrite는 별도 고위험 경로로 둔다.
- destructive restore는 대상 이름 재입력, 변경 영향, 최종 summary 확인을 요구한다.
- catalog 손실 상황을 위한 `목적지에서 Snapshot 찾기`와 catalog rebuild 진입점을 제공한다.

검증 기준:

- 사용자는 Snapshot ID와 연결 문자열을 복사해 입력하지 않고 복원을 완료할 수 있다.
- overwrite는 일반 확인 체크박스만으로 시작할 수 없다.
- eligibility gate 실패는 실패한 조건과 안전한 다음 동작을 함께 제시한다.

### C-04. 실행 상태와 오류가 휘발성 UI 상태에 갇혀 있어 재시작·인계·재시도가 불가능하다

`Jobs.tsx`, `Schedule.tsx`, `Restore.tsx`는 실행 로그를 컴포넌트 state 배열에 누적한다.
화면을 닫거나 앱을 재시작하면 운영 문맥이 사라지며, 로그 닫기와 실제 작업 취소의 의미도
분명하지 않다. persistent queue, lease, retry, stage timeline, evidence라는 명세와 맞지
않는다.

필수 조치:

- 모든 수동·예약 backup과 restore를 동일 Work Item 경로로 보내고 Activity에 영속화한다.
- 목록에는 상태, 단계, 대상, 시작·대기 시간, retry, 실패 분류를 표시한다.
- 상세 inspector에는 단계 timeline과 사람이 읽는 오류를 우선하고 원시 로그를 보조로 둔다.
- `패널 닫기`, `실행 취소`, `재시도`, `다시 계획`을 서로 다른 동작과 확인 수준으로 제공한다.
- 앱 재시작 뒤 `leased`와 `running` 작업을 복구 중 상태로 표시하고 orphan 판단 결과를 남긴다.

검증 기준:

- 앱을 강제 종료하고 다시 열어도 실행 상태와 마지막 의미 있는 event를 찾을 수 있다.
- 같은 오류의 재시도가 별도 실행으로 흩어지지 않고 원 실행과 연결된다.

### C-05. 핵심 form의 programmatic label과 오류 연결이 대부분 누락됐다

현재 여러 화면의 시각적 `<label>`은 `htmlFor`가 없고 대응 input/select에 `id`나 `name`도
없다. 예시는 `Databases.tsx`, `Jobs.tsx`, `Restore.tsx`, `Setup.tsx`의 대부분 필드다.
성공·실패 flash와 field error도 `aria-live`, `aria-invalid`, `aria-describedby` 연결이 없다.
버튼 비활성화 사유도 스크린리더와 키보드 사용자에게 설명되지 않는다.

필수 조치:

- 모든 input, select, checkbox에 안정적인 `id/name`을 주고 `label htmlFor`로 연결한다.
- hint와 error id를 만들고 `aria-describedby`, `aria-invalid`를 상태에 따라 설정한다.
- 비동기 probe, 저장, 실행 결과는 적절한 `aria-live` 영역으로 알린다.
- stepper, tabs, navigation에 현재 상태를 `aria-current`, `role=tablist`, `aria-selected` 등으로
  노출한다.
- 아이콘 전용 또는 의미가 불명확한 버튼은 accessible name을 갖는다.
- 비활성화된 제출 버튼 주변에 충족되지 않은 필수 조건을 텍스트로 제공한다.

검증 기준:

- browser accessibility tree에서 모든 필드의 name, description, error 관계를 확인한다.
- 키보드만으로 연결 생성, 계획 생성, Snapshot 검색, 복원 확인을 완료한다.

### C-06. 지원 capability가 선택 전에 차단되지 않아 “지원됨”이라는 잘못된 기대를 만든다

`Jobs.tsx`는 엔진·도구 상태와 무관하게 L1, L2, L3를 모두 선택 가능하게 한다.
Destination 선택도 실제 write/read/delete/list/object lock probe 결과보다 등록 종류와
미리 계산한 배지에 의존한다. 이 UI는 미검증 기능을 숨기거나 `지원 안 함`으로 표시하라는
명세와 충돌한다.

필수 조치:

- Connection, Destination, Plan wizard에 공통 capability matrix를 사용한다.
- 상태를 `검증됨`, `조건부`, `지원 안 함`, `미검증`으로 구분하고 tool/version 근거를 함께
  표시한다.
- 지원하지 않는 옵션은 단순 disabled 상태로 끝내지 말고 제한과 대안을 같은 위치에 쓴다.
- L3를 선택하면 test target, 예상 자원, 주기, 최근 성공 evidence를 확인하게 한다.
- preview capability와 실제 probe capability를 시각적으로 구분한다.

검증 기준:

- Support Contract를 통과하지 않은 엔진·도구 조합은 지원 기능으로 선택할 수 없다.
- 사용자는 계획 저장 전에 capability 불일치와 해결 방법을 확인한다.

### C-07. queue와 Resource Permit이 운영자에게 설명되지 않는다

다중 DB 환경에서 중요한 것은 작업 수가 아니라 왜 기다리고 있고 원본 DB에 어떤 부하가
걸리는지다. 현재 UI는 Windows Task Scheduler 등록과 개별 잡 실행만 보여주며 queue depth,
lease, host permit, CPU permit, destination permit, 예상 시작 시각을 제공하지 않는다.
사용자는 정상 backpressure와 stuck 상태를 구분할 수 없다.

필수 조치:

- Overview와 Activity에 실행 중·대기 중 수와 주요 대기 사유를 표시한다.
- 각 queued Work Item에 `같은 호스트의 백업 실행 중`, `복원 우선`, `목적지 업로드 제한`처럼
  사람이 이해하는 이유를 제공한다.
- Settings의 concurrency는 숫자만 노출하지 말고 영향, 안전 권장값, 현재 사용량을 함께
  보여준다.
- `expired lease`, 장시간 queued, RPO 임박·위반을 일반 대기와 분리해 attention 상태로 둔다.

검증 기준:

- 10개 plan이 동시에 due일 때 사용자는 어떤 두 작업이 실행되고 나머지가 왜 대기하는지
  한 화면에서 확인한다.
- 앱 재시작 후에도 queue 상태가 빈 화면이나 “처리 중”으로 초기화되지 않는다.

## Important Findings

### I-01. 최초 설정이 보안 키 생성에서 끝나고 첫 가치까지 이어지지 않는다

`Setup.tsx`의 키스토어와 복구코드 흐름은 보안적으로 명확하지만, 완료 후 빈 Dashboard로
이동한다. 사용자는 제품의 주목적인 연결형 백업보다 보안 제품을 먼저 경험하며, 다음
선택지를 스스로 추론해야 한다.

개선안:

- 설정 완료 후 `첫 연결 추가`를 기본 동작으로 제공한다.
- 상단에 `연결 → DB 선택 → 목적지 → 계획 → 첫 백업 확인` 체크리스트를 유지한다.
- 복구코드 보관은 복사 외에 파일 저장 또는 인쇄 등 안전한 보관 수단을 제공하되 저장 위치와
  위험을 설명한다.
- 중단 후 재진입하면 완료한 단계부터 이어가고 입력한 비밀은 불필요하게 복구하지 않는다.

### I-02. Plan 생성 form이 한 화면에 과밀하며 운영자가 capability와 결과를 예측하기 어렵다

현재 `Jobs.tsx`는 이름, DB, 목적지, 불변성, 검증 레벨, cron, 보존 수치를 한 번에 요구한다.
다중 Target, 목적지별 retention, RPO, schedule collision, preflight 결과를 추가하면 더
복잡해진다.

개선안:

- `대상 → 목적지 → 일정·RPO → 보존·검증 → preflight summary`의 guided workspace를 쓴다.
- cron은 고급 입력으로 남기고 기본 UI는 일·주기·시각·timezone을 명시하는 schedule builder를
  제공한다.
- 여러 Target의 공통 설정과 엔진별 예외를 분리해 보여준다.
- 완료 summary에 예상 다음 실행, 대상 수, 목적지 수, L3 일정, retention 결과를 표시한다.

### I-03. 오류를 삼키거나 문자열 하나로 표시해 사용자가 복구 경로를 알 수 없다

여러 `list...().catch(() => {})` 호출은 일부 데이터 로드 실패를 빈 상태처럼 보이게 한다.
`String(e)` 기반 flash는 구조화 error code, phase, retryability를 사용하지 않는다. 연결
실패, 자격증명 잠금, 목적지 권한 부족, native tool 누락이 동일한 표현 경로를 공유한다.

개선안:

- 빈 상태와 로드 실패를 명확히 분리하고 부분 실패 배너에 재시도 동작을 제공한다.
- 오류를 `무엇이 실패했는가`, `영향`, `자동 재시도 여부`, `사용자 다음 조치`, `상세 코드`로
  계층화한다.
- retry 가능한 오류는 해당 설정을 유지한 채 다시 probe할 수 있게 한다.
- 비밀이나 원시 명령행은 사용자 오류 상세와 support bundle에서 redaction한다.

### I-04. Overview의 안심 문구가 실제 운영 증거보다 앞선다

`Dashboard.tsx`는 job health만으로 `모든 백업이 안전합니다`를 표시하고 최근 백업이
서명·검증·오프사이트 보관된다고 일반화한다. 최근 실패, 마지막 성공과 실패의 분리, RPO,
L3 기한, queue 상태를 계산하지 않는다. 문서의 copy rule과 직접 충돌한다.

개선안:

- “안전” 같은 총괄 판단 대신 `보호 중 18/20`, `RPO 위반 1`, `L3 기한 초과 1`처럼 검증 가능한
  사실을 먼저 표시한다.
- 최근 성공과 최근 실패를 나란히 보이고 성공이 실패 event를 지우지 않게 한다.
- 상단은 KPI 카드 그리드보다 개입 필요 목록과 queue 상태를 우선한다.
- 모든 정상 상태에도 마지막 L3 시각과 데이터 갱신 시각을 근거로 보여준다.

### I-05. 대량 데이터와 power-user 작업을 위한 검색·필터·일괄 동작이 없다

현재 DB, 목적지, job은 div 목록으로 렌더링되며 검색, 정렬, pagination, 저장된 view,
keyboard row navigation이 없다. 100개 plan과 100,000개 Activity Event 목표에서 사용할 수
없다.

개선안:

- Inventory Page에 query와 filter chip, 정렬, pagination 또는 virtualization, row inspector를
  공통 제공한다.
- DB에는 engine, connection, protected/unprotected, drift 필터를 제공한다.
- Snapshot에는 DB, 시간, 검증 level, 목적지, restore eligibility 필터를 제공한다.
- Activity에는 상태, phase, retryability, RPO, plan, host 필터와 저장된 view를 제공한다.
- 다중 Target 보호, 계획 pause/resume, 재probe는 선택 항목과 영향 요약이 있는 일괄 동작으로
  제공한다.

### I-06. Destination 등록이 지나치게 길고 실제 probe보다 입력 완성 여부에 의존한다

`Destinations.tsx`의 writer/reader 분리는 좋은 방향이지만 S3와 SSH의 모든 필드를 한 화면에
노출한다. Object Lock과 SSH append-only capability는 form 값으로 미리 표시될 뿐 실제
권한·list/get/write/delete·host key 검증 결과가 없다.

개선안:

- 종류 선택 후 provider별 단계형 등록을 사용하고 고급 필드는 접힌 영역에 둔다.
- 경로와 개인키는 OS file picker를 제공한다.
- writer와 reader를 각각 probe하고 권한별 capability 결과를 표시한다.
- Object Lock bucket 설정, retention 충돌, reader가 writer와 동일한 신원인지 사전 점검한다.
- 실패한 probe 뒤 입력을 보존하고 해당 단계에서만 수정·재시도하게 한다.

### I-07. 위험 동작의 확인 수준이 위험도와 맞지 않는다

복원뿐 아니라 Connection 삭제, Target 제거, Plan 삭제, Destination credential 교체,
retention 단축, Object Lock 변경, queue 취소도 데이터 보호에 영향을 준다. 명세에는 일부
영향 확인이 있지만 일관된 위험 등급과 확인 패턴이 없다.

개선안:

- 동작을 일반 변경, 보호 공백 가능, 실행 중단, 파괴적 복원으로 등급화한다.
- 확인 화면은 영향을 받는 Target·Plan·실행·Snapshot 수와 되돌릴 수 있는지 표시한다.
- 삭제 대신 pause, archive, credential rotate처럼 회복 가능한 동작을 우선한다.
- 확인 문구 재입력은 overwrite와 irrecoverable credential/key 동작에 제한한다.

### I-08. 비동기 작업의 취소·재시도·화면 이탈 규칙이 정의되지 않았다

연결 probe, discovery, destination preflight, backup, verification, restore는 수 초에서
수 시간까지 걸린다. 디자인 문서는 probe 취소와 재시도를 언급하지만 화면별 상태 보존,
중복 클릭, 화면 이동, 앱 종료 시 동작이 구체화되지 않았다.

개선안:

- 짧은 요청과 Work Item을 구분하고, 장시간 작업은 Activity로 승격한다.
- 화면 이탈 시 작업은 계속되며 전역 실행 tray 또는 Activity에서 재진입할 수 있게 한다.
- 취소 요청, 취소 진행, 취소 완료를 구분하고 child process 종료 여부를 보여준다.
- 같은 probe의 중복 실행을 coalesce하고 마지막 성공 시각과 현재 실행을 함께 표시한다.

### I-09. Snapshot과 Recovery의 역할 경계가 사용자에게 모호할 수 있다

명세는 Snapshots에서 복원을 시작하고 Recovery에서 Restore Run을 본다고 정의하지만,
사용자는 두 메뉴가 모두 “복원” 기능이라고 느낄 수 있다.

개선안:

- Snapshots는 복원 후보의 탐색·검증·보존을, Recovery는 진행 중·과거 복원과 drill evidence를
  담당한다고 페이지 제목과 empty state에서 명시한다.
- Snapshot 상세의 복원 동작은 Recovery workspace를 열고 선택 문맥을 유지한다.
- `실제 복원`과 `복원 검증(L3)`을 목적, 대상, 위험 배지로 분리한다.

### I-10. 알림 설정이 UI 여정에서 빠져 있어 개입 필요 상태를 놓칠 수 있다

`Schedule.tsx`는 알림을 config의 후속 배선으로 안내한다. v2가 Overview의 예외 중심 운영을
약속하려면 사용자가 앱을 열지 않는 시간의 실패·RPO·L3 기한 초과도 전달돼야 한다.

개선안:

- Settings에 채널 등록, probe, event별 routing, quiet hours, test notification을 제공한다.
- Plan 생성 summary에서 알림 정책 적용 여부를 보여준다.
- 알림 전송 실패 자체도 Activity와 Overview의 attention 상태로 만든다.

### I-11. 공통 빈 상태가 현재 위치의 설명만 하고 다음 작업으로 연결하지 않는다

`등록된 데이터베이스가 없습니다. 오른쪽에서 추가하세요`, `등록된 잡이 없습니다` 같은
문구는 현재 구현에서는 동작하지만 v2의 여러 페이지에서는 사용자가 선행 조건을 찾아
이동해야 한다.

개선안:

- 빈 상태는 이유, 다음 동작, 필요한 선행 조건을 포함한다.
- 예: Databases 빈 상태에서 `연결 추가`, Snapshots 빈 상태에서 `보호 계획 보기`,
  Activity 빈 상태에서 `첫 수동 백업 실행`으로 직접 이동한다.
- 검색 결과 없음과 데이터 자체 없음, 로드 실패를 서로 다른 empty state로 쓴다.

### I-12. 시간·일정·RPO 표시는 timezone과 기준 시각을 명확히 해야 한다

여러 지점과 향후 agent를 고려하면 `0 3 * * *`, `2일 전`, `다음 예약`만으로는 DST, 앱
호스트 시각, DB 지점 시각, UTC event를 구분할 수 없다.

개선안:

- Plan은 IANA timezone을 명시하고 다음 세 번의 실행 시각을 preview한다.
- Activity와 evidence는 원본 UTC와 사용자의 표시 timezone을 일관되게 처리한다.
- RPO 계산 기준과 마지막 성공 Snapshot 시각을 같은 위치에 표시한다.

## Nice-to-Have Findings

### N-01. 반복 구성을 위한 Plan Template이 있으면 다중 DB 도입 시간이 줄어든다

일정, 목적지, retention, verification, notification을 template으로 저장하고 여러 Target에
적용할 수 있게 한다. 적용 후 각 Plan은 독립적으로 변경할 수 있어야 하며 template 변경의
자동 전파 여부를 명확히 한다.

### N-02. 저장된 view와 URL에 준하는 앱 내부 deep link가 운영 인계를 돕는다

Activity 실패 필터, 특정 Snapshot, Restore Run evidence를 재현 가능한 링크 또는 앱 내부
식별자로 공유하면 장애 인계와 support bundle 분석이 쉬워진다.

### N-03. 키보드 중심 운영을 위한 command palette와 단축키가 유용하다

검색 초점, 새 연결, 새 계획, Activity 열기, 선택 행 inspector 열기 등 비파괴 동작에
단축키를 제공한다. 복원과 삭제 같은 고위험 동작은 단축키만으로 실행하지 않는다.

### N-04. 복원 리허설 모드는 실제 사고 전 사용자 학습에 도움이 된다

선택한 Snapshot의 eligibility, 예상 도구, 대상, 시간, smoke query를 실행 없이 확인하는
rehearsal을 제공한다. 기존 dry-run이라는 구현 용어보다 `복원 준비 점검`처럼 결과를
설명하는 이름을 쓴다.

### N-05. 대량 운영자를 위한 밀도 조절과 열 프리셋이 유용하다

기본, 운영, 보안·감사 열 프리셋을 제공하고 고밀도 table에서도 상태 텍스트와 접근 가능한
이름을 유지한다.

## Cross-Cutting UX Requirements

### Error Recovery Contract

모든 실패 표면은 최소 다음 정보를 같은 순서로 제공해야 한다.

1. 실패한 객체와 단계
2. 현재 데이터 보호에 미치는 영향
3. 자동 재시도 여부와 다음 시각
4. 사용자가 할 수 있는 안전한 다음 동작
5. redaction된 기술 상세와 error code

### Long-running Work Contract

- 시작 요청이 접수됐는지와 실제 실행이 시작됐는지를 구분한다.
- queued 이유와 예상 시작은 추정값임을 표시한다.
- 화면을 떠나도 작업은 지속되고 전역 Activity에서 찾을 수 있다.
- 취소는 요청·진행·완료 상태를 갖고 partial artifact 처리 결과를 남긴다.
- 완료 toast는 보조 수단이며 영속 Activity와 evidence가 진실 소스다.

### Accessibility Release Gate

- 각 핵심 여정을 keyboard-only로 완료한다.
- 모든 control의 accessible name, description, error를 accessibility tree에서 검사한다.
- focus는 drawer·dialog 진입과 종료 시 예측 가능한 위치로 이동한다.
- 상태와 capability는 색, 아이콘, 위치만으로 전달하지 않는다.
- table은 header 관계, row 선택 상태, 정렬 상태를 프로그램적으로 노출한다.
- 동적 로그와 상태는 과도하게 읽히지 않도록 summary live region과 수동 상세를 분리한다.
- 200% 확대와 1024px 폭에서 내용과 동작이 잘리거나 겹치지 않는다.

## Recommended UX Delivery Order

1. 공통 shell, 객체별 IA, 접근 가능한 form·status·table primitives
2. Connection probe와 DB discovery를 포함한 first-value onboarding
3. 단계형 Backup Plan과 capability/preflight
4. persistent Activity, queue reason, 취소·재시도
5. Snapshot inventory, eligibility, catalog rebuild
6. 안전한 Recovery workspace와 evidence
7. Overview 예외 중심 재구성
8. 대량 데이터, saved view, bulk operation, 알림 설정

## Dictionary Updates

| 행위 | 용어 | 정의(제안) | 근거 |
|------|------|------------|------|
| 추가 | Protection Status(보호 상태) | Database Target이 유효한 Plan, 최근 committed Snapshot, RPO, 검증 정책을 기준으로 현재 보호되고 있는지를 나타내는 계산 상태 | Overview와 Databases에서 단순 최근 성공과 실제 보호 여부를 구분해야 함 |
| 추가 | Attention State(개입 필요 상태) | 실패, RPO 위반, 검증 기한 초과, discovery drift, expired lease처럼 운영자의 판단이나 조치가 필요한 상태의 공통 분류 | 내비게이션 배지와 예외 중심 Overview의 일관된 의미가 필요함 |
| 추가 | Probe Result(연결 점검 결과) | 접속·인증·TLS·권한·native tool·저장소 capability 점검의 구조화된 결과와 시각 | 성공/실패 문자열과 실제 capability 근거를 구분해야 함 |
| 추가 | Discovery Drift(탐색 변경) | 재탐색한 DB 목록과 기존 Database Target 사이의 추가·누락·변경 차이로, 자동 삭제 없이 검토가 필요한 상태 | 삭제된 DB의 Plan과 Snapshot을 보존하며 사용자 판단을 요청해야 함 |
| 추가 | Restore Drill(복원 훈련) | 실제 장애 복구가 아니라 정책에 따라 격리 대상에 수행하는 주기적 L3 복원 검증 | 실제 Restore Run과 검증 목적의 실행을 UI와 evidence에서 구분해야 함 |
| 정의 보강 | Restore Eligibility(복원 적격성) | 실행 가능 여부뿐 아니라 실패한 gate, 조건부 제한, 안전한 다음 조치를 포함하는 스냅샷별 계산 결과 | 복원 전에 사용자가 왜 가능·불가능한지 이해하고 복구해야 함 |
| 추가 | Plan Template(계획 템플릿) | 일정·목적지·보존·검증·알림의 반복 가능한 초기값 묶음으로, 생성된 Backup Plan과 별도 수명주기를 가짐 | 다중 DB 일괄 도입에서 설정 반복을 줄이되 계획과 실행의 독립성을 유지해야 함 |
| 정의 보강 | Activity Event(활동 이벤트) | 운영 판단용 메시지, 구조화 error code, retryability, 영향, 관련 객체 링크를 포함하는 영속 기록 | 휘발성 원시 로그와 사용자 복구 가능한 실행 이력을 구분해야 함 |
