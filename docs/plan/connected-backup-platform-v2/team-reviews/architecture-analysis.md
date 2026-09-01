# Architecture Review — Connected Backup Platform v2

> 검토일: 2026-07-26
> 범위: 아키텍처 적합성, 결합도와 마이그레이션 경계, 영속 큐와 디스패처,
> 확장성·성능, 보안, 데이터 소유권, 프로세스 수명주기, 테스트와 기술 부채

## 종합 판정

`desktop-first modular strangler` 방향은 현재 코드베이스에 적합하다. 기존 Rust 코어에는
`BackupSource::plan/spawn`, `Destination` capability, spool-once fan-out, 서명 매니페스트,
catalog rebuild, L1/L2/L3 검증처럼 재사용 가치가 높은 실행 경계가 이미 있다. Tauri와 CLI를
버리고 Databasement를 포크하는 것보다, 이 코어 앞에 application service와 영속
dispatcher를 놓고 연결·대상·계획·스냅샷 중심으로 제어 모델을 교체하는 편이 회귀 위험과
장기 유지비가 낮다.

다만 현재 초안은 제품 경계를 올바르게 잡았을 뿐, 실행 소유권과 영속 상태의 정확한 의미가
아직 충분히 결정되지 않았다. 특히 다음 다섯 항목은 구현 전에 ADR과 스키마로 고정해야 한다.

1. Tauri가 닫힌 뒤에도 살아 있는 dispatcher의 실제 호스트 프로세스
2. 운영 SQLite, 서명 Manifest, secret store가 각각 소유하는 진실의 범위
3. lease claim, fencing, 재시도의 at-least-once 실행 규칙
4. 다중 Target·다중 Destination에서 Plan occurrence, Run, Snapshot, Replica의 cardinality
5. Windows 자식 프로세스 트리의 취소·크래시·재수거 수명주기

이 항목을 확정하지 않고 UI와 엔진부터 늘리면 기존의 “job별 독립 프로세스” 문제를 새 이름
아래에서 반복할 가능성이 높다. 반대로 이 다섯 경계를 먼저 확정하면 Rust/Tokio 자체는
현재 목표 규모를 충분히 처리할 수 있다.

## 현재 구조와 목표 구조의 적합성

### 유지할 경계

| 현재 경계 | 판정 | v2 적용 방식 |
|---|---|---|
| `BackupSource::plan/spawn` | 유지 | 기존 3개 엔진을 새 `Engine Adapter`의 backup 하위 경계로 감싼다. |
| `Destination` capability와 commit/abort | 유지 | 정적 adapter registry를 사용하고 capability를 영속 프로필에 투영한다. |
| spool-once fan-out | 유지하되 분해 | dump/encode 결과와 목적지별 replica upload를 별도 상태로 기록한다. |
| 서명 Manifest | 유지 | Artifact와 복원 판단의 진실 소스로 유지한다. |
| SQLite catalog writer | 교체 확장 | rebuildable catalog 전용 actor에서 operational repository/queue 소유자로 확장한다. |
| Tauri sidecar 이벤트 | 호환 계층 | v2 Activity read model이 준비될 때까지 UI 진행 표시 adapter로만 사용한다. |
| Windows Task Scheduler | 역할 축소 | 개별 Plan 실행기가 아니라 dispatcher/service의 생존 또는 wake-up만 담당한다. |

### 끊어야 할 결합

- 현재 TOML `Database`는 서버 접속, 자격증명, 개별 DB 선택, 엔진 옵션을 한 구조에 담는다.
  `ConnectionProfile`과 `DatabaseTarget`으로 나누되 기존 구조를 즉시 삭제하지 않고
  versioned importer가 새 모델로 변환하게 해야 한다.
- 현재 `Job`은 선언, 스케줄, 단일 source, 목적지 집합을 한 단위로 묶고 실행 결과는
  마지막 part의 backup ID를 대표값으로 사용한다. 새 모델에서 `BackupPlan`,
  `PlanOccurrence`, `BackupRun`, `Snapshot`을 분리해야 한다.
- 현재 `execute_job`은 config lookup, secret 해제, adapter 조립, 정책 판정, dump, fan-out,
  catalog 기록을 한 함수에 결합한다. 이를 그대로 dispatcher worker로 감싸면 재시작
  checkpoint와 단계별 retry를 만들기 어렵다.
