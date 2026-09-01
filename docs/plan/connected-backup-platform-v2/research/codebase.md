# Codebase Research: Connected Backup Platform v2

## 조사 범위와 판정 기준

- 조사 기준은 기존 `codemap/index.md`와 관련 카테고리 맵, `README.md`, `docs/plan/db-backup-system/`, 최근 원격 복구 핸드오프다.
- v2의 제품 정의는 “여러 DB 서버를 연결·탐색하고, 선택한 데이터베이스를 여러 목적지에 예약 백업하며, 스냅샷·로그·복원을 한 콘솔에서 관리하는 연결형 백업 플랫폼”으로 둔다.
- 현재 보안 기능은 별도 제품 방향이 아니라 연결형 백업 플랫폼의 공통 안전 계층으로 재사용한다.
- 상태 분류:
  - `Implemented`: 실제 실행 경로와 테스트가 연결됨.
  - `Partial`: 핵심 타입이나 하위 기능은 있으나 사용자 흐름 또는 운영 배선이 끊김.
  - `Missing`: 제품 흐름을 제공하는 실행 경로가 없음.

## 현재 제품 경계

현재 코드는 Windows 단일 운영자용 로컬 우선 제품이다. Tauri 데스크톱 콘솔이 TOML 설정과 로컬 SQLite 카탈로그를 읽고, 완성된 Rust CLI를 사이드카로 실행한다. 서버형 control plane, HTTP API, 원격 agent, 사용자·조직·RBAC 경계는 없다.

근거:

- workspace는 코어, 설정, CLI, 보안, 파이프라인, 두 source, 두 원격 destination, catalog, verify, scheduler와 Tauri 앱으로 구성된다. agent/API 서버 크레이트는 없다: `Cargo.toml:2-18`.
- Code Map에는 route와 HTTP API 신호가 모두 0개다: `codemap/index.md`.
- Tauri invoke 표면은 setup, 등록, dashboard, job 실행, restore, schedule로 한정된다: `apps/console/src-tauri/src/lib.rs:9-24`.
- 화면도 dashboard, DB·목적지, 백업 실행, restore, schedule의 다섯 로컬 view다: `apps/console/src/App.tsx:19-27`, `apps/console/src/App.tsx:106-113`.

따라서 v2는 기존 제품을 폐기하는 재작성보다, 현재 보안 백업 엔진을 worker/runtime로 유지하고 그 위에 연결 관리, 중앙 큐, 스냅샷 UX, 추가 엔진 및 선택적 원격 agent를 올리는 확장으로 보는 것이 맞다.

## 기능 현황 매트릭스

