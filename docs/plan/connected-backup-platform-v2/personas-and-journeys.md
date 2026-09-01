# Personas and Journeys

> 작성일: 2026-07-26
> 범위: Connected Backup Platform v2의 1차 사용자와 향후 확장 사용자

## Persona 1: 혼합 DB 운영자

### Context

- Windows 관리 PC에서 PostgreSQL, MySQL, MariaDB 여러 인스턴스를 관리한다.
- 전담 백업 제품 운영자는 아니며 장애 알림과 정기 확인에 쓸 시간이 적다.
- 가장 큰 불안은 “백업이 성공했다고 나오지만 실제로 복원되는가”다.

### Jobs to Be Done

- 서버 연결을 한 번 등록하고 DB를 여러 개 선택한다.
- 모든 DB에 같은 야간 일정과 저장 목적지를 빠르게 적용한다.
- 오늘 개입해야 할 실패만 찾는다.
- 특정 시점 Snapshot을 새 DB 이름으로 복원한다.

### Primary Journey

1. Connection Profile을 만들고 probe 결과를 확인한다.
2. 발견된 DB 중 보호할 대상을 선택한다.
3. 기존 Destination과 일정 템플릿으로 Backup Plan을 만든다.
4. 첫 수동 실행에서 Snapshot과 L1/L2 결과를 확인한다.
5. Overview에서 실패, RPO 위반, L3 기한만 매일 확인한다.

### Failure Moment

연결 실패가 하나의 일반 오류로만 보이거나, 여러 plan이 동시에 실행돼 운영 DB가 느려지면
제품을 신뢰하지 않는다.

## Persona 2: 소규모 팀 인프라 관리자

### Context

- 이종 DB와 여러 저장소를 관리한다.
- SQL Server와 SQLite까지 하나의 도구로 묶고 싶다.
- 감사·보안 요구 때문에 불변성, 암호화, 복원 증적이 필요하다.

### Jobs to Be Done

- 엔진별 실제 지원 범위와 제한을 사전에 판단한다.
- writer와 reader 자격증명을 분리한다.
- 보존 정책과 object lock이 맞지 않는 설정을 실행 전에 차단한다.
- 월별 restore drill 증적을 내보낸다.

### Primary Journey

1. Connections와 Destinations의 capability matrix를 점검한다.
2. DB 중요도에 따라 검증 정책과 RPO를 설정한다.
3. queue가 호스트별 동시성을 제한하는지 진단 화면에서 확인한다.
4. 정기 L3 결과와 실패 event를 검토하고 support bundle을 만든다.

### Failure Moment

UI가 지원하지 않는 기능을 선택 가능하게 보이거나, 최근 성공이 지난 실패를 가리면 운영
판단을 맡길 수 없다.

## Persona 3: 장애 복구 담당자

### Context

- 평소 plan을 만들지 않지만 장애 때 즉시 투입된다.
- 원본 catalog가 손실됐거나 로컬 PC를 교체한 상황도 다룬다.
- 잘못된 대상에 복원하는 것이 복원 실패보다 더 위험할 수 있다.

### Jobs to Be Done

- 목적지에서 Snapshot inventory를 재구축한다.
- 서명과 검증 수준을 기준으로 복원 후보를 좁힌다.
- 기존 DB를 덮어쓰지 않고 격리된 이름으로 먼저 복원한다.
- restore 과정과 결과 evidence를 남긴다.

### Primary Journey

1. reader 자격증명과 key material로 catalog를 rebuild한다.
2. DB, 시각, 검증 level로 Snapshot을 검색한다.
3. Restore Eligibility gate와 제한을 확인한다.
4. 새 대상에 복원하고 smoke query 결과를 확인한다.
5. Restore Run evidence를 사건 기록에 첨부한다.

### Failure Moment

Snapshot id를 수동 입력해야 하거나, 원본 writer 자격증명이 없다는 이유로 복원을 못 하면
재해 복구 제품으로 실패한 것이다.

## Persona 4: 원격 지점 플랫폼 운영자

### Context

- 향후 단계의 사용자다.
- 여러 망 또는 지점에 DB가 있고 중앙에서 상태를 보고 싶다.
- DB 자격증명과 평문 dump가 중앙 제어면을 통과하는 것을 원하지 않는다.

### Jobs to Be Done

- 지점 agent를 등록하고 capability와 버전을 확인한다.
- 중앙에서 plan을 배포하되 실행과 암호화는 DB 인접 위치에서 수행한다.
- agent offline, lease expiry, 중복 실행을 판단한다.

### Journey Boundary

v2 첫 릴리스에서는 구현하지 않는다. 다만 동일 Work Item과 Engine Adapter 계약, 원격에서
암호화·서명 후 전송한다는 trust boundary를 설계에 보존한다.

## Cross-persona Journey Map

| 단계 | 운영자 | 관리자 | 복구 담당자 | 핵심 증거 |
|------|--------|--------|-------------|-----------|
| 연결 | 등록·probe | capability 검토 | reader 연결 | 버전·권한·tool |
| 계획 | 대상·일정 선택 | 보존·검증 정책 | 해당 없음 | preflight 결과 |
| 실행 | 현재 상태 확인 | permit·RPO 확인 | 해당 없음 | Activity Event |
| 스냅샷 | 최근 성공 확인 | L1/L2/L3 검토 | 복원 후보 검색 | 서명 Manifest |
| 복원 | 필요 시 새 DB | drill 운영 | 실제 사고 복구 | Restore Run evidence |

## Product Implications

- Overview는 평균 성공률보다 예외와 근거를 우선한다.
- Connection과 Database Target은 반드시 분리한다.
- Snapshot에서 바로 Restore Run을 시작한다.
- concurrency 이유를 `대기 중` 한 단어가 아니라 permit별로 설명한다.
- 보안은 별도 전문가 화면이 아니라 각 journey의 gate와 evidence로 노출한다.