- 현재 카탈로그 writer는 프로세스당 하나다. 여러 CLI 프로세스가 같은 DB를 열 수 있으므로
  “프로세스 전체에서 단 하나”라는 주석은 시스템 전체 단일 writer를 보장하지 않는다.
  v2에서는 dispatcher 프로세스만 operational write 권한을 가져야 한다.
- 현재 Tauri는 장기 작업을 sidecar로 실행하고 stdout/stderr를 UI 이벤트로 전달한다.
  v2에서 UI 이벤트가 실행 상태의 근거가 되면 UI 재시작 때 상태가 유실된다. UI는
  dispatcher가 저장한 Activity와 Run 상태를 구독하거나 polling해야 한다.

## Critical Findings

### C1. Dispatcher의 실제 생존 프로세스가 정해지지 않았다

초안은 “단일 중앙 dispatcher”와 “Windows Scheduler가 dispatcher를 깨운다”고 정의하지만,
dispatcher가 Tauri 프로세스 내부인지, 별도 Windows service인지, 일정 시간 queue를
drain하고 종료하는 CLI인지 정하지 않았다. Tauri 내부라면 앱을 닫은 동안 예약 백업이
실행되지 않는다. Task Scheduler가 매번 별도 dispatcher를 띄우면 singleton 보장과 전역
permit이 다시 프로세스별로 갈라진다.

필수 결정:

- v2의 권장 정본은 별도 `dbbackup-service` 프로세스다.
- Tauri와 CLI는 application command를 operational DB 또는 IPC를 통해 service에 전달한다.
- service는 OS 단일 인스턴스 lock과 DB owner identity를 모두 확보한 뒤에만 claim한다.
- Task Scheduler는 service 자동 시작·복구의 보조 수단으로만 사용한다. Windows Service
  채택이 지연되면 `dispatcher --drain-until-idle`을 허용할 수 있지만, 같은 singleton과
  lease 규칙을 공유해야 한다.
- UI 종료, 로그아웃, 화면 잠금과 무관하게 최소권한 서비스 계정으로 계속 실행돼야 한다.

이 결정은 queue, DPAPI, IPC, 업데이트, 로그 위치를 모두 좌우하므로 첫 아키텍처 ADR이어야
한다.

### C2. 데이터 진실 소스의 범위가 문서마다 충돌한다

`spec.md`는 “Manifest가 백업 진실 소스이고 운영 SQLite는 제어 상태와 검색 인덱스”라고
하지만 `interview.md`는 “SQLite 진실 소스, TOML export/import”라고 적는다. 둘은
서로 다른 bounded context에서는 모두 맞지만 현재 표현은 catalog rebuild가 계획·연결·큐도
복구할 수 있는 것처럼 오해될 수 있다.

다음과 같이 소유권을 분리해야 한다.

| 데이터 | 권위 소스 | 복구 방식 |
|---|---|---|
| Connection, Target, Destination metadata, Plan, schedule, policy | Operational SQLite | versioned export/import와 별도 control-plane backup |
| 암호화된 credential과 key wrapping metadata | Secret store/keystore | 비밀번호·복구 코드·DPAPI 정책에 따른 복구 |
| Work Item, lease, attempt, Run, Activity | Operational SQLite | crash recovery와 보존 정책. Manifest로 재구축 불가 |
| Artifact identity, checksum, engine/tool metadata, destination object | 서명 Manifest | 목적지 scan과 서명 검증으로 rebuild |
| Snapshot 검색 projection, replica 상태, verification projection | SQLite projection | Manifest와 verification evidence로 reconcile |

운영 DB와 rebuildable catalog를 물리적으로 한 SQLite 파일에 둘 수는 있지만, schema와
migration, 삭제·rebuild 명령은 명확히 분리해야 한다. `catalog rebuild`가 Plan, queue,
connection을 지우거나 덮어쓰지 못하도록 해야 한다. 운영 DB 자체가 유실되면 백업 artifact는
찾고 복원할 수 있어도 원래 일정과 연결 구성은 돌아오지 않는다는 사실을 제품에 명시해야 한다.

### C3. Lease만으로 중복 실행과 오래된 worker의 commit을 막을 수 없다

현재 요구사항은 heartbeat 중단 후 재개를 말하지만, lease가 만료된 뒤 이전 worker가 늦게
살아나 결과를 commit하는 경우를 막는 fencing 규칙이 없다. 백업은 일반적인 exactly-once
실행을 보장할 수 없으며, at-least-once 실행과 idempotent commit을 명시해야 한다.