| 영역 | 현재 상태 | 코드 근거 | v2에서 필요한 변화 |
|---|---|---|---|
| PostgreSQL 논리 백업 | Implemented | `Engine::Postgresql` 및 `PostgresSource`: `crates/dbbackup-core/src/lib.rs:114-120`, `crates/dbbackup-source-pg/src/lib.rs:160-257` | 서버 연결과 DB 선택을 분리하고 발견 목록을 UI에 노출 |
| MySQL·MariaDB 논리 백업 | Implemented | 엔진 enum과 `MySqlSource`의 버전·테이블 엔진·charset preflight: `crates/dbbackup-core/src/lib.rs:116-120`, `crates/dbbackup-source-mysql/src/lib.rs:124-200` | 서버 단위 DB 탐색 및 다중 DB 선택 |
| SQL Server, MongoDB, SQLite, Firebird, Redis·Valkey | Missing | 엔진 enum이 PostgreSQL/MySQL/MariaDB만 허용: `crates/dbbackup-core/src/lib.rs:114-120` | engine adapter, restore capability, tool packaging, 전용 폼과 E2E 추가 |
| DB 등록 | Partial | Tauri가 alias/host/port/dbname/user/password를 즉시 config에 저장: `apps/console/src-tauri/src/config_edit.rs:26-93` | 서버 연결 프로필과 선택된 DB 대상을 별도 엔터티로 분리 |
| 연결 테스트 | Missing | CLI가 명시적으로 `not implemented yet` 반환: `crates/dbbackup-cli/src/commands/test_connection.rs:8-14` | 실제 인증·권한·버전·도구 호환성 진단을 공통 API로 제공 |
| DB 발견 | Partial | PostgreSQL source 내부 plan은 live DB와 등록 DB를 비교하지만 실행 시점 내부 로직이며 UI API가 아님: `crates/dbbackup-source-pg/src/lib.rs:210-252` | `discover_databases(connection_id)`를 독립 read operation으로 추출 |
| SSH 터널 기반 DB 연결 | Partial | config에는 `Database.tunnel`이 있으나 Tauri 추가 경로는 항상 `None`: `crates/dbbackup-config/src/schema.rs:69-87`, `apps/console/src-tauri/src/config_edit.rs:78-89` | 등록 UI, test/discovery/run 모두 동일 tunnel resolver 사용 |
| Local 목적지 | Implemented | `DestKind::Local`과 `LocalDest` 배선: `crates/dbbackup-core/src/lib.rs:122-128`, `crates/dbbackup-cli/src/commands/run.rs:250-257` | 볼륨 상태·여유 공간·접근성 health 노출 |
| S3 및 Object Lock | Implemented | S3 destination 배선, capability/assert, 분리 reader identity가 존재: `crates/dbbackup-config/src/schema.rs:106-161`, `crates/dbbackup-cli/src/commands/run.rs:258-260` | 연결 self-test를 UI에 연결하고 provider preset 확장 |
| SFTP/SSH 목적지 | Implemented | SSH destination, host-key pinning, 별도 reader source가 존재 | append-only 실제 보장 상태와 연결 health를 운영 UI에 노출 |
| Azure Blob, SMB, FTP 등 | Missing | `DestKind`가 S3/Ssh/Local만 허용: `crates/dbbackup-core/src/lib.rs:122-128` | destination adapter와 credential/test UX 추가 |
| job 모델 | Partial | source 1개, destination N개, GFS retention, verify policy, schedule 보유: `crates/dbbackup-config/src/schema.rs:164-200` | 서버/DB 선택 집합, 실행 우선순위, RPO, resource class 추가 |
| 수동 다중 job 실행 | Implemented, 순차 | 필터된 job을 하나의 runtime에서 `for`로 순차 실행: `crates/dbbackup-cli/src/commands/run.rs:53-60`, `crates/dbbackup-cli/src/commands/run.rs:113-136` | 중앙 dispatcher가 bounded worker pool로 처리 |
| 목적지 fan-out | Implemented, 무제한 fan-out | part마다 `fan_out`을 호출하고 목적지별 부분 성공을 합산: `crates/dbbackup-cli/src/commands/run.rs:395-435`; `crates/dbbackup-pipeline/src/fanout.rs:75-130` | 목적지별 semaphore와 bandwidth/backpressure 정책 |
| 전역 동시성 | Missing | `max_parallel_dumps`는 config→`PipelineCfg`에 복사되지만 실행부에서 읽히지 않는다: `crates/dbbackup-config/src/schema.rs:213-233`, `crates/dbbackup-cli/src/commands/run.rs:287-296`; 전체 참조는 선언·기본값·대입뿐 | job/host/destination/verify/restore별 실제 semaphore와 중앙 큐 |
| scheduler | Partial | job마다 별도 Task Scheduler 항목을 만들고 CLI를 기동: `crates/dbbackup-cli/src/commands/schedule.rs:32-75` | dispatcher 1개 또는 daemon 1개가 due job을 큐잉 |
| 같은 job 중복 방지 | Implemented | Task XML `IgnoreNew`와 catalog overlap guard: `crates/dbbackup-scheduler/src/xml.rs:101-110`, `crates/dbbackup-catalog/src/overlap.rs:76-130` | 서로 다른 job 간 전역 자원 경합까지 확장 |
| 백업 카탈로그 | Implemented core | SQLite가 backup/artifact/verification/run을 기록하고 signed manifest로 rebuild 가능 | catalog를 cache로 유지하되 read model과 pagination/search 추가 |
| 스냅샷 목록·검색·상세 | Missing UI | query는 최신 job별 1건과 최근 N건만 제공: `crates/dbbackup-catalog/src/query.rs:58-90`; dashboard는 job별 최신만 조회: `apps/console/src-tauri/src/commands.rs:154-178` | snapshot inventory, 필터, 상세, artifact/verification timeline |
| 복원 엔진 | Implemented core | Local/S3/SSH manifest source, 서명 게이트, decrypt-only/target restore가 CLI에 연결됨 | catalog-selected restore와 preflight plan을 표준 진입점으로 |
| 복원 UI | Partial | 사용자가 backup ID와 source destination을 직접 입력: `apps/console/src/screens/Restore.tsx:13-35`, `apps/console/src/screens/Restore.tsx:58-108` | snapshot 선택→자격 확인→target 선택→dry-run→확인→실행 |
| L1/L2/L3 검증 | Partial | 서명·무결성·format·Docker restore 구현 및 별도 verify command가 존재 | job의 `verify.schedule/level`을 자동 실행 경로에 연결하고 결과 cadence 관리 |
| Tauri 운영 UI | Partial | setup/dashboard/registration/jobs/restore/schedule 존재 | 서버 상세, DB browser, snapshots, structured logs, health, agent 화면 추가 |
| 원격 agent | Missing | workspace와 Tauri command 표면에 agent가 없음 | 네트워크 격리 DB를 위한 worker protocol, 등록·heartbeat·capability 모델 |
| API·다중 사용자 | Missing | server/router/API crate가 없고 Code Map API 신호 0 | 필요 시 desktop core와 분리된 control-plane API 및 RBAC 설계 |

