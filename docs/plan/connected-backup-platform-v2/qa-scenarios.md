# Connected Backup Platform v2 QA Scenarios

## 1. 테스트 원칙

이 문서는 `operation-scenarios.md`의 각 메뉴·업무 흐름을 실행 가능한 검증 케이스로 바꾼다.
초기 v2는 public HTTP API가 없으므로 frontend/backend 통합은 Tauri thin wrapper와
authenticated local IPC 계약을 기준으로 한다.

- `supported` 판정은 mock만으로 얻을 수 없다. 실제 engine/tool/restore가 필요한 케이스는
  managed fixture에서 실행한다.
- Attempt는 중복될 수 있으나 Plan Occurrence와 complete Recovery Point의 멱등성은 보장한다.
- backup process exit 0만으로 성공하지 않고 Manifest, Replica, verification, restore evidence를
  해당 단계에 맞게 검사한다.
- 오류 메시지는 stable code와 운영자가 취할 수 있는 동작을 함께 검증한다.
- secret, token, key material, raw connection string은 snapshot·log·support bundle에 남지 않는다.

## 2. Fixture와 공통 환경

| Fixture ID | 구성 | 용도 |
|---|---|---|
| FX-LOCAL | Windows service + Tauri + Local Destination | 기본 UI/IPC/backup/restore |
| FX-PG | 지원 PostgreSQL server/tool + 결정적 seed와 content oracle | PG backup/L1/L2/L3/restore |
| FX-MY | MySQL과 MariaDB 분리 fixture + content oracle | 엔진별 지원 계약 |
| FX-MULTI | 동일 host 16개 Target, global=2, host=1 | admission과 queue |
| FX-REPLICA | required Local + required fault-controllable NAS | degraded/repair |
| FX-SQLSERVER | sqlpackage/BACPAC 지원·비지원 object fixture | Wave 1 후보 판정 |
| FX-SQLITE | WAL, active writer/reader, 장기 transaction DB | online backup/atomic restore |
| FX-MIGRATION | legacy TOML, keystore, Catalog, Task XML bundle | shadow import/cutover |
| FX-LOAD | Activity 100,000건, 긴 한글/영문 DB 이름 | UI 성능·접근성 |

모든 managed fixture 결과는 source revision, build ID, OS, engine/server/tool version과 hash,
fixture seed, content oracle 결과를 증적에 포함한다.

## 3. 메뉴별 테스트

### Overview: OP-M01

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M01-01 | 정상 | 성공, 최근 실패, RPO 위반, verification overdue가 각각 존재 | 네 상태를 합성하지 않고 별도 행과 관련 리소스 링크로 표시 | Playwright+IPC fixture |
| QA-M01-02 | 오류 | service를 중지한 뒤 앱 시작 | 과거 projection은 stale/read-only, service unavailable 표시, 정상 추정 없음 | Playwright |
| QA-M01-03 | 경계 | projection 갱신 시각이 freshness 임계값 초과 | 마지막 갱신 시각과 stale badge, 새 작업 동작 비활성 | component+clock |
| QA-M01-04 | 접근성 | keyboard-only와 200% zoom으로 Attention 이동 | focus 순서·이름·상태가 접근성 tree에 있고 선택 후 inspector로 이동 | Playwright+a11y |

### Connections: OP-M02

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M02-01 | 정상 | FX-PG endpoint와 credential 등록 후 probe/discover | Secret Ref만 응답, server identity와 DB 목록, 선택 가능한 Target 표시 | IPC integration |
| QA-M02-02 | 오류 | 잘못된 비밀번호로 probe | `CONNECTION_AUTH_FAILED`, Connection draft 보존, 응답/Activity에 secret 없음 | managed fixture |
| QA-M02-03 | 경계 | 같은 source resource identity를 다른 표시명으로 등록 | 기존 Connection 영향과 병합/별도 선택, 자동 덮어쓰기 없음 | integration |
| QA-M02-04 | 장애 | discovery 중 한 DB 권한 오류와 다른 DB 성공 | partial observation과 누락 원인 보존, 성공 Target 자동 삭제 없음 | managed fixture |
| QA-M02-05 | 접근성 | light/dark, select option, validation error, 취소/재시도 | label/name/description/error 연결, 테마 대비, focus 복귀 통과 | Playwright+a11y |