필수 queue 규칙:

- claim은 `queued/delayed` 상태, due time, attempt 상한을 조건으로 한 단일 SQLite
  transaction에서 수행한다.
- claim마다 증가하는 `lease_generation` 또는 임의 `fencing_token`을 발급한다.
- heartbeat, phase transition, result commit은 `work_item_id + fencing_token`이 현재 값과
  일치할 때만 성공한다.
- lease를 잃은 worker는 새 artifact commit과 Snapshot projection 갱신을 거부당해야 한다.
- scheduled occurrence의 idempotency key는 `plan_id + plan_revision + scheduled_for`처럼
  결정적으로 만든다.
- artifact object key와 Manifest identity도 동일 occurrence/run identity에서 파생해
  재시도 시 중복을 탐지한다.
- lease expiry 후에는 signed committed Manifest 존재 여부를 먼저 reconcile한 다음
  requeue 또는 성공 처리한다.

SQLite transaction만으로 DB 밖의 S3/SFTP commit을 원자화할 수 없으므로, “데이터 object
업로드 완료 → 서명 Manifest 최종 publish → SQLite projection” 순서와 stale worker fencing을
합쳐야 한다.

### C4. 다중 Target·다중 Destination의 실행·Snapshot cardinality가 모호하다

`BackupPlan`은 Target과 Destination을 각각 하나 이상 허용하지만 도메인 다이어그램은
Plan→Backup Run→Snapshot을 단일 관계처럼 보인다. 현재 코드도 PostgreSQL globals와 DB
part를 여러 backup ID로 만들면서 마지막 part ID만 Job 결과 대표값으로 사용한다. 이 상태로
다중 DB를 추가하면 부분 성공, 재시도, RPO, 검증, 복원 대상이 모호해진다.

권장 모델:

```text
BackupPlan(revision)
  -> PlanOccurrence(scheduled_for, idempotency_key)
     -> BackupRun(one DatabaseTarget)
        -> DumpArtifact(one logical backup payload)
           -> Snapshot(committed logical recovery point)
              -> ArtifactReplica(one Destination)
              -> VerificationRecord
```

- 하나의 Plan occurrence가 여러 Target Run을 만든다.
- Snapshot은 하나의 DB에 대한 하나의 복구 시점을 나타낸다.
- 다중 목적지는 Snapshot의 replica다. 목적지별 성공·실패·보존·reader 적격성을 별도로 둔다.
- Plan에는 최소 성공 replica 수 또는 필수 destination 집합을 정의한다. 아무 정책 없이
  “하나만 성공하면 성공”으로 두면 사용자가 기대한 지리적 중복을 조용히 위반한다.
- `BackupRun succeeded`, `Snapshot committed`, `PlanOccurrence partially_succeeded`를
  구분한다.
- PostgreSQL globals는 DB Snapshot의 독립 Snapshot이 아니라 동일 occurrence에서 생성된
  `ClusterArtifact` 또는 명시적 paired artifact로 연결하고, 복원에 필요한 쌍이 모두
  적격한지 판정한다.

이 모델이 없으면 Activity, RPO, 복원 선택, fan-out retry가 모두 서로 다른 성공 정의를
사용하게 된다.

### C5. 직접 자식 종료만으로 Windows 프로세스 트리 수명주기를 보장할 수 없다

PostgreSQL과 MySQL dump는 `kill_on_drop(true)`와 stderr drain을 사용하고 있어 현재
기본기는 좋다. 그러나 dispatcher crash, 강제 종료, dump 도구가 다시 자식을 만드는 경우,
PID 재사용, 서비스 업데이트에서는 직접 child handle만으로 orphan tree 정리를 보장할 수
없다.

필수 supervisor 경계:

- Windows Job Object에 실행별 프로세스 트리를 넣고 `KILL_ON_JOB_CLOSE`를 사용한다.
- Work Item에 OS process identity를 단순 PID가 아닌 run token, 시작 시각, tool hash와 함께
  기록한다.
- graceful cancel, timeout, hard kill, `wait`, pipe drain, 임시 credential 삭제, spool 격리의
  순서를 공통 `ProcessSupervisor`가 소유한다.
- 서비스 재시작 시 DB의 running process metadata와 실제 Job Object 생존 여부를
  reconcile한다. 존재 확인 없이 PID만 kill하지 않는다.