## 영역별 상세

### 1. 데이터베이스 엔진과 연결 모델

`BackupSource`가 `plan()`과 `spawn(part)`를 분리해 새 엔진 추가의 기본 seam은 좋다. PostgreSQL은 globals와 DB part를 구분하고, 서버의 live DB 목록과 등록 목록을 비교한다. MySQL/MariaDB는 버전 분기, client compatibility, non-transactional table, charset/collation, binlog anchor를 캡처한다.

하지만 config의 `Database`는 “서버 연결”과 “선택된 단일 DB”를 하나로 합친 구조다: `crates/dbbackup-config/src/schema.rs:69-87`. 실행부도 PostgreSQL `registered_databases`에 단 하나의 `dbname`만 전달하고 MySQL connection에도 하나의 DB를 고정한다: `crates/dbbackup-cli/src/commands/run.rs:305-360`.

v2에서는 다음 경계를 분리해야 한다.

```text
ConnectionProfile
  id, engine, host, port, auth, tls, tunnel/agent, discovered_version

DatabaseTarget
  id, connection_id, database_name, include/exclude, engine_options

BackupPlan
  id, database_target_ids, destination_ids, schedule, retention, verification
```

이 분리는 같은 서버의 DB 수가 늘 때 자격증명을 중복 저장하지 않고, 연결 테스트와 DB 발견 결과를 재사용하게 한다. 기존 config v1은 migration에서 `Database` 한 건을 `ConnectionProfile + DatabaseTarget` 한 쌍으로 변환할 수 있다.

### 2. 연결 테스트와 발견

가장 큰 제품 공백이다. CLI 명령 이름과 UI 입력 폼은 있지만 실제 `test-connection`은 stub이다. Tauri invoke 목록에도 test/discovery command가 없고, `add_database`는 네트워크 검증 없이 암호화 저장부터 수행한다.

기존 source의 preflight 로직은 재사용하되 백업 실행에 종속시키지 말아야 한다.

- 공통 `ConnectionAdapter`:
  - `test_connection()`
  - `probe_server()`
  - `discover_databases()`
  - `check_backup_permissions()`
  - `check_restore_permissions()`
- PostgreSQL은 현재 admin connection, version query, database catalog query를 추출한다.
- MySQL/MariaDB는 현재 meta seam을 확장해 `SHOW DATABASES`와 system schema 필터를 제공한다.
- tunnel/agent resolution은 test, discovery, backup, restore가 하나의 resolver를 사용해야 한다.

### 3. 목적지

현재 목적지 계층은 v2에서도 가장 재사용 가치가 높다. `Destination` capability와 required immutability assert, commit/abort protocol, 부분 성공 fan-out, writer/reader identity 분리는 유지한다. 특히 catalog rebuild와 restore가 writer credential을 재사용하지 않는 최근 구현은 control-plane 손실 복구의 기반이다.

확장 시 destination 종류를 enum match에 계속 추가하기보다 등록 가능한 adapter descriptor/capability로 전환하는 것이 안전하다. 그래도 v2 첫 단계는 과도한 동적 플러그인보다 정적 Rust adapter가 적합하다.

권장 우선순위:

1. Local/NAS 경로 health 및 SMB 경로 호환
2. S3-compatible provider preset
3. Azure Blob
4. SFTP 유지
5. FTP는 보안 정책상 명시적 opt-in 또는 후순위

### 4. jobs, scheduler, concurrency

Rust 자체의 thread 처리 능력보다 현재 orchestration이 병목이다.

- 한 CLI 실행에서 여러 job은 순차다.
- 한 PostgreSQL plan의 globals와 DB parts도 `for part in &parts`로 순차다: `crates/dbbackup-cli/src/commands/run.rs:365-435`.
- 각 part 내부 목적지 fan-out만 concurrent다.
- 예약은 job마다 별도 Windows task와 별도 `dbbackup.exe` 프로세스를 만든다.
- `IgnoreNew`와 overlap guard는 같은 job 중복만 막는다. 같은 시각의 서로 다른 job은 각자 runtime, dump process, spool, upload를 만들 수 있다.
- `max_parallel_dumps`는 현재 비활성 설정이다. 이름만 보고 이미 제한된다고 가정하면 안 된다.

