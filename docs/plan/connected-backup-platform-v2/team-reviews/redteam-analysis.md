# Red Team Review: Connected Backup Platform v2

> 검토일: 2026-07-26
> 대상: `spec.md`, `interview.md`, `research.md`, `unknowns.md`, `docs/domain-dictionary.md`
> 판정: 현재 초안 그대로는 구현 착수 `NO-GO`. 아래 Critical 항목을 명세와 출시 게이트에 반영한 뒤
> Wave 0에 한해 조건부 착수할 수 있다.

## 1. Executive Verdict

방향 자체는 타당하다. Rust 실행 코어를 폐기하지 않고 연결·탐색·계획·스냅샷·복원 제품 모델을
올리는 것은 재작성보다 안전하다. 그러나 현재 명세는 “중앙 dispatcher를 둔다”는 구조적 문장과
“안전하게 복구한다”는 결과 문장 사이의 실패 의미론이 비어 있다. 백업 제품에서 이 공백은 성능
문제가 아니라 중복 백업, 불완전 복구 지점, 거짓 성공, 복원 시 코드 실행으로 이어진다.

가장 위험한 가정은 다음과 같다.

- lease가 만료되면 이전 worker가 멈췄다고 가정한다.
- 서명된 매니페스트가 존재하면 완전하고 최신인 복구 지점이라고 가정한다.
- 여러 목적지 중 하나만 성공해도 계획의 내구성 목표가 충족됐다고 가정한다.
- PostgreSQL globals와 DB dump를 각각 커밋해도 하나의 복구 지점으로 안전하게 조합된다고 가정한다.
- `sqlpackage` DACPAC을 데이터 백업으로 취급할 수 있다고 가정한다.
- Tauri, CLI, Windows Scheduler가 같은 application service를 호출하면 단일 실행권자가 자연히
  보장된다고 가정한다.
- L3가 “실행 가능”하고 일부 ignored 테스트가 존재하면 복원 가능성이 검증됐다고 가정한다.
- TOML에서 SQLite로 정본을 옮기는 동안 이전 scheduler와 새 dispatcher가 동시에 실행되지
  않는다고 가정한다.

이 제품은 exactly-once 실행을 약속해서는 안 된다. 현실적인 계약은 “at-least-once 시도 +
idempotent occurrence + fenced commit + 재조정”이다. 이 계약이 도메인 사전, DB 스키마,
destination object key, queue 상태 전이에 동시에 반영되어야 한다.

## 2. Critical Findings

### C-01. 단일 실행권자가 아직 아키텍처 결정이 아니다

현재 Tauri는 `apps/console/src-tauri/src/run.rs`에서 `dbbackup.exe`를 직접 `std::process::Command`
로 기동한다. 기존 Scheduler도 job별 CLI 프로세스를 기동한다. v2 명세는 dispatcher만 예약 작업의
시작 권한을 가진다고 하지만, 데스크톱 종료 후에도 동작하는 Windows service인지, 짧게 기동되는
singleton daemon인지 확정하지 않았다.

이 상태로 점진 마이그레이션하면 UI, 구 scheduler, 새 dispatcher가 동일 계획을 동시에 실행할 수
있다. Tauri가 종료되어도 `std::process::Child`는 자동 종료되지 않으며 현재 sidecar 코드에는
PID 추적, 취소, Windows Job Object, 프로세스 트리 종료가 없다.

필수 완화:

1. Wave 0 ADR에서 유일한 execution authority를 Windows service 또는 OS singleton daemon으로
   확정한다.
2. UI·CLI·Scheduler는 모두 queue enqueue 또는 dispatcher wake만 수행한다.
3. 구 `run_job` 직접 실행 경로는 호환 기간에도 feature flag와 cutover marker로 차단한다.
4. Windows Job Object에 모든 dump/restore 하위 프로세스를 묶고, PID뿐 아니라 process start time을
   기록해 PID 재사용을 방지한다.
5. 서비스가 준비되지 않은 경우 예약 백업 기능을 “활성”으로 표시하지 않는다.

### C-02. lease 만료만으로 재실행하면 split-brain commit이 발생한다

`FR-DSP-04`는 heartbeat가 끊긴 작업을 재개 또는 실패 처리한다고 하지만, heartbeat 단절은 worker
종료를 뜻하지 않는다. SQLite writer 정체, suspend/resume, 네트워크 분할, 긴 CPU 단계에서도 이전
worker는 계속 원격 목적지에 commit할 수 있다. lease 만료 후 새 worker가 같은 occurrence를 다시
실행하면 두 개의 UUID Snapshot이 모두 유효하게 커밋될 수 있다.

필수 완화:

- `ScheduledOccurrence`에 `(plan_id, target_id, scheduled_for_utc)` 유일 제약을 둔다.
- lease마다 증가하는 `FencingToken`을 발급하고 heartbeat·상태 전이·완료를
  `(work_item_id, attempt_id, fencing_token)` 조건부 갱신으로 제한한다.