- shutdown에는 새 claim 중단, 진행 작업 grace period, heartbeat 종료, hard kill 여부
  기록이 필요하다.

이 항목은 중복 dump와 운영 DB 부하 폭주를 막는 핵심이므로 engine adapter 개별 구현에
맡기면 안 된다.

## Important Findings

### I1. Queue 상태 머신이 문서 사이에서 일치하지 않는다

`spec.md`는 `queued, leased, running, succeeded, failed, cancelled, expired`를 요구하지만,
`research/web.md`는 `failed_retryable, delayed, failed_terminal, canceled, lease_expired`를
사용한다. spelling과 의미를 하나로 고정해야 한다.

권장 terminal 상태는 `succeeded`, `partially_succeeded`, `failed_terminal`, `cancelled`이고,
재시도 대기는 `delayed`, 실행권 상실은 상태가 아니라 `lease_expired` Activity Event로
기록한 뒤 `queued` 또는 `failed_terminal`로 전이한다. `expired`를 terminal로 두면 “lease가
만료됨”과 “보존기한이 끝남”이 충돌한다. 모든 전이는 이전 상태와 fencing token을 조건으로
한 compare-and-set이어야 한다.

### I2. Resource Permit의 획득 순서와 대기 정책이 없다

작업 task가 global permit을 잡은 채 host permit을 기다리면 다른 host의 실행을 막는
head-of-line blocking이 발생한다. 여러 resource key를 각자 await하면 순서 차이로 교착이나
불공정이 생길 수 있다.

- dispatcher가 현재 가용 자원에 맞는 runnable item을 선택하고 필요한 permit 집합을
  원자적으로 예약해야 한다.
- 부분 permit을 잡은 채 다른 permit을 기다리지 않는다.
- key 정렬 순서를 고정하되, 우선순위 aging과 lane quota를 함께 적용한다.
- host permit은 dump까지만, CPU permit은 encode까지만, destination permit은 해당 replica
  upload/검증까지만 보유한다.
- restore는 `database:{connection}:{database}` exclusive permit을 사용하고 같은 DB backup과
  충돌 정책을 명시한다.

### I3. Backup Run 하나를 재시도 단위로 두면 목적지 장애 때 dump를 반복한다

느린 목적지 하나 때문에 전체 Run이 끝나지 않거나, 일부 replica 실패 후 재시도할 때 이미
성공한 dump와 upload까지 반복하면 DB 부하와 spool 사용량이 커진다.

최소 phase checkpoint를 `preflight`, `dumping`, `encoded`, `replicating`, `verifying`,
`committing`으로 둔다. encoded artifact가 온전하고 lease fencing이 유효하면 목적지별
replica work를 독립 재시도할 수 있어야 한다. spool은 reference count 또는 필요한 replica가
끝날 때까지의 durable ownership을 가져야 하며, 저용량 임계치에서는 새 dump보다 기존
replica 완료를 우선해야 한다.

### I4. Plan 수정과 예약 occurrence의 재현성이 정의되지 않았다

Plan이 queue 대기 중 수정되면 기존 Work Item이 새 목적지·보존·검증 정책을 따라야 하는지
알 수 없다. 실행 시점에 mutable Plan을 다시 읽으면 같은 occurrence 재시도가 다른 동작을
한다.

- Plan은 revision을 가진다.
- enqueue 시 Target, Destination, schedule, policy reference와 실행에 필요한 옵션의
  immutable snapshot을 materialize한다.
- 자격증명 값 자체는 복사하지 않고 versioned secret reference를 사용한다.
- Plan 비활성화·삭제는 이미 승인된 occurrence의 cancel 정책을 별도로 적용한다.

### I5. 현재 SQLite writer 패턴은 async dispatcher와 다중 프로세스 소유권에 맞게 바꿔야 한다

현재 `CatalogWriterHandle::run`은 std 채널 응답을 동기로 기다린다. 짧은 transaction에는
문제가 작지만 Tokio worker에서 직접 호출하면 runtime thread를 block한다. 또한 각 CLI가
writer actor를 하나씩 만들 수 있어 시스템 전체 single writer가 아니다.

service 내부에서는 async channel 기반 DB actor 또는 명시적인 blocking DB executor를 두고,
application service가 raw rusqlite connection을 보지 않게 repository 계약으로 감싸야 한다.
WAL, busy timeout, foreign keys는 유지하되 schema migration은 service 시작 시 독점 lock 아래
실행해야 한다. queue claim과 상태 전이는 일반 catalog upsert보다 우선순위가 높아야 한다.