### Databases: OP-M03

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M03-01 | 정상 | unprotected Target에서 “보호 계획 만들기” | Plan wizard에 정확한 Target이 미리 선택되고 capability 표시 | Playwright |
| QA-M03-02 | 오류 | 필요한 backup capability가 `failed`인 Target 선택 | 활성화 차단, 필요한 tool/version과 limitation 표시 | integration |
| QA-M03-03 | 경계 | discovery freshness 초과이나 마지막 backup은 성공 | discovery stale과 protection history를 별도 상태로 표시 | component |
| QA-M03-04 | 장애 | 보호 중 Target이 다음 discovery에서 사라짐 | Target/Run 이력 보존, Discovery Drift Attention, 자동 retire 없음 | integration |

### Destinations: OP-M04

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M04-01 | 정상 | writer로 임시 객체 쓰고 reader로 hash 검증 | Destination `ready`, 역할별 probe evidence 저장 | adapter integration |
| QA-M04-02 | 오류 | writer 성공, reader credential 실패 | backup plan 활성화 차단, write 성공을 restore eligibility로 오인하지 않음 | managed fixture |
| QA-M04-03 | 경계 | free space가 예상 spool+artifact보다 작음 | preflight 실패와 필요/가용 byte 표시, native dump 시작 0회 | integration |
| QA-M04-04 | 오류 | published Plan이 참조하는 Destination archive | impact 응답 후 replacement 없이 archive 거부 | repository test |
| QA-M04-05 | 보안 | writer/reader/delete secret이 서로 다름 | 각 operation은 지정 role만 resolve, UI/DB/log에는 Secret Ref만 존재 | security integration |

### Backup Plans: OP-M05

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M05-01 | 정상 | Target, required Destination, schedule/RPO/retention/verification 설정 | preflight 통과 후 revision 1 publish, next due 표시 | UI+repository |
| QA-M05-02 | 오류 | best-effort Destination만 선택해 publish | DB trigger와 application 모두 publish 거부 | schema test |
| QA-M05-03 | 오류 | engine이 지원하지 않는 옵션 선택 | `ENGINE_CAPABILITY_MISMATCH`, draft 보존 | application test |
| QA-M05-04 | 경계 | published revision의 membership 직접 변경 | immutable trigger 거부, revise는 새 revision 생성 | schema+application |
| QA-M05-05 | 경계 | run-now를 같은 idempotency key로 연속 요청 | Plan Occurrence/Work Item 재사용, 중복 Recovery Point 없음 | IPC integration |
| QA-M05-06 | 경계 | DST 중복/누락 시각과 절전 복귀 | UTC ledger, bounded latest-one, missed reason이 결정적으로 기록 | scheduler property test |

### Activity: OP-M06

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M06-01 | 정상 | host permit 때문에 queued인 Work Item 조회 | 정확한 wait reason, active permit, next eligible time 표시 | integration |
| QA-M06-02 | 정상 | native process가 running이며 heartbeat 갱신 | 단계 timeline과 freshness 갱신, UI polling/subscription이 작업에 영향 없음 | integration |
| QA-M06-03 | 경계 | claim 전 cancel | native process 시작 0회, `cancelled`, permit/spool allocation 0 | runtime test |
| QA-M06-04 | 장애 | dump가 자식 process를 만든 상태에서 cancel | Windows Job Object로 tree 종료, stderr drain, staging 격리 | Windows managed |
| QA-M06-05 | 장애 | heartbeat 만료 후 이전 worker가 commit 시도 | stale fence update 0, Reconciliation, 새 Attempt만 terminal 전이 | fault injection |
| QA-M06-06 | 성능 | FX-LOAD 100,000 event 탐색·필터·뒤로가기 | keyset pagination, bounded DOM, 필터/cursor/selection 복원 | Playwright perf |