- 매니페스트에 occurrence ID와 fencing token을 서명해 넣는다.
- destination commit은 occurrence ID 기준 conditional-create 또는 동일 checksum 재사용으로
  멱등화한다.
- lease 만료 시 즉시 재배포하지 말고 이전 process/agent의 종료 또는 fenced 상태를 확인한다.
- “exactly once” 대신 “중복 시도 가능, committed recovery point는 occurrence당 하나”를 명세한다.

### C-03. 복구 지점의 원자 단위가 정의되지 않았다

현재 `run.rs`는 PostgreSQL globals와 각 DB part를 순차로 생성하고 part마다 원격 매니페스트를
독립 커밋한다. 앞 part가 성공한 뒤 뒤 part가 실패하면 이미 공개된 매니페스트는 남는다.
`primary_backup_id`도 마지막 part로 덮어쓴다. 여러 Target을 한 Backup Plan에 넣는 v2에서는 이
문제가 더 커진다.

`Snapshot`을 단일 artifact로 정의하면 다음을 구분할 수 없다.

- globals만 있고 DB dump가 없는 불완전 쌍
- 세 목적지 중 한 곳만 완전한 쌍을 가진 상태
- 한 Plan에서 일부 Target만 성공한 상태
- 동일 occurrence의 retry가 만든 복수 artifact

필수 완화:

- `RecoveryPoint` 또는 `SnapshotSet`을 도입해 required part 집합과 destination별 완전성을
  명시한다.
- part 매니페스트는 준비 상태로 두고, required part가 모두 존재한 뒤 서명된 set manifest를
  최종 commit marker로 게시한다.
- Backup Plan은 Target별 Work Item으로 분해하고 `PlanRun`은 집계 상태만 가진다.
- 복원 UI는 complete set만 기본 노출하고, 불완전 set은 진단·repair 대상으로만 표시한다.
- globals와 DB의 버전, source identity, consistency window가 맞는지 검증한다.

### C-04. 목적지 성공 정책이 데이터 내구성 요구와 충돌한다

현재 구현은 각 part에서 목적지 하나만 성공해도 백업 자체를 성공으로 두고 나머지를
`PartialSuccess`로 처리한다. 명세의 `FR-PLN-01`은 여러 Destination을 묶지만 어느 목적지가
필수인지, 최소 성공 수가 몇인지, object lock 목적지가 실패해도 성공인지 정하지 않았다.

사용자가 로컬+S3를 “2-copy 정책”으로 선택했는데 로컬만 성공해도 대시보드가 성공을 보여주면
랜섬웨어 내성 목표는 충족되지 않는다.

필수 완화:

- Destination membership에 `required`, `best_effort`, `repair_source` 역할을 둔다.
- Plan에 `all-required`, `minimum-copies`, `any-one` 내구성 정책을 명시한다.
- Backup Run 실행 성공과 Protection Compliance를 분리한다.
- required 목적지 실패는 성공 배지를 금지하고 repair queue와 RPO/RCO 위반을 생성한다.
- destination별 retention/object-lock 증적까지 충족해야 “보호됨”으로 판정한다.

### C-05. TOML→SQLite 전환의 migration과 rollback이 설계되지 않았다

현재 config migration은 `crates/dbbackup-config/src/migrate.rs`의 scaffold이며 실제 migration은
없다. catalog DDL도 `CREATE ... IF NOT EXISTS` 중심이고 operational queue 스키마 버전과 downgrade
차단 계약은 아직 없다. SQLite 정본 전환은 단순 serialization 변경이 아니다. secret reference,
연결 ID, scheduler task, 계획 occurrence, destination identity를 동시에 바꾼다.

가장 위험한 실패는 migration 도중 이전 Task Scheduler가 계속 실행되고 새 dispatcher도 동일
계획을 실행하는 것이다. rollback으로 구 바이너리를 다시 띄우면 새 queue를 인식하지 못해 같은
문제가 재발한다.

필수 완화:

1. migration 전 모든 구 scheduler task를 열거하고 일시 중지한 뒤 snapshot을 남긴다.
2. TOML, keystore, catalog DB, Scheduler XML을 한 recovery bundle로 백업한다.
3. shadow-import→참조 무결성 검증→dry-run diff→단일 cutover marker 기록 순서로 전환한다.
4. cutover marker가 존재하면 구 바이너리는 실행을 거부한다.
5. migration은 schema version별 transactional step과 checksum을 가진다.
6. rollback은 데이터 파일 복원뿐 아니라 scheduler 권한 반환까지 포함한다.
7. import/export 시 stable ID와 secret reference 충돌 규칙을 명시한다.

### C-06. SQL Server DACPAC은 데이터 백업 계약으로 사용할 수 없다

Engine Roadmap의 “`sqlpackage` DACPAC”은 치명적인 범위 오류다. DACPAC은 주로 스키마 모델이며
일반적인 데이터 포함 논리 백업이 아니다. 데이터를 포함하는 논리 이동 형식은 BACPAC 계열이고,
서버 수준 복구·로그 체인·시점 복구는 native `.bak` 및 로그 백업 계약과도 다르다.