v2의 실행 구조:

```text
Scheduler trigger / manual request / API request
                    |
                    v
              Persistent Job Queue
                    |
          Global Dispatcher / leases
          /          |            \
      host gate   local CPU gate   destination gate
          \          |            /
              bounded workers
                    |
       dump -> encode -> fan-out -> verify
```

필요한 최소 제어:

- `max_concurrent_jobs`
- `max_dumps_per_connection`
- `max_cpu_encoders`
- `max_uploads_per_destination`
- `max_concurrent_l3_verifications`
- `max_concurrent_restores` 기본 1
- spool disk reservation
- RPO/priority 기반 queue ordering
- cancellation, lease heartbeat, crash recovery

기존 `run_scheduled_job`, catalog `runs`, `RunGuard`, notification sink는 worker 실행 결과 표준화에 재사용한다. Task Scheduler는 모든 job을 직접 실행하기보다 dispatcher/daemon 시작과 깨우기 역할로 축소하는 것이 맞다.

### 5. 스냅샷과 카탈로그

현재 catalog는 보안 경계가 잘 잡혀 있다. SQLite는 표시용 cache이며 복원 신뢰 판단은 signed remote manifest를 다시 검증한다. 이 원칙은 유지해야 한다.

제품 공백은 read model과 UI다.

- backup별 artifact, destination, checksum, retention, verify history는 schema에 존재하지만 화면은 job별 최신 백업 위주다.
- 검색, engine/server/database/status/date/verification/immutability 필터가 없다.
- snapshot detail, 단계별 로그, 목적지별 receipt, retention 만료일이 없다.
- 복원 화면이 catalog를 소비하지 않아 backup ID를 수동 입력한다.

v2 catalog API는 최소한 다음을 제공해야 한다.

- paged snapshot list + filters
- snapshot detail
- artifact/destination state
- verification timeline
- execution phase logs
- restore eligibility
- manifest refresh/reconcile

SQLite는 desktop 단일 노드 read model로 계속 적합하다. 다중 agent/API로 확장할 때는 local agent catalog와 control-plane aggregate catalog의 소유권을 별도 ADR로 결정해야 한다.

### 6. restore와 verification

핵심 보안 엔진은 유지 대상이다.

- unsigned/tampered manifest를 복호화 전에 거부
- 별도 reader identity로 remote manifest와 object 읽기
- L1 file integrity, L2 format validity, L3 sandbox restore
- recovery code와 decrypt-only 경로
- catalog rebuild 후 복원 가능

다만 `Job.verify`의 level/schedule은 설정과 UI에 있으나 백업 완료 후 자동 검증을 실행하는 배선은 확인되지 않았다. `run.rs`는 upload와 catalog upsert 후 job result를 반환하고 verify를 호출하지 않는다: `crates/dbbackup-cli/src/commands/run.rs:365-439`. verification cadence 모듈도 독립 테스트 중심이다. 이를 “자동 검증 완료”로 표시하면 안 된다.

복원 UX는 수동 backup ID 입력 대신 다음 흐름으로 바꾼다.

1. snapshot 선택
2. signed manifest 재조회·검증
3. artifact/reader/target preflight
4. restore plan과 위험 영향 표시
5. dry-run 또는 decrypt-only 선택
6. 명시적 확인
7. 실행 단계와 결과 기록

### 7. Tauri UI

현재 UI는 보안 setup, 암호화 등록, job 구성, 실행 로그, restore 위험 확인, schedule 등록까지 연결되어 있어 폐기 대상이 아니다. 사이드카 stdout/stderr를 `run:log`와 `run:done` 이벤트로 스트리밍하는 패턴도 장시간 작업 UI에 적합하다.

v2 정보 구조는 다음으로 확장하는 것이 자연스럽다.

```text
Dashboard
Connections
  Servers
  Databases
Destinations
Backup Plans
Snapshots
Restores
Activity / Logs
Agents
Settings
```

Databasement에서 흡수할 핵심은 시각 스킨보다 연결, 서버 상세, DB 탐색, snapshot inventory, structured job logs, 선택 기반 restore 흐름이다. 현재 `DESIGN.md`의 보안 콘솔 시각 체계와 서명·불변성·검증 badge는 유지한다.

### 8. 원격 agent와 API