### Snapshots: OP-M07

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M07-01 | 정상 | complete/protected/L3 fresh Recovery Point 조회 | Part, Replica, signed Manifest, 네 상태 축과 evidence 표시 | integration |
| QA-M07-02 | 경계 | incomplete와 Orphan Artifact 존재 | 기본 목록 제외, 진단 필터에서만 원인과 cleanup 상태 표시 | repository+UI |
| QA-M07-03 | 오류 | complete이나 required NAS Replica missing | `degraded + non_compliant + eligible/conditional`, “protected” 금지 | projection test |
| QA-M07-04 | 보안 | Manifest signature invalid 또는 unknown key | restore blocked, quarantine, Attention과 ingest 진단 | security integration |
| QA-M07-05 | 정상 | Local 적격 Replica에서 NAS repair | 기존 encoded object 재사용, 새 Replica commit 뒤 compliance 재계산 | adapter integration |
| QA-M07-06 | 장애 | Catalog projection 삭제 후 Destination inventory rebuild | control tables 불변, 동일 Recovery Point ID/상태로 재구축 | managed fixture |

### Recovery: OP-M08

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M08-01 | 정상 | 적격 Snapshot을 새 DB로 복원 | preflight, Work Item, engine restore, content oracle, Restore Run 성공 | managed restore |
| QA-M08-02 | 오류 | complete Snapshot이나 적격 reader Replica 없음 | restore.prepare 차단과 repair 동작, native restore 0회 | application test |
| QA-M08-03 | 보안 | overwrite 대상 identity 확인과 유효 token | 한 요청만 실행, token consumed, 감사 Activity에 digest/ref만 기록 | security integration |
| QA-M08-04 | 보안 | 다른 SID/target/payload, 만료 또는 재사용 token | 안정된 `RESTORE_CONFIRMATION_`/`IPC_` 오류, 상태 변경 0 | security integration |
| QA-M08-05 | 오류 | 공간·도구·권한·버전 중 하나가 preflight 실패 | 어떤 단계가 실패했는지 표시, restore process 시작 0회 | table-driven test |
| QA-M08-06 | 오류 | native restore exit 0 후 content oracle 실패 | Restore Run `failed` 또는 `cleanup_required`, 성공 badge 없음 | managed restore |
| QA-M08-07 | 장애 | restore 중 cancel/서비스 중단 | target 자동 삭제 없음, cleanup evidence와 안전한 재개/정리 안내 | fault injection |

### Settings: OP-M09

| ID | 유형 | 조건·행동 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-M09-01 | 정상 | global/host/CPU/destination/restore permit 변경 | expected revision 검증, 새 admission부터 적용, running allocation 유지 | integration |
| QA-M09-02 | 오류 | capacity 0, 음수, 안전 상한 초과 | field error와 저장 거부, 기존 설정 불변 | application test |
| QA-M09-03 | 오류 | notification test delivery 실패 | backup 결과 불변, notification failure Activity/Attention 생성 | integration |
| QA-M09-04 | 정상 | live queue/heartbeat 중 control backup | SQLite online backup, integrity/FK/schema 검사, 작업 중단 없음 | rusqlite integration |
| QA-M09-05 | 장애 | backup API 중 disk full 또는 write failure | control backup `failed`, 성공 record 없음, production DB integrity 유지 | fault injection |
| QA-M09-06 | 보안 | cutover marker 이후 legacy binary 실행 | 시작 전 `MIGRATION_OLD_BINARY_GUARD`, 새 작업/secret access 0 | Windows integration |

## 4. End-to-End 업무 테스트