DACPAC 생성이 성공했다고 데이터 복원 가능 백업으로 표시하면 실제 장애 시 스키마만 남는
데이터 손실로 이어진다.

필수 완화:

- SQL Server Wave 1을 “BACPAC logical export/import 후보”로 다시 조사·검증한다.
- DACPAC은 schema-only export capability로 별도 표시하고 `Backup` 성공 지표에 포함하지 않는다.
- 데이터 유형, 크기, 사용자·로그인, 권한, SQL Agent job, 암호화 객체, 외부 종속성 제외 범위를
  test matrix에 넣는다.
- `.bak`을 지원하기 전에는 “full database recovery” 문구를 사용하지 않는다.

### C-07. spool 디스크 고갈은 이미 선언된 설정으로도 방어되지 않는다

`PipelineCfg.min_free_disk`는 존재하고 `preflight_disk_guard`도 정의돼 있지만 실제 run 경로에서
호출되지 않는다. `max_parallel_dumps`도 마찬가지로 선언만 됐다. 여러 DB를 병렬화하면 dump 크기를
모른 채 같은 볼륨에 spool을 만들고, SQLite catalog/WAL까지 같은 디스크에 둘 수 있다.

디스크 고갈은 단순 작업 실패가 아니라 queue heartbeat 정지, SQLite 손상, lease 재배포, orphan
프로세스, 중복 commit을 연쇄적으로 만들 수 있다.

필수 완화:

- 볼륨별 `SpoolReservation`을 permit과 함께 원자적으로 획득한다.
- 추정 크기가 없으면 보수적 상한 또는 사용자 quota를 사용한다.
- catalog/control DB와 대용량 spool을 가능한 별도 볼륨에 둔다.
- `.part` 경로에 쓰고 fsync 후 atomic rename으로 local commit한다.
- 디스크 임계치에는 soft throttle과 hard stop을 분리한다.
- 종료·크래시 후 orphan spool/manifest를 안전하게 분류하는 scavenger를 둔다.

### C-08. 실제 대상 복원과 restore-as-exec 경계가 아직 제품 계약이 아니다

현재 CLI restore는 `--decrypt-only`만 지원하며 실제 DB target 복원은 명시적으로 미배선이다.
L3 Docker sandbox와 운영 대상 복원은 다른 위험 모델이다. 논리 dump는 신뢰된 데이터가 아니라
DB 정의, 함수, trigger, event, `DEFINER`, 확장, 파일·프로그램 접근을 포함할 수 있는 실행 입력이다.
서명은 출처를 증명할 뿐, source가 침해된 시점에 만들어진 악성 dump의 안전성을 증명하지 않는다.

필수 완화:

- 기본 복원은 새 DB·새 namespace에만 허용하고 기존 DB overwrite는 별도 파괴적 승인으로 분리한다.
- restore preflight에서 provenance, engine/tool matrix, 서명, 무결성, 최근 L3, target version,
  권한 차이를 모두 평가한다.
- restore credential은 create/import에 필요한 최소 권한과 사후 소유권 이전 권한을 분리한다.
- PG globals, MySQL routines/trigger/DEFINER, SQL Server 외부 객체 등 실행성 객체를 inventory로
  보여주고 정책으로 차단할 수 있게 한다.
- L3는 격리 네트워크, CPU/메모리/디스크 제한, 고정 digest 이미지, timeout을 강제한다.
- 운영 복원은 dry-run 결과와 exact target fingerprint를 재확인한 뒤 시작한다.

### C-09. “지원”과 “검증됨”이 현 코드보다 앞서 있다

현재 자동 backup 후 verify 배선은 없고, L2/L3 핵심 테스트는 외부 인프라 의존으로 ignored다.
실제 restore CLI는 decrypt-only이며 prune 실행도 `not implemented yet`이다. 이 상태에서
G1의 기존 세 엔진 “연결→탐색→계획→백업→검증→복원 E2E”는 새 기능 개발 전에 먼저 실패해야
정상이다.

필수 완화:

- G0에서 false-green 테스트를 “통과”가 아니라 명시적 실패로 전환한다.
- ignored L2/L3를 CI 성공 건수에 포함하지 않는다.
- 관리 testbed가 없는 matrix 항목은 `experimental` 또는 `backup-only-unverified`로 표시한다.
- test 결과에는 engine server version, client tool version, OS, image digest, timestamp를 저장한다.
- retention, verification cadence, notification이 저장만 되고 실행되지 않는 경우 UI에서
  “구성됨”과 “시행됨”을 구분한다.

### C-10. 서명된 매니페스트는 최신성이나 완전한 이력을 증명하지 않는다

매니페스트는 artifact의 출처·무결성 정본으로 적합하지만, 목적지에서 최신 매니페스트를 삭제하거나
과거 상태만 replay해도 개별 서명은 여전히 유효하다. catalog rebuild만으로는 “마지막 백업 이후
누락된 occurrence”와 “실패 이력”을 복구할 수 없다. append-only 목적지라도 listing 일관성,
prefix 축소, credential 범위에 따라 누락을 감지하지 못할 수 있다.