현재 SSH는 destination 및 tunnel 전송 기술이지 원격 backup agent가 아니다. dump tool은 콘솔 PC에서 실행되며 해당 PC가 DB 네트워크와 도구 버전에 접근할 수 있어야 한다.

원격 agent가 필요한 경우:

- DB가 사설망에 있고 중앙 PC가 직접 접근할 수 없음
- dump를 DB 근처에서 실행해 WAN 전송량을 줄여야 함
- Linux 전용 DB 도구 또는 engine별 runtime이 필요함
- 수십·수백 DB의 작업을 여러 노드로 분산해야 함

agent v1 계약은 작게 시작해야 한다.

- register/approve/revoke
- heartbeat와 capabilities
- leased job 수신
- local dump/encode 또는 dump-only
- signed progress/result event
- version/tool inventory
- cancellation과 lease expiry

HTTP API와 multi-user control plane은 desktop v2의 필수 전제가 아니다. 먼저 내부 application service interface를 Tauri command와 CLI가 공유하게 만들고, 이후 동일 인터페이스에 API adapter를 붙이는 순서가 재작성 위험을 낮춘다.

## 재사용할 기존 패턴

1. `BackupSource::plan/spawn`과 `DumpPlanPart` 경계
2. engine별 version probe 및 bundled binary matrix
3. `Destination` capability와 commit/abort protocol
4. 목적지 fan-out의 부분 성공 결과 모델
5. backup별 DEK, signed manifest, 복호 전 signature gate
6. writer와 recovery reader identity 분리
7. SQLite single-writer와 signed-manifest rebuild
8. `run_scheduled_job`의 overlap/result/notification 표준화
9. stream→hash→compress→encrypt→spool commit 파이프라인
10. L1/L2/L3 verification과 sandbox restore
11. config migration, deny-unknown validation, encrypted field 저장
12. Tauri sidecar event streaming과 장시간 작업 상태 패턴

Patterns: 12

## 주요 위험

1. `max_parallel_dumps`가 동작한다고 오인해 대규모 동시 예약을 허용할 위험
2. job별 프로세스/runtime 구조가 CPU thread, dump process, disk, network를 무제한 경쟁시키는 위험
3. server와 database가 결합된 config를 그대로 확장해 자격증명·health·discovery 상태가 중복되는 위험
4. 연결 테스트 stub 상태에서 “등록 성공”을 운영 가능으로 오인하는 위험
5. tunnel 필드는 있으나 UI와 source 실행 경로의 일관된 resolver가 없는 위험
6. verify/retention policy가 저장됐다는 이유로 자동 시행된 것으로 표시할 위험
7. catalog read model 부족으로 사용자가 복원 가능한 snapshot을 찾지 못하는 위험
8. 새 engine을 enum match로만 추가하다 config, manifest, toolchain, restore, sandbox, UI가 서로 어긋나는 위험
9. remote agent/API를 성급히 추가해 인증, lease, secret 소유권, catalog truth가 모호해지는 위험
10. 기존 ignored/infrastructure-dependent L2/L3 테스트를 모든 환경의 실제 복원 보장으로 과대평가할 위험

Risks: 10

## v2 계획에 반영할 권장 경계

### 반드시 먼저

1. `ConnectionProfile`과 `DatabaseTarget` 분리 및 v1 config migration
2. engine 공통 test/probe/discover interface
3. 중앙 dispatcher와 실제 semaphore/resource budget
4. snapshot catalog read API와 선택 기반 restore
5. verify cadence와 retention 실행 배선

### 그다음

1. 서버·DB·snapshot·activity 중심 Tauri UI
2. SQL Server, MongoDB, SQLite 순의 engine adapter 후보 검증
3. Azure Blob/SMB 등 destination 확장
4. 원격 agent protocol
5. 선택적 HTTP API, 조직/RBAC

## 후속 확인 필요

- 현재 변경 작업이 아직 커밋되지 않은 큰 dirty worktree다. 구현 단계에서는 기존 변경을 기준선으로 보존하고 새 변경을 논리적으로 분리해야 한다.
- 실제 `cargo test --workspace`, ignored L2/L3, live S3/SSH 결과는 별도 검증 트랙에서 최신 상태를 다시 확인해야 한다.
- PostgreSQL/MySQL 실제 연결 discovery와 권한 진단은 live fixture가 필요하다.
- v2에 우선 추가할 DB 엔진 순서는 사용자 환경의 실제 보유 DB 수, 크기, RPO/RTO, OS를 받아 확정해야 한다.
- agent와 API가 v2 필수인지, desktop-first 이후 단계인지 제품 경계 결정이 필요하다.

Follow-up: yes