| ID | 운영 시나리오 | 주입 조건 | 기대 결과 | 자동화 |
|---|---|---|---|---|
| QA-E01 | OP-E01 첫 보호 백업 | FX-PG+FX-LOCAL | 5분 목표 내 complete/protected Snapshot과 실제 evidence | Playwright+managed |
| QA-E02 | OP-E02 다중 DB due | FX-MULTI, duplicate tick 포함 | global≤2, host≤1, occurrence/Recovery Point 중복 0 | runtime benchmark |
| QA-E03 | OP-E03 required Replica 실패/repair | NAS 단절 후 복구 | degraded 검색 가능, non-compliant, repair 뒤 protected | managed storage |
| QA-E04 | OP-E04 crash Reconciliation | dump/upload/commit 각 단계 service kill | stale commit 0, permit 누수 0, terminal/queued 수렴 | fault matrix |
| QA-E05 | OP-E05 새 DB restore | FX-PG Snapshot | content oracle 통과한 Restore Run과 L3 evidence | managed restore |
| QA-E06 | OP-E06 overwrite restore | token replay·identity mismatch 포함 | 일치한 단일 요청만 수행, 모든 거부 감사 | security E2E |
| QA-E07 | OP-E07 Discovery Drift | rename/drop/new DB | history 보존, 승인 전 plan mutation 0 | managed discovery |
| QA-E08 | OP-E08 RPO+notification 실패 | clock advance+channel failure | 두 Attention 독립 생성·해소 | clock+integration |
| QA-E09 | OP-E09 safe cutover | 각 migration step power loss | dual-run 0, marker 경계별 rollback/forward-fix 정확 | Windows fault matrix |
| QA-E10 | OP-E10 control backup drill | WAL writer/readers active | online backup 복원·integrity·migration read 통과 | rusqlite managed |
| QA-E11 | OP-E11 SQL Server BACPAC | supported/unsupported object fixtures | 통과 범위만 candidate/supported, DACPAC data claim 0 | managed SQL Server |
| QA-E12 | OP-E12 SQLite online backup | WAL/lock/long transaction | 일관된 복원, overwrite 실패 시 원본 보존 | managed SQLite |
| QA-E13 | OP-E13 retention delete | hold/pinned/last compliant/partial delete | dry-run 일치, 보호 대상 보존, tombstone과 retry 감사 | storage fault test |
| QA-E14 | OP-E14 unauthorized IPC | 비승인 SID/process | payload operation 0, `IPC_UNAUTHORIZED_SID`, secret 노출 0 | Windows security |

## 5. Frontend–Service IPC 계약 테스트

| ID | 유형 | 요청 | 기대 결과 |
|---|---|---|---|
| QA-I01 | 오류 | unknown `protocolVersion` | payload 처리 전 `IPC_UNSUPPORTED_VERSION` |
| QA-I02 | 오류 | `issuedAtMs`/`expiresAtMs` 만료 | `IPC_EXPIRED_REQUEST`, Work Item 0 |
| QA-I03 | 보안 | 최대 body보다 큰 envelope | decode/dispatch 전 `IPC_BODY_TOO_LARGE`, bounded memory |
| QA-I04 | 오류 | 알 수 없는 command와 필드 | stable `IPC_UNKNOWN_COMMAND`/validation code, Rust debug 노출 없음 |
| QA-I05 | 보안 | DACL 우회 또는 승인되지 않은 SID | pipe/authorization 거부, resource existence 과다 노출 없음 |
| QA-I06 | 경계 | 같은 SID·command·idempotency key·payload 재전송 | 기존 result/Work Item을 `deduplicated=true`로 반환 |
| QA-I07 | 보안 | 같은 idempotency key에 다른 payload | `IPC_IDEMPOTENCY_CONFLICT`, 첫 요청 결과 불변 |
| QA-I08 | 보안 | consumed confirmation token 재전송 | CAS update 0, `IPC_REPLAYED_REQUEST` 또는 action별 replay code |
| QA-I09 | 정상 | 모든 long-running command | IPC timeout 전에 Work Item ID 반환, 실행은 service가 소유 |
| QA-I10 | 장애 | Activity subscription 끊김 뒤 last sequence로 재연결 | 순서·중복 처리 규칙대로 누락 없이 재개, 작업은 취소되지 않음 |

## 6. 비기능·장애·보안 테스트