필수 완화:

- plan/target별 단조 증가 occurrence sequence 또는 hash-chained signed ledger를 둔다.
- reconcile은 발견된 매니페스트뿐 아니라 예상 occurrence ledger와 비교한다.
- 목적지별 list coverage, pagination, continuation token, listing 시각을 증적으로 남긴다.
- 삭제·누락은 단순 “snapshot 없음”이 아니라 freshness/RPO 위반으로 판정한다.
- signing key rotation과 폐기된 key의 검증 역사도 ledger에 포함한다.

## 3. Important Findings

### I-01. permit 획득 순서와 자원 정체성이 없다

전역→호스트→CPU→목적지 permit을 작업마다 다른 순서로 잡으면 교착과 head-of-line blocking이
생긴다. permit을 일부 보유한 채 다음 permit을 기다리지 않도록 원자적 admission 또는 고정 획득
순서를 명시해야 한다. `host:port` 문자열은 alias, tunnel, DNS round-robin으로 우회될 수 있으므로
`SourceResourceIdentity`를 canonicalize해야 한다.

### I-02. heartbeat 자체가 진행 증거가 아니다

coordinator가 heartbeat를 계속 쓰면서 dump process가 무한 hang될 수 있다. phase progress,
bytes read/written, last child output 시각, DB-side activity를 분리하고 phase별 stall timeout을
둬야 한다.

### I-03. Activity 폭주가 lease를 만료시킬 수 있다

SQLite single writer에 로그 event, queue heartbeat, catalog upsert가 함께 몰리면 사용자 로그가
제어 상태를 굶길 수 있다. heartbeat·lease CAS를 높은 우선순위의 짧은 transaction으로 분리하고,
Activity는 bounded buffer, batch insert, backpressure, drop counter를 사용해야 한다.

### I-04. 일정 의미론이 DST·절전·장기 종료를 다루지 않는다

`coalesce-one`만으로는 DST 중복 시각, 존재하지 않는 시각, PC 절전, 앱 장기 종료 후 catch-up 범위를
결정할 수 없다. schedule timezone을 IANA/Windows timezone ID로 고정하고 occurrence는 UTC로
materialize해야 한다. `skip`, `latest-one`, `bounded-catch-up` 정책과 missed occurrence ledger가
필요하다.

### I-05. retry가 장애를 증폭할 수 있다

지수 backoff만으로는 동일 목적지 장애 시 수백 plan이 동시에 재시도하는 thundering herd를 막지
못한다. full jitter, destination circuit breaker, retry budget, deadline, user pause, RPO priority
승격이 필요하다.

### I-06. “최근 성공”과 “현재 보호 상태”를 분리해야 한다

최근 백업 성공, required destination 충족, verification freshness, restore drill freshness, RPO,
retention은 서로 다른 축이다. 단일 초록 상태나 최고 검증 레벨을 보여주면 false-green이 된다.
각 Snapshot의 검증 결과와 Plan의 현재 compliance를 별도 계산해야 한다.

### I-07. L3 smoke query는 부분 데이터 손실을 놓칠 수 있다

schema hash와 총 row count만 맞아도 특정 테이블·LOB·encoding·constraint·sequence·view·routine이
손상될 수 있다. 엔진별 sentinel dataset과 object inventory, table별 row count, sampled content
hash, sequence/identity, constraint 활성 상태를 검증해야 한다.

### I-08. 엔진별 일관성 수준을 같은 “백업”으로 숨기면 안 된다

- MySQL/MariaDB의 non-transactional table은 `--single-transaction`으로 일관성이 보장되지 않는다.
- MongoDB는 standalone, replica set, sharded cluster에서 일관성 계약이 다르다.
- SQLite는 WAL과 장기 transaction 상태에서 단순 파일 복사가 안전하지 않다.
- Redis/Valkey RDB 취득은 서버 filesystem 접근과 save 정책이 필요할 수 있다.
- PostgreSQL logical dump와 globals는 동일 시점 원자 snapshot이 아니다.

각 실행에 `ConsistencyGrade`와 evidence를 기록하고 UI에서 복원 범위를 명시해야 한다.

### I-09. Redis/Valkey “backup-only”는 제품 약속과 충돌한다

제품 정의는 실제 복원 가능성 증명이다. 복원 절차와 서비스 중단 영향이 검증되지 않은 RDB 파일
수집을 “지원 DB”로 노출하면 백업 파일 수집기일 뿐이다. 최소한 수동 restore runbook과 관리
testbed 검증이 없으면 실험적 export로만 분류해야 한다.

### I-10. 목적지 repair와 orphan 정리가 없다

manifest upload 전에 실패한 ciphertext, 일부 목적지만 실패한 Snapshot, 불변 저장소의 orphan
object를 어떻게 다룰지 정의되지 않았다. append-only writer는 list/delete가 불가능할 수 있어
자동 청소도 못 한다. repair source 선정, checksum 비교, 불변성 보존 복제, orphan report,
비용 추적이 필요하다.