### I6. Operational DB와 catalog projection의 수명주기를 분리해야 한다

100,000 Activity Event와 queue, 구성, artifact projection을 한 파일에 둘 수는 있지만,
retention과 rebuild 범위가 다르다.

- Activity payload와 debug log blob을 분리하고 요약 필드는 인덱싱한다.
- debug log는 압축·보존기한·페이지 단위 읽기를 적용한다.
- artifact projection rebuild는 control tables를 건드리지 않는다.
- operational backup/export는 queue의 일시 상태를 제외하고 일관된 transaction snapshot으로
  만든다.
- SQLite schema version과 Manifest format version을 독립 관리한다.

### I7. Engine Adapter를 하나의 거대 trait로 만들지 않아야 한다

공통 support contract는 제품 정책으로 옳지만 `probe`부터 `restore`까지 모두 한 Rust trait에
넣으면 backup-only 엔진과 SQLite 같은 file engine이 빈 구현을 양산한다.

권장 분해:

- `ConnectionProvider`: probe, discover, permission preflight
- `BackupProvider`: plan, spawn, artifact descriptor
- `RestoreProvider`: eligibility, plan, execute, validate
- `ToolProvider`: detect, version, integrity, compatibility
- capability descriptor가 어떤 provider가 존재하는지 표현

기존 `BackupSource`는 `BackupProvider` adapter 뒤에 그대로 유지해 점진적으로 전환한다.
Tauri/SQLite 타입이 provider trait에 새지 않게 하고 직렬화 가능한 application DTO만
경계에 둔다.

### I8. Capability는 현재 probe 결과와 인증 matrix를 함께 가져야 한다

정적 엔진 capability와 특정 연결에서 probe된 capability를 구분하지 않으면 SQL Server
DACPAC, Redis backup-only, 버전별 restore 제한을 정확히 표시할 수 없다.

- `declared_capability`: adapter가 구현한 기능
- `observed_capability`: 현재 server/tool/credential probe 결과
- `certified_capability`: 관리 testbed matrix를 통과한 조합

사용자에게 “지원”으로 표시할 수 있는 것은 세 조건의 교집합이다. probe 결과에는
`observed_at`, tool hash/version, server version, credential revision을 기록해 stale 여부를
판정한다.

### I9. Unattended service의 secret 소유권과 ACL을 확정해야 한다

현재 DPAPI Machine-scope와 keyfile entropy는 무인 실행 seam으로 재사용할 수 있다. 하지만
별도 service가 어느 계정으로 실행되고 Tauri가 어느 계정으로 secret을 등록하는지 정하지
않으면 service가 keyfile을 읽지 못하거나, 반대로 불필요한 로컬 프로세스가 읽을 수 있다.

- service account, 파일 ACL, SQLite ACL, spool ACL, tool directory ACL을 설치 단계에서
  설정한다.
- Machine-scope DPAPI가 같은 머신의 관리자 공격을 막는 경계가 아님을 유지한다.
- UI는 평문 secret을 service IPC에 전달한 뒤 즉시 zeroize하고 Activity에 기록하지 않는다.
- export 기본값은 secret 제외다. secret 포함 export는 별도 암호화·재인증 경계를 둔다.
- `register-dpapi`, key rotation 등 현재 stub인 관리 흐름은 service 전환 전에 완성해야 한다.

### I10. Restore 승인은 UI가 아니라 application service에서 강제해야 한다

복원은 향후 CLI/API도 호출하므로 destructive warning과 eligibility gate를 React 버튼에만
두면 우회된다.

- restore command에 immutable restore plan hash와 짧은 유효기간의 confirmation token을
  요구한다.
- target identity, overwrite 정책, Snapshot identity, 검증 상태가 token에 결합돼야 한다.
- production target overwrite와 검증 실패 override는 별도 고위험 정책으로 감사 기록한다.
- restore가 성공해도 validation 실패면 `restored_unvalidated` 또는 실패 상태로 구분한다.

### I11. Activity Event의 “불변” 의미가 보안 감사로 과대 해석될 수 있다