| ID | 유형 | 검증 | 통과 기준 |
|---|---|---|---|
| QA-N01 | 성능 | FX-MULTI 16개 due admission | permit 초과 0, scheduler tick이 heartbeat writer를 굶기지 않음 |
| QA-N02 | 장애 | leased/starting/running/committing 단계별 service kill | 각 단계가 정해진 Reconciliation 상태로 수렴 |
| QA-N03 | 장애 | spool reserve 전·후 disk full | reserve 전 native tool 0회, 이후 partial 격리와 permit 반환 |
| QA-N04 | 장애 | Destination timeout/재연결/중복 응답 | bounded retry, same object/Manifest idempotency, false protected 0 |
| QA-N05 | 장애 | child/grandchild process와 PID 재사용 | Job Object+start time+boot ID로 잘못된 process kill 0 |
| QA-N06 | 보안 | password/token/key/connection string canary 주입 | UI, Activity, evidence, support bundle 검색 결과 0 |
| QA-N07 | 보안 | unsigned/변조/native tool path swap | 실행 전 차단, Engine Certification 승격 0 |
| QA-N08 | 성능 | Activity 100,000건 cursor/filter/inspect | 정의된 UI latency budget과 bounded DOM, SQLite full scan 회귀 없음 |
| QA-N09 | 성능 | 1GB 동일-host PG/MySQL과 CPU encode 경합 | global/host/CPU permit 준수, heartbeat freshness budget 통과 |
| QA-N10 | 접근성 | 1024/1280/1600px, 200% zoom, keyboard-only 5개 journey | WCAG AA 핵심 대비, focus trap/return, 표 header/sort/selection 통과 |
| QA-N11 | 접근성 | light/dark의 container/text/select option/placeholder/autofill | 하드코딩 배경·검은 글자 잔존·option 대비 오류 0 |
| QA-N12 | 시각 | Pretendard/Space Grotesk/JetBrains Mono 실제 face load와 reduced motion | 필요한 weight만 loaded, 한글 system fallback 없음, motion 비활성 존중 |

## 7. Release Gate 매핑

| Gate | 필수 QA |
|---|---|
| G0 Truth | QA-M01-01~03, QA-M07-03, QA-I04, QA-N06~07 및 section-01 claims tests |
| G0-B Store | QA-M05-02, QA-M05-04~06, QA-M09-04~05, schema migration/invariant tests |
| G1 Durable Core | QA-M06-01~05, QA-E02~04, QA-N01~05 |
| G2 Existing Engines | QA-M02-01~02, QA-M04-01~03, QA-M08-01/05/06, QA-E01/03/05 |
| G3 Product UI | QA-M01~M09 전체, QA-N08~12 |
| G4 New Engine Wave 1 | QA-E11, QA-E12 |
| Launch | QA-E06~10, QA-E13~14, QA-I01~10, 설치·upgrade·rollback matrix |

어느 gate도 수동 육안 확인 하나로 통과시키지 않는다. managed test가 필요한 항목은 같은 build의
기계 판독 가능한 evidence를 첨부한다.

## 8. 자동화 우선순위

1. SQLite schema/trigger/CAS와 domain state machine 단위·property test
2. local IPC envelope/SID/idempotency/confirmation contract test
3. fake Process Supervisor와 fault-controllable Destination을 사용한 deterministic runtime test
4. PostgreSQL/MySQL/MariaDB 실제 backup/restore fixture
5. Playwright 9개 메뉴와 keyboard-only 핵심 journey
6. Windows Job Object, Task Scheduler cutover, service upgrade fault matrix
7. SQL Server/SQLite Wave 1 managed fixture

## 9. 케이스 요약

| 범주 | 케이스 수 |
|---|---:|
| 메뉴별 정상·오류·경계 | 49 |
| End-to-End 업무 | 14 |
| Frontend–Service IPC | 10 |
| 비기능·장애·보안 | 12 |
| **합계** | **85** |

85개 케이스는 section 파일의 더 작은 table-driven/unit case를 대체하지 않는다. 이 문서는
제품 수준 acceptance와 회귀 범위를 고정하며, 각 구현 section은 담당 케이스를 더 세분화한다.