### I-11. secret reference와 회전 의미론이 필요하다

운영 SQLite에는 secret payload가 아니라 versioned `SecretRef`만 저장해야 한다. queued Work Item이
enqueue 시점 credential version을 고정할지, 시작 시 최신 credential을 resolve할지 결정해야 한다.
agent에 전달할 때는 장기 DB password 대신 short-lived scoped credential을 우선해야 한다.
지원 번들에는 canary secret 기반 누출 회귀 테스트가 필요하다.

### I-12. 환경변수 secret 전달도 완전한 비노출은 아니다

현재 PostgreSQL과 MySQL dump는 비밀번호를 argv 대신 `PGPASSWORD`/`MYSQL_PWD`에 넣는다. argv
노출은 줄지만 같은 사용자 권한의 프로세스, crash dump, 상속된 child에는 노출될 수 있다.
프로세스별 최소 환경 구성, 상속 제한, OS ACL, 실행 계정 분리, 가능하면 password file/pipe와
즉시 삭제를 검토해야 한다. 로그 redaction이 환경 노출 위험을 대체하지는 않는다.

### I-13. 로컬 control store 손실 복구가 artifact 복구와 다르다

매니페스트 rebuild는 Snapshot inventory 일부를 되살릴 뿐 Connection Profile, Plan, schedule,
secret mapping, occurrence ledger, notification 설정을 복원하지 못한다. operational SQLite의
암호화 export, 정기 백업, integrity check, disaster recovery 절차가 필요하다.

### I-14. native tool 공급망과 호환성은 출시 게이트여야 한다

도구 경로를 찾는 것만으로는 충분하지 않다. 다운로드 origin, license, Authenticode/hash,
버전 pinning, downgrade, 자동 업데이트, CVE 대응, Windows architecture를 관리해야 한다.
사용자 PATH 폴백은 검증되지 않은 동명 바이너리를 실행할 수 있으므로 production 기본값으로
허용하지 않는 편이 안전하다.

### I-15. 복원과 백업의 자원 우선순위가 모호하다

restore permit을 별도로 두어도 source host, CPU, spool volume, destination bandwidth는 공유한다.
복구 중 대규모 예약 백업이 RTO를 방해하거나, 복구 우선으로 모든 백업을 멈춰 RPO를 위반할 수
있다. 운영자가 볼 수 있는 priority override와 자원 예약 비율이 필요하다.

### I-16. Plan 삭제·Connection drift의 불변 참조가 필요하다

실행 시점에 live Plan/Connection을 다시 읽으면 queued 작업이 사용자가 방금 바꾼 다른 대상이나
옵션으로 실행될 수 있다. Work Item은 비밀을 제외한 immutable execution spec hash와 engine/tool
matrix를 캡처해야 한다. 삭제는 tombstone으로 남기고 과거 Run·Snapshot 참조를 보존해야 한다.

### I-17. 범위가 한 릴리스에 비해 과도하다

정규화 모델, SQLite migration, dispatcher, 서비스, UI 9개 영역, 엔진 8종, destination 확장,
verification, restore, agent seam을 동시에 진행하면 핵심 실패 의미론보다 화면과 로고가 먼저
완성될 가능성이 높다. Wave 0 완료 전 Wave 1 adapter 코드 착수를 금지하는 branch/release gate가
필요하다.

## 4. Nice-to-Have Findings

### N-01. 고정 permit보다 자동 조절이 장기적으로 유리하다

초기 보수값은 적절하지만, 향후 p95 dump latency, spool pressure, destination throttle을 이용한
안전한 adaptive concurrency를 고려할 수 있다. 초기 v2에는 필요하지 않다.

### N-02. 예상 용량·시간·비용을 plan 생성 시 보여줄 수 있다

직전 dump 추세와 destination 단가를 바탕으로 backup window, spool peak, 월 저장 비용을
예측하면 운영자가 concurrency와 retention을 현실적으로 정할 수 있다.

### N-03. 대역폭 창과 업무시간 정책이 유용하다

원격 DB와 S3 upload는 업무 트래픽을 방해할 수 있다. destination/source별 bandwidth limit과
업무시간 pause window는 엔진 확장 이후 가치가 높다.

### N-04. queue 시뮬레이터가 설정 오류를 줄인다

Plan 저장 전 다음 24시간 occurrence와 예상 resource contention을 시뮬레이션하면 몰림 현상을
사전에 보여줄 수 있다.

### N-05. 규정 준수 export는 내부 상태와 분리하는 편이 좋다

verification evidence, destination immutability, restore drill을 서명된 audit bundle로 내보내면
운영 DB 손실 시에도 증적을 보존할 수 있다.

### N-06. UI에는 “지원” 대신 capability matrix가 더 정확하다

엔진 로고 하나보다 backup format, consistency, L2, L3, direct restore, PITR, globals/server
objects의 지원 여부를 작은 matrix로 보여주는 편이 과대 약속을 줄인다.