로컬 SQLite append-only 규칙은 애플리케이션 수준의 변경 방지일 뿐 로컬 관리자에 대한
tamper evidence가 아니다. Activity Event를 사용자 판단용 운영 기록으로 정의하고, 보안
감사 증적이 필요하면 event chain hash 또는 서명된 Run/Drill evidence를 별도로 둬야 한다.
debug log와 Activity Event도 분리해 비밀 redaction과 보존 정책을 독립 적용한다.

### I12. 서명키 회전과 과거 Manifest 신뢰가 agent 이전에도 필요하다

Manifest가 진실 소스라면 현재 verifying key 하나만 핀하는 모델은 키 회전 후 과거
Snapshot을 어떻게 검증할지 정의해야 한다.

- signing key ID와 유효기간을 Manifest에 포함한다.
- 과거 공개키의 신뢰 이력과 폐기·침해 상태를 keystore/control-plane export에 보존한다.
- 정상 회전과 compromise rotation의 복원 정책을 구분한다.
- catalog rebuild가 어느 key set으로 어떤 Manifest를 수락했는지 evidence를 남긴다.

### I13. Tool supply chain 검증은 adapter preflight의 필수 입력이어야 한다

도구 경로와 버전만 기록해서는 교체된 바이너리를 탐지할 수 없다. bundled tool manifest,
hash 또는 vendor signature, 라이선스, 실행 플랫폼을 versioned inventory로 관리한다.
실행 직전 확인한 tool identity를 Run과 signed Manifest에 기록한다. 사용자 지정 PATH 도구는
별도 위험 상태와 명시적 승인 없이 certified capability를 얻지 못하게 하는 편이 안전하다.

### I14. Sleep/resume, 시간 변경, 종료와 업데이트가 queue 의미에 포함돼야 한다

Windows 데스크톱 환경에서는 절전 중 heartbeat와 예약 시각이 함께 멈춘다.

- due time은 UTC로 저장하고 표시만 local timezone을 사용한다.
- 절전 복귀 후 coalesce와 missed run/RPO breach를 결정적으로 계산한다.
- lease owner에 service instance/boot identity를 포함하고 wall-clock backward jump에
  민감하지 않은 grace 규칙을 둔다.
- 업데이트는 새 claim을 중지하고 작업을 drain하거나 안전 checkpoint에서 종료한 뒤 schema를
  migrate한다.
- 서비스 강제 종료·재부팅·업데이트 중 각 phase의 기대 복구 행동을 테스트한다.

### I15. 관측성은 로그가 아니라 queue와 resource 상태를 중심으로 설계해야 한다

최소 운영 지표는 queue depth 외에 lane별 oldest age, RPO 예상 위반 시각, permit wait,
active process, spool 예약량, destination별 backlog, lease renewal 실패, orphan cleanup이다.
metric label에 connection/plan UUID를 무제한 넣으면 cardinality가 커지므로 Activity 검색
필드와 집계 metric label을 분리한다. 최근 성공이 최근 실패를 가리는 현재 read model 문제는
별도 attention projection으로 해결한다.

### I16. 테스트 seam을 구현과 동시에 만들지 않으면 dispatcher가 검증 불가능해진다

다음 의존성을 trait 또는 injectable service로 둬야 한다.

- clock와 schedule calculator
- ID/idempotency key generator
- queue repository와 transaction boundary
- resource allocator
- process supervisor
- engine/destination fake adapter
- filesystem/spool capacity probe
- fault injector

테스트는 최소 다음을 포함해야 한다.

1. 모든 상태 전이와 금지 전이의 table test
2. claim 경쟁, lease expiry, stale fencing token, retry backoff의 deterministic test
3. dump 종료 전후, object upload 후, Manifest publish 전후, SQLite projection 전후 crash matrix
4. cancellation과 timeout 후 child tree·pipe·spool 잔존 여부
5. 2·4·8·16 동시 due에서 global/host/CPU/destination 상한 불변식
6. 느린 host·느린 destination·disk low-water 격리
7. v1 TOML에서 v2 DB로 migration, 중단 후 재실행, rollback/export round-trip
8. adapter별 support contract conformance와 실제 restore E2E
9. redaction, tool hash 변조, secret export, restore confirmation의 security regression
10. 100,000 Activity Event pagination과 주요 dashboard query 성능

ignored 인프라 테스트는 릴리스 통과로 계산하지 말고 관리 testbed 결과를 별도 release artifact로
수집해야 한다.

### I17. Future Agent seam은 준비하되 distributed queue로 미리 일반화하지 않아야 한다