## 5. Probability × Impact Risk Matrix

척도: Probability와 Impact는 각각 1(낮음)~5(매우 높음), Score는 두 값의 곱이다.

| ID | 위험 | P | I | Score | 필수 완화 |
|----|------|---|---|-------|-----------|
| R-01 | verify·restore 미배선인데 지원/보호됨으로 표시 | 5 | 5 | 25 | G0 false-green 제거, 관리 testbed 증적 |
| R-02 | lease 만료 후 구 worker와 신 worker가 이중 commit | 4 | 5 | 20 | fencing token, occurrence unique, CAS completion |
| R-03 | migration 중 구 Scheduler와 dispatcher 이중 실행 | 4 | 5 | 20 | scheduler freeze, cutover marker, 구 바이너리 차단 |
| R-04 | SQL Server DACPAC을 데이터 백업으로 오인 | 4 | 5 | 20 | BACPAC/.bak 계약 재정의, schema-only 명시 |
| R-05 | globals/DB 또는 다중 Target 일부만 커밋된 복구 지점 | 4 | 5 | 20 | SnapshotSet, set manifest, complete gate |
| R-06 | spool 동시 생성으로 디스크·SQLite 고갈 | 4 | 5 | 20 | 볼륨 reservation, hard stop, 별도 control volume |
| R-07 | 한 목적지만 성공했는데 계획을 보호됨으로 표시 | 4 | 5 | 20 | required destination과 compliance 상태 |
| R-08 | 실제 복원에서 dump 내 실행성 객체가 권한 악용 | 3 | 5 | 15 | provenance, least privilege, object policy, 새 DB 기본 |
| R-09 | Tauri/CLI 종료 후 dump process orphan | 4 | 4 | 16 | Windows Job Object, process identity, kill tree |
| R-10 | retention 미시행으로 저장소 고갈·정책 위반 | 4 | 4 | 16 | 시행 상태 분리, prune E2E, destination별 증적 |
| R-11 | retry storm이 DB·목적지 장애를 증폭 | 3 | 4 | 12 | jitter, circuit breaker, retry budget |
| R-12 | Activity write 폭주로 heartbeat 지연·lease 만료 | 3 | 4 | 12 | 제어 transaction 우선순위, batch log, drop counter |
| R-13 | DST·절전·장기 종료로 occurrence 누락 또는 중복 | 3 | 4 | 12 | UTC occurrence ledger, catch-up 정책 |
| R-14 | 서명된 과거 매니페스트 replay로 최신 손실 은폐 | 2 | 5 | 10 | signed sequence/hash-chain, expected occurrence reconcile |
| R-15 | engine 확장 범위가 Wave 0 완성을 잠식 | 5 | 4 | 20 | Wave 0 branch gate, 신규 adapter 착수 금지 |
| R-16 | secret가 UI 문자열·환경·support bundle로 노출 | 3 | 5 | 15 | SecretRef, 최소 환경, canary redaction test |
| R-17 | control SQLite 손실로 계획·연결·일정 복구 불가 | 3 | 4 | 12 | 암호화 control backup, integrity check, DR drill |
| R-18 | L3 smoke query가 부분 데이터 손실을 통과 | 3 | 5 | 15 | sentinel/content hash/object inventory |

## 6. Required State-Machine Corrections

### 6.1 Queue

권장 상태는 단순 문자열보다 전이 조건을 포함해야 한다.

```text
scheduled
  -> queued
  -> leased(attempt_id, worker_id, fencing_token, expires_at)
  -> starting(process_identity)
  -> running(last_progress_at)
  -> committing(commit_intent)
  -> succeeded | partial | failed | cancelled | expired
```

규칙:

- `expired`는 terminal success/failure가 아니라 소유권 불확실 상태다.
- `cancel_requested`와 `cancelled`를 구분한다.
- `committing` 이후에는 destination reconcile 없이 재실행하지 않는다.
- terminal update는 fencing token CAS에 성공한 worker만 수행한다.
- 상태 전이는 append-only event와 current row를 같은 transaction으로 기록한다.

### 6.2 Backup/Recovery Point

```text
PlanRun
  -> TargetRun[]
      -> ArtifactPart[]
      -> DestinationCopy[]
      -> RecoveryPointSet
```

- `TargetRun succeeded`: required part가 로컬 pipeline gate를 통과
- `DestinationCopy committed`: ciphertext와 part manifest가 모두 존재
- `RecoveryPointSet complete`: required part 전체의 set manifest가 commit
- `Protection compliant`: required destination 수, immutability, verification freshness 충족
- `Restorable`: support matrix와 restore eligibility gate 통과

이 다섯 상태를 “성공” 하나로 합치지 않는다.

## 7. Mandatory Failure-Injection and Anti-False-Green Tests

| 영역 | 반드시 주입할 실패 | 통과 조건 |
|------|-------------------|-----------|
| Queue lease | heartbeat 직전/직후 dispatcher kill, SQLite busy, 시스템 절전 | occurrence당 complete set 1개, stale attempt 완료 CAS 거부 |
| Process | dump 본체와 손자 process hang, UI/service 강제 종료 | Job Object 전체 종료, orphan 0, partial commit 0 |
| Pipeline | stdout 중단, stderr flood, exit 0+fatal stderr, `spawn_blocking` 취소 | 파이프 deadlock 없음, spool 미커밋, cancellation bounded |
| Disk | spool 도중 disk full, catalog WAL disk full, reservation 경쟁 | control DB 보존, 신규 admission 중단, 기존 상태 결정 가능 |
| Destination | ciphertext 성공 후 manifest 실패, manifest 성공 후 app crash | reconcile이 orphan/committed를 구분, 중복 Snapshot 없음 |
| Multi-part | PG globals 성공 후 DB 실패, DB 성공 후 set manifest 실패 | Restorable 목록에 불완전 set 미노출 |
| Multi-destination | required S3 실패·best-effort local 성공 | Backup 실행 결과와 compliance 상태 분리 |
| Schedule | DST forward/backward, 24시간 절전, clock rollback | occurrence ledger 누락·중복 없음, 정책대로 catch-up |
| Migration | 각 migration step 직후 전원 차단, 구 바이너리 재실행 | 원본 복구 또는 완료 재개, 이중 scheduler 실행 없음 |
| Restore | 서명 정상이나 악성 routine/trigger 포함, target fingerprint 변경 | 정책 차단 또는 재승인, 잘못된 target 변경 없음 |
| Verification | 빈 DB, 일부 테이블 누락, LOB 손상, sequence 불일치 | L3 실패, evidence가 Snapshot에 귀속 |
| Secret | canary password를 stderr/env error/support bundle에 주입 | 최종 사용자 출력과 bundle에서 canary 0건 |
| Engine matrix | 서버/client patch 조합 변경, tool downgrade | 지원 상태 자동 강등, 검증 전 로고/지원 문구 미노출 |
| Catalog rebuild | 최신 매니페스트 삭제·과거만 replay | freshness/RPO 위반 탐지, 과거 성공으로 정상화 금지 |
| Retention | object lock 중 prune, partial copy, clock skew | required copy 조기 삭제 0, 실패가 compliance에 노출 |

테스트 운영 규칙:

- ignored 테스트는 pass 집계에서 제외하고 “미실행”으로 실패 게이트에 표시한다.
- mock E2E와 managed live restore E2E를 별도 지표로 둔다.
- 테스트가 artifact 생성만 확인하지 않고 restore 후 content oracle까지 확인해야 한다.
- fault point마다 crash 전후 SQLite와 destination 상태를 모두 검증한다.
- support matrix의 각 행은 마지막 성공 증적과 만료 시각을 가져야 한다.

## 8. Scope and Sequencing Corrections

현재 Release Gate를 다음처럼 강화해야 한다.

1. `G0 Truth and Cutover Safety`
   - 기존 세 엔진의 실제 결과 기록, verify/restore/retention 미배선 재현
   - execution authority ADR
   - TOML→SQLite migration·rollback rehearsal
   - 구 Scheduler 이중 실행 차단
2. `G1 Durable Core`
   - occurrence unique, fencing token, process Job Object, spool reservation
   - RecoveryPointSet과 required destination semantics
   - crash-point reconcile
3. `G2 Existing Engine Recovery`
   - PostgreSQL/MySQL/MariaDB managed backup→restore→content validation
   - 자동 verification cadence와 retention 시행
4. `G3 Product UI`
   - Connections, Targets, Plans, Activity, Snapshots, Recovery
   - 성공/compliance/restorable 상태 분리와 접근성
5. `G4 New Engine`
   - SQL Server 형식 재결정 후 SQLite와 각각 독립 출시
   - 엔진당 support contract 전 행 통과
6. `G5 Expansion`
   - agent/API/RBAC은 별도 trust-boundary ADR 이후

Wave 0의 Definition of Done을 통과하기 전 SQL Server·SQLite adapter 구현을 시작하지 않는 것이
좋다. Databasement 수준의 정보구조는 UI 범위 기준선으로 사용하되, 엔진 수와 팀 기능까지 같은
마일스톤에 묶어 “패리티”라고 부르지 않는다.

## 9. Decisions Required Before Implementation

질문 형태로 작업을 멈출 필요는 없지만, 아래 기본값을 spec에 확정해야 한다.

| 결정 | 레드팀 권장 기본값 |
|------|-------------------|
| execution authority | UI와 독립된 Windows service singleton |
| delivery semantics | at-least-once attempt + fenced idempotent occurrence |
| queue work granularity | Target당 Work Item, PlanRun은 집계 |
| backup atomic unit | required part를 묶은 RecoveryPointSet |
| destination success | `all-required`; best-effort는 compliance에 미포함 |
| scheduler catch-up | bounded latest-one, 모든 missed occurrence 기록 |
| restore target | 새 DB 기본, 기존 DB overwrite 별도 승인 |
| SQL Server Wave 1 | BACPAC 후보 검증; DACPAC은 schema-only |
| Redis/Valkey | 복원 runbook과 testbed 전에는 experimental export |
| config cutover | SQLite one-way cutover + TOML export, 구 binary guard |
| agent | Wave 0~4 범위 밖, protocol seam만 유지 |