initial v2는 단일 Windows service와 local SQLite가 가장 단순하고 안전하다. 지금부터
distributed lease나 multi-node consensus를 넣는 것은 과도하다. 다만 Work Item request,
capability report, progress/result event, artifact identity를 직렬화 가능한 protocol-neutral
DTO로 두면 이후 agent가 같은 계약을 사용할 수 있다.

agent 도입 시에는 local resource permit과 central assignment lease가 별개라는 점, agent가
offline일 때 secret을 누가 보유하는지, result signature와 protocol version을 새 ADR로
결정해야 한다.

## Nice-to-Have Findings

### N1. 적응형 동시성은 고정 permit 이후 단계로 둔다

CPU, disk latency, source replication lag, destination throttling을 보고 permit을 자동 조정할
수 있지만 첫 버전은 예측 가능한 고정 상한이 낫다. 실제 telemetry와 부하 시험이 쌓인 뒤
명시적 opt-in으로 도입한다.

### N2. 대역폭 제한과 시간대별 resource budget을 지원할 가치가 있다

호스트별 dump 수만으로 운영 시간의 네트워크 영향을 제어하지 못할 수 있다. destination별
bandwidth cap과 업무 시간/야간 resource profile은 다수 DB 운영에 유용하지만 correctness
게이트 뒤에 둔다.

### N3. Queue simulation 화면은 운영 설정 실수를 줄일 수 있다

현재 Plan 수, 예상 dump 시간, permit, RPO로 다음 24시간 queue를 시뮬레이션해 충돌과 예상
지연을 보여주면 사용자가 concurrency를 무작정 높이는 일을 줄일 수 있다.

### N4. Transactional outbox가 향후 API·agent 확장을 단순화한다

Activity Event와 외부 notification을 같은 transaction에서 outbox로 기록하면 crash 사이의
알림 유실을 줄일 수 있다. 첫 버전에서도 notification 신뢰가 중요하면 도입하고, 아니면
application event 인터페이스만 확보한다.

### N5. Dynamic plugin ABI는 당분간 피하는 것이 낫다

엔진과 목적지 확장은 정적 Rust registry와 capability descriptor로 충분하다. 안정적인
plugin ABI와 sandbox는 agent/API보다도 별도 제품 문제이므로 초기 확장성 목표에 넣지 않는다.

### N6. Operational DB 물리 분리는 규모가 커질 때 선택할 수 있다

초기에는 한 SQLite 파일 안에서 control schema와 projection schema를 논리 분리해도 된다.
Activity 보존과 rebuild가 writer latency를 방해하는 데이터가 확인되면 파일을 분리하되,
분산 transaction을 만들기 전에 outbox/reconcile 전략을 먼저 설계한다.

## 권장 마이그레이션 순서

### 1. 실행 소유권과 데이터 소유권 고정

- ADR: `dbbackup-service` 수명주기와 singleton
- ADR: Operational SQLite, Manifest, secret store 진실 범위
- ADR: at-least-once queue, fencing, commit protocol
- v2 schema와 migration runner 작성

### 2. 기존 코어를 감싸는 application seam

- `ConnectionService`, `PlanService`, `SnapshotQueryService`, `RestoreService`
- `ConnectionProvider`, `BackupProvider`, `RestoreProvider`, `ToolProvider`
- 기존 TOML과 `BackupSource`를 읽는 compatibility adapter
- Tauri와 CLI의 직접 sidecar 실행을 command submission으로 전환

### 3. Persistent queue와 process supervisor

- PlanOccurrence/BackupRun/WorkItem/Lease/Activity schema
- 단일 dispatcher, resource allocator, Windows Job Object supervisor
- crash matrix, load/backpressure test를 기존 3개 엔진 fake adapter로 먼저 통과

### 4. Snapshot·Replica·Verification read model

- 기존 fan-out 결과를 ArtifactReplica로 기록
- 서명 Manifest reconcile과 catalog rebuild 경계 분리
- snapshot 선택 기반 restore와 service-side eligibility/confirmation

### 5. 기존 엔진의 support contract 완성

- PostgreSQL, MySQL, MariaDB probe/discover/restore/validate
- globals pairing, tool integrity, 실제 L3 evidence
- 여기까지 통과한 뒤 SQL Server·SQLite wave 시작

### 6. 제품 UI와 후속 확장