## 10. Finding Counts

| 등급 | 건수 |
|------|------|
| Critical | 10 |
| Important | 17 |
| Nice-to-Have | 6 |

## Dictionary Updates

| 행위 | 용어 | 정의(제안) | 근거 |
|------|------|------------|------|
| 추가 | Scheduled Occurrence(예약 발생) | `plan_id`, `target_id`, `scheduled_for_utc`로 식별되는 한 번의 예정 실행. 실제 Attempt와 구분하며 유일해야 한다. | DST, 재시작, retry에서도 due 누락·중복을 판정하려면 선언된 일정과 실행 시도를 분리해야 함 |
| 추가 | Attempt(실행 시도) | 하나의 Scheduled Occurrence를 worker가 처리하려고 한 개별 시도. 여러 Attempt가 존재할 수 있으나 committed recovery point는 하나여야 한다. | exactly-once 실행 가정을 제거하고 at-least-once 현실을 모델링 |
| 추가 | Fencing Token(펜싱 토큰) | lease 재발급 때 단조 증가하며 오래된 worker의 heartbeat·완료·commit을 거부하는 세대 번호. | lease 만료만으로 이전 worker의 종료를 증명할 수 없음 |
| 추가 | Recovery Point Set(복구 지점 집합) | 한 Target의 동일 occurrence에 필요한 globals·DB·보조 artifact와 set manifest를 묶은 원자적 복원 단위. | part별 매니페스트가 독립 커밋되면 불완전 PostgreSQL 쌍이 노출됨 |
| 추가 | Destination Copy(목적지 사본) | Recovery Point Set의 특정 Destination 내 ciphertext·manifest·immutability·retention 상태. | 다중 목적지 부분 성공과 복구 가능성을 Snapshot 하나로 표현할 수 없음 |
| 추가 | Required Destination(필수 목적지) | Plan의 내구성·불변성 준수를 위해 성공이 반드시 필요한 Destination membership 역할. | 현재 any-one 성공은 사용자의 2-copy 정책을 거짓 성공으로 만들 수 있음 |
| 추가 | Protection Compliance(보호 준수 상태) | required copy, RPO, immutability, retention, verification freshness를 모두 평가한 Plan/Target 상태. | 실행 성공과 현재 보호됨을 분리해야 false-green 방지 가능 |
| 추가 | Consistency Grade(일관성 등급) | 엔진·토폴로지·옵션에 따라 artifact가 보장하는 시점 일관성 수준과 증적. | MySQL non-transactional, Mongo topology, PG globals 등 엔진 의미가 다름 |
| 추가 | Source Resource Identity(원본 자원 정체성) | alias나 표시 hostname이 아니라 tunnel·agent·canonical endpoint를 반영해 host permit을 적용하는 안정 식별자. | alias를 달리해 동일 DB 서버의 동시성 제한을 우회할 수 있음 |
| 추가 | Spool Reservation(스풀 예약) | dump 시작 전에 특정 볼륨의 예상 사용량을 확보하고 terminal 상태에서 해제하는 resource permit. | 선언된 min free disk만으로 동시 dump의 디스크 고갈을 막을 수 없음 |
| 추가 | Reconciliation(재조정) | queue, process identity, destination object, signed ledger를 비교해 crash 후 authoritative 상태를 결정하는 절차. | `committing` 중 crash는 단순 retry로 안전하게 복구할 수 없음 |
| 추가 | Orphan Artifact(고아 산출물) | 대응하는 complete set manifest가 없거나 유효한 occurrence에 귀속되지 않은 ciphertext·part manifest·local spool. | ciphertext 성공 후 manifest 실패 및 취소 시 비용·보안 상태 관리 필요 |
| 수정 | Snapshot(스냅샷) | UI 검색 표현으로 유지하되, 복원 가능 판정의 원자 단위는 complete Recovery Point Set으로 제한한다. | 현재 정의는 단일 committed artifact와 다중 part 복구 지점을 혼동 |
| 수정 | Lease(실행 임대) | 시간 제한 소유권에 fencing token, attempt ID, worker/process identity, 조건부 완료 규칙을 포함한다. | heartbeat 만료 후 구 worker의 commit을 차단할 수 있어야 함 |
| 수정 | Restore Eligibility(복원 적격성) | 서명·무결성뿐 아니라 complete set, provenance, consistency, engine/tool matrix, target fingerprint, verification freshness를 포함한다. | 유효한 artifact가 곧 안전한 운영 대상 복원을 뜻하지 않음 |
| 수정 | Support Contract(지원 계약) | 각 capability에 데이터 포함 범위, consistency grade, managed restore evidence, 증적 만료를 포함한다. | DACPAC schema-only 같은 명목 지원을 데이터 백업으로 오인할 위험 |