- Connections, Databases, Plans, Activity, Snapshots, Recovery 순으로 application service에 연결
- 접근성·100k event query·UI 재접속 검증
- agent/API는 single-node service 계약이 안정된 뒤 별도 ADR로 시작

## Release Gate 보강안

| Gate | 추가해야 할 아키텍처 판정 |
|---|---|
| G0 Truth | 현재 여러 CLI writer와 job별 process 경합, stub 관리 명령, false-green 테스트를 재현 |
| G1 Core Flow | v1 config migration과 application service 경유 E2E, Plan revision 재현성 |
| G2 Concurrency | fencing stale commit 차단, process tree cleanup, phase resume, replica retry |
| G3 Product | UI 종료·재실행 중 service 지속, Activity 재연결, 부분 성공의 정확한 표시 |
| G4 Engine | declared/observed/certified capability 교집합과 tool identity evidence |
| G5 Expansion | agent identity, secret custody, protocol version, central/local lease 분리 |

## 최종 권고

현재 계획은 방향상 GO다. 단, 구현 시작점은 UI 재구성이나 신규 엔진 추가가 아니라
`service host + operational ownership + fenced queue + execution cardinality + process
supervisor`여야 한다. 이 다섯 기반이 완료된 뒤에는 Databasement 수준의 UI 흐름과 엔진
확장이 기존 Rust 보안 코어 위에 자연스럽게 올라간다.

## Dictionary Updates

| 행위 | 용어 | 정의(제안) | 근거 |
|---|---|---|---|
| ADD | Plan Occurrence(계획 발생) | Backup Plan의 특정 revision이 특정 `scheduled_for` 시각에 발생시킨 하나의 실행 의도. 다수 Target Run을 묶고 idempotency key를 소유한다. | 다중 Target 계획과 중복 schedule tick을 정확히 모델링해야 함 |
| ADD | Artifact Replica(아티팩트 복제본) | 하나의 Snapshot payload와 Manifest가 특정 Destination에 commit된 복제 상태. 목적지별 적격성·보존·오류를 소유한다. | fan-out 부분 성공과 복원 reader 적격성을 Snapshot 성공과 분리해야 함 |
| ADD | Fencing Token(펜싱 토큰) | lease claim마다 발급되는 단조 증가 세대 또는 유일 토큰. 현재 토큰을 보유한 worker만 상태와 결과를 commit할 수 있다. | 만료된 worker의 늦은 commit과 중복 Snapshot 방지 |
| ADD | Process Supervisor(프로세스 감독자) | native tool process tree의 시작, pipe drain, timeout, 취소, wait, orphan cleanup과 임시 credential 정리를 소유하는 공통 실행 경계 | engine별 `kill_on_drop`만으로 dispatcher crash와 Windows child tree를 처리할 수 없음 |
| ADD | Plan Revision(계획 리비전) | Backup Plan 변경 시 증가하며 queued occurrence가 참조하는 불변 실행 설정 버전 | 대기 중 Plan 수정 후 재시도 의미를 결정적으로 유지 |
| ADD | Operational Store(운영 저장소) | Connection, Target, Plan, queue, lease, Run, Activity의 권위 소스인 SQLite 영역 | rebuild 가능한 Catalog projection 및 Manifest 진실 소스와 구분 필요 |
| REFINE | Snapshot(스냅샷) | 한 Database Target의 한 복구 시점을 나타내는 committed logical backup. 하나 이상의 Artifact Replica와 검증 기록을 가진다. | 다중 Destination fan-out에서 Snapshot과 저장 복제본을 혼용하지 않기 위함 |
| REFINE | Lease(실행 임대) | Work Item의 시한부 실행권. heartbeat, expiry뿐 아니라 fencing token과 owner instance identity를 필수로 가진다. | lease 만료 후 stale worker의 결과 commit을 차단해야 함 |
| REFINE | Activity Event(활동 이벤트) | 실행 상태 전이와 운영 판단을 위한 append-only application record. 기본적으로 로컬 관리자에 대한 tamper-evident 감사 로그를 뜻하지 않는다. | “불변” 표현의 보안 보장 과대 해석 방지 |
| REFINE | Support Contract(지원 계약) | declared adapter 기능, 현재 probe된 observed capability, 관리 testbed에서 인증된 capability의 교집합으로 인정되는 엔진 지원 기준 | enum·구현·현장 조건·복원 인증을 분리해야 함 |
