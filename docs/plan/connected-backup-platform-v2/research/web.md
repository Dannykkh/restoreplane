# Connected Backup Platform v2 — Web Research

- 조사일: 2026-07-26
- 범위: 3개 주제, 주제당 공식·1차 출처 2개
- 목적: 다중 데이터베이스 연결·백업 플랫폼의 실행 모델, 엔진 어댑터 범위, 운영 안전장치를 v2 계획에 반영
- 주의: Databasement 공식 문서는 해당 제품이 실제로 선언한 지원 범위의 1차 출처다. 각 DB 공급자가 보장하는 전체 호환성 표를 대신하지 않으므로, 출시 전 엔진별 공급자 문서와 실제 E2E로 다시 인증해야 한다.

## 1. Tokio 외부 덤프 프로세스와 CPU 작업의 제한된 동시성

### 공식 출처

1. [Tokio `process` module](https://docs.rs/tokio/latest/tokio/process/)
2. [Tokio `spawn_blocking`](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)

### 확인된 사실

- `tokio::process::Command`는 Windows와 Unix의 OS 프로세스를 비동기적으로 시작하고 종료를 기다릴 수 있게 한다. `pg_dump`, `mariadb-dump` 같은 덤프 도구의 실제 CPU·메모리·I/O 부담은 별도 OS 프로세스가 지며, Tokio task 자체의 가벼움과는 구분해야 한다.
- `Child` 핸들을 drop해도 자식 프로세스는 기본적으로 계속 실행된다. 취소·타임아웃 경로에서 `kill_on_drop(true)`를 설정하고, 가능한 경우 명시적으로 종료한 뒤 `wait`하여 프로세스 수거와 임시 파일 정리를 완료해야 한다.
- 파이프의 입력·출력을 잘못 기다리면 자식과 부모가 서로 대기하는 교착이 생길 수 있다. stdout/stderr는 실행 중 비동기적으로 소비하고, 출력 전체를 무제한 메모리에 모으지 않아야 한다.
- `spawn_blocking`의 기본 blocking thread 상한은 CPU 작업용으로는 매우 크다. Tokio 공식 문서도 CPU-bound 작업이 많을 때 semaphore 등으로 병렬 실행 수를 제한하거나 Rayon 같은 전용 실행기를 사용하라고 안내한다.
- 이미 시작된 `spawn_blocking` 작업은 일반적인 task abort로 중단할 수 없다. 압축·암호화가 긴 단일 호출이면 종료가 지연될 수 있으므로 작업을 유한한 단계로 만들고, 시작 전 permit과 취소 상태를 확인해야 한다.

### v2 계획에 미치는 영향

#### 단일 중앙 dispatcher

잡마다 독립적인 다중 스레드 Tokio runtime과 `restoreplane.exe`를 동시에 띄우는 구조 대신, 하나의 dispatcher가 예약 트리거와 수동 요청을 영속 큐에 넣고 제한된 worker가 실행한다.

```text
Scheduler / UI / CLI
        |
        v
Persistent bounded queue
        |
        v
Central dispatcher
  | global job permits
  | per-host permits
  | CPU permits
  | destination permits
        |
        v
Native dump process -> compress/encrypt -> upload -> verify
```

#### 계층별 permit

| 제한 | 초기 기본값 | 이유 |
|---|---:|---|
| `max_concurrent_jobs` | 2 | 로컬 디스크·메모리·네트워크 총량 보호 |
| `max_dumps_per_host` | 1 | 동일 운영 DB 서버에 덤프 부하가 몰리는 상황 방지 |
| `max_cpu_stages` | `max(1, min(2, logical_cpu / 2))` | 압축·암호화가 Tokio worker와 시스템 전체를 점유하지 않게 제한 |
| `max_uploads_per_destination` | 2 | S3/SFTP/로컬 볼륨별 처리량과 연결 수 보호 |
| `max_concurrent_restores` | 1 | 파괴적 복원의 동시 실행 방지 |

초기값은 안전한 출발점이며, 벤치마크와 사용자 장비 프로파일을 통해 조정한다. permit은 프로세스 시작 직전이 아니라 해당 자원을 쓰는 전체 단계 동안 유지한다. 예를 들어 host permit은 덤프 종료까지, destination permit은 업로드와 원격 검증 종료까지 유지한다.

#### 프로세스 수명주기

1. 잡 lease와 전역·host permit 확보
2. 도구 경로, 버전, 서버 호환성 preflight
3. 비밀을 명령줄에 넣지 않고 환경 변수·stdin·권한 제한 임시 설정으로 전달
4. `kill_on_drop(true)`와 단계별 timeout을 설정해 프로세스 시작
5. stdout/stderr를 제한된 버퍼 또는 파일 스트림으로 지속 소비
6. 성공 여부와 종료 코드를 기록하고 반드시 wait
7. 실패·취소 시 자식 종료, 부분 산출물 격리, permit 반환

#### CPU 단계

- 동기 zstd·암호화 API를 유지한다면 CPU semaphore를 획득한 후 `spawn_blocking` 또는 전용 Rayon pool에서 실행한다.
- 가능하면 dump → compression → encryption → destination의 bounded streaming pipeline으로 디스크와 메모리 피크를 낮춘다.
- 취소는 `spawn_blocking` abort에 의존하지 않는다. chunk 경계의 cooperative cancellation 또는 중단 가능한 스트리밍 API를 채택한다.

### 수용 기준

- 2·4·8·16개 잡 동시 예약 부하에서 설정된 process·CPU·upload 상한을 넘지 않는다.
- 잡 취소와 timeout 후 덤프 프로세스, 열린 파이프, 스풀 파일이 남지 않는다.
- 느린 host 또는 destination 하나가 다른 host 잡의 실행 permit을 독점하지 않는다.
- queue depth, oldest job age, active permits, process duration, CPU-stage wait time을 계측한다.

## 2. 다중 DB 네이티브 백업·복원 도구와 호환성 경계

### 공식 출처

1. [Databasement v1.6 — Database Servers](https://david-crty.github.io/databasement/user-guide/database-servers/)
2. [Databasement v1.6 — Backups](https://david-crty.github.io/databasement/user-guide/backups/)

### 확인된 기준선

Databasement v1.6은 표준 CLI 기반으로 다음 범위를 선언한다.

| 엔진 | Databasement 기준 도구·형식 | 문서상 범위 | 복원 | v2에서 반드시 명시할 경계 |
|---|---|---|---|---|
| PostgreSQL | `pg_dump` v18 | 12–18 | 지원 | plain SQL과 archive format의 복원 경로를 구분하고 client/server 조합을 E2E 인증 |
| MySQL | `mariadb-dump` | 5.6, 5.7, 8.x, 9.x | 지원 | MariaDB client가 MySQL의 모든 객체·인증·신규 문법을 보존한다고 가정하지 않고 major별 검증 |
| MariaDB | `mariadb-dump` | 10.x–12.x | 지원 | server/client major, routine, event, trigger, view, binary data를 포함한 round-trip 검증 |
| SQL Server | `sqlpackage` Extract/Publish, `.dacpac` | 2017, 2019, 2022, Azure SQL | 지원 | `.dacpac`은 DB 엔진의 전체 `.bak`과 의미가 다르며 login·permission·role 같은 server-bound object가 제외됨 |
| MongoDB | `mongodump` / `mongorestore`, archive | 4.2–8.0 | 지원 | dump tool·source server·target server 조합을 기록하고 인증된 조합 외 restore를 차단 또는 경고 |
| SQLite | `sqlite3 .backup` 또는 문서 다른 구간의 file copy | 3.x | 지원 | 실행 중 raw file copy를 기본으로 삼지 말고 `.backup`/Backup API 또는 quiesce+WAL 처리 경로 사용 |
| Firebird | `gbak` v5, `.fbk` | 3.x–5.x | 지원 | 경로 기반 DB이며 `gbak -rep`가 기존 파일을 교체하므로 명시적 파괴 확인과 격리 복원 필요 |
| Redis | `redis-cli --rdb` | 2.8+ | 미지원 | 전체 인스턴스 RDB 백업만 v2 초기 범위로 두고 restore는 운영 중단·데이터 디렉터리 제어가 가능한 agent 이후로 연기 |
| Valkey | `redis-cli --rdb` | 7.2+ | 미지원 | Redis와 별도 adapter capability와 호환성 테스트를 유지하고 restore는 초기 비활성화 |

### 중요한 해석

- DB 이름을 설정 화면에 추가하는 것만으로는 “지원”이 아니다. `연결 테스트 → 버전 감지 → DB 탐색 → 권한 preflight → dump → 암호화·업로드 → 무결성 검증 → 격리 restore → 데이터 검증` 전체가 통과한 조합만 지원 상태가 된다.
- Databasement의 SQL Server 방식은 portable logical package에 가깝다. v2의 첫 SQL Server 지원은 `.dacpac` 패리티로 시작할 수 있지만, UI와 문서에 “전체 인스턴스/로그인/권한/트랜잭션 로그 백업이 아님”을 분명히 표시해야 한다. 향후 native `.bak`은 DB 서버 파일시스템에 접근 가능한 remote agent 모드로 별도 제공하는 것이 안전하다.
- Databasement 공식 문서 안에서도 SQLite가 “file copy”와 `sqlite3 .backup`으로 다르게 표현된다. v2는 일관성이 보장되는 `.backup` 또는 Backup API를 정본으로 선택하고, 원격 raw copy는 DB 정지와 WAL 동반 조건을 만족할 때만 고급 모드로 허용해야 한다.
- Redis/Valkey는 백업 파일 생성과 안전한 서비스 복원이 같은 문제가 아니다. 초기 릴리스에서 backup-only capability를 정확히 표시하는 것이 잘못된 복원 버튼을 제공하는 것보다 낫다.
- Databasement 예시처럼 password를 CLI 인자로 넘기면 process list와 로그에 노출될 수 있다. v2 adapter contract는 비밀 전달 채널과 redaction을 공통 정책으로 강제한다.

### 공통 adapter contract

모든 엔진 provider는 다음 인터페이스와 증거를 제공해야 한다.

```text
detect_tool()
probe_server()
discover_databases()
preflight_backup()
plan_backup()
run_backup()
verify_artifact()
plan_restore()
run_restore()
validate_restored_database()
```

manifest에는 최소 다음 필드를 서명해 저장한다.

- engine, source server version, source feature/compatibility mode
- dump tool 이름·정확한 버전·플랫폼
- dump format과 compression/encryption format
- 포함·제외된 객체 범위
- restore 지원 여부와 인증된 target version 범위
- agent 필요 여부, 파괴적 restore 여부
- checksum, manifest signature, L1/L2/L3 결과

### 단계별 지원 순서

1. PostgreSQL·MySQL·MariaDB: 기존 구현을 adapter contract에 맞추고 major별 E2E를 완성
2. SQL Server `.dacpac`·SQLite `.backup`: Windows 주력 사용자 가치가 높고 도구 경계가 비교적 명확
3. MongoDB·Firebird: archive/path 기반 restore와 버전 매트릭스 추가
4. Redis·Valkey: backup-only로 출시하고 agent 기반 격리 restore가 검증된 뒤 복원 capability 활성화
5. SQL Server `.bak`: remote agent와 서버 측 저장·권한 모델이 완성된 뒤 별도 모드로 추가

### 출시 게이트

- 지원한다고 표시하는 각 major version에 container 또는 VM 기반 backup→restore E2E가 존재한다.
- schema, row count, 대표 데이터 hash, routine/view/trigger/index 등 엔진별 중요 객체를 검증한다.
- source/target/tool 조합이 인증 표 밖이면 기본적으로 restore를 차단하고 override에는 위험 확인과 감사 로그가 필요하다.
- DB 공급자별 공식 문서 교차검증과 실제 도구 matrix는 구현 직전 adapter별 후속 조사로 수행한다.

## 3. 다중 DB 큐 운영, backpressure, host 격리, 복원 훈련

### 공식 출처

1. [Amazon Builders’ Library — Avoiding insurmountable queue backlogs](https://d1.awsstatic.com/builderslibrary/pdfs/avoiding-insurmountable-queue-backlogs.pdf)
2. [AWS Well-Architected REL09-BP04 — Perform periodic recovery](https://docs.aws.amazon.com/wellarchitected/latest/framework/rel_backing_up_data_periodic_recovery_testing_data.html)

### 확인된 사실

- 큐는 장애 동안 요청을 보존하지만, 입력률이 처리율보다 오래 높으면 backlog 자체가 복구 시간을 늘리는 실패 요인이 된다.
- AWS는 workload 분리, workload별 thread/concurrency 제한, semaphore, queue depth 기반 backpressure, delay queue, 오래된 작업의 분리, in-flight 상한을 사용한다.
- 하나의 workload가 느린 외부 의존성 때문에 concurrency를 모두 소비하지 않게 격리해야 한다. 이 프로젝트에서는 DB host와 destination을 각각 workload/resource key로 매핑할 수 있다. 이는 출처의 원칙을 백업 도메인에 적용한 설계 추론이다.
- 백업 존재 여부나 restore 명령 성공만으로 복구 가능성을 증명할 수 없다. AWS는 운영과 같은 절차로 새 위치에 주기적으로 복원하고, 데이터 접근성·무결성·RPO를 확인하며 실제 RTO를 측정하라고 권고한다.

### v2 운영 모델

#### 영속 잡 상태

```text
queued -> leased -> running -> succeeded
                    |       -> failed_retryable -> delayed
                    |       -> failed_terminal
                    |       -> canceled
                    -> lease_expired -> queued
```

각 잡에는 `idempotency_key`, `attempt`, `lease_owner`, `lease_expires_at`, `scheduled_for`, `priority`, `resource_keys`, `next_retry_at`을 둔다. 프로세스가 죽어도 lease 만료 후 안전하게 재개하되, 이미 생성된 객체와 snapshot manifest를 확인해 중복 업로드를 방지한다.

#### workload 격리

- `host:{server_id}`: 동일 서버 dump 동시성
- `destination:{destination_id}`: 업로드·검증 동시성
- `cpu:{agent_id}`: 압축·암호화 동시성
- `database:{server_id}:{database_id}`: 동일 DB backup/restore 충돌 방지
- `restore:{agent_id}`: agent별 파괴적 작업 단일 실행

전역 permit만 두지 않고 resource key별 permit을 함께 사용한다. 느린 SFTP 한 곳이나 과부하된 DB 서버 하나가 다른 서버의 백업을 모두 멈추지 않게 한다.

#### queue와 backpressure

- 예약 백업, 수동 백업, 복원·복원 훈련을 최소한 별도 lane으로 분리하고 각 lane에 상한과 가중치를 둔다.
- 같은 job의 예약 tick이 이미 queued/running이면 동일 의도를 계속 쌓지 않고 coalesce한다. 단, 조용히 폐기하지 않고 `missed_run`과 RPO breach를 기록한다.
- 재시도 가능한 네트워크·일시 오류만 exponential backoff와 jitter로 재시도한다. 인증·권한·호환성 오류는 즉시 terminal로 보내 운영자 수정 전 재시도 폭주를 막는다.
- queue depth뿐 아니라 oldest job age와 예상 RPO 위반 시각을 backpressure 신호로 사용한다.
- 수동 요청이 상한을 넘으면 즉시 실패시키기보다 예상 대기와 원인을 UI에 보여준다. 디스크 안전선 미달처럼 실행 자체가 위험한 경우에만 enqueue를 거부한다.

#### 우선순위와 공정성

초기 우선순위는 `사용자 승인 복원/긴급 백업 > RPO 임박 예약 백업 > 일반 예약 백업 > L3 훈련`으로 두되, aging으로 낮은 우선순위의 무기한 starvation을 막는다. 복원은 우선순위가 높아도 동일 DB의 실행 중 dump가 안전 지점에 도달한 뒤 exclusive permit을 얻어야 한다.

### 자동 복원 훈련

1. 실제 운영 restore adapter와 실제 snapshot catalog에서 대상 선택
2. production이 아닌 격리된 임시 target 생성
3. 서명·checksum·도구·버전 compatibility preflight
4. 동일한 복호화·복원 경로 실행
5. 엔진별 validation query, row count, schema/object 검사, 대표 데이터 hash 수행
6. snapshot 시점과 복원 데이터로 RPO 판정
7. 시작부터 검증 완료까지 측정해 RTO 판정
8. 성공·실패 증거와 로그를 서명된 drill record로 보관
9. 임시 target과 임시 비밀을 정리하고 실패 시 알림

L1은 파일·서명 검증, L2는 도구가 artifact를 읽고 구조를 검사하는 검증, L3는 격리 DB에 실제 복원하고 쿼리까지 통과하는 훈련으로 유지한다. 대시보드의 핵심 지표는 단순 backup success rate가 아니라 “RPO 내 복구 가능한 snapshot”, “최근 L3 성공 시각”, “측정 RTO”, “복원 훈련 연체”가 되어야 한다.

### 관측성과 검증

- queue: depth, oldest age, enqueue/complete rate, delayed/DLQ 수
- resource: global/host/destination/CPU permit 사용률과 대기 시간
- process: engine/tool/version별 duration, timeout, exit code
- reliability: missed run, RPO breach, retry count, orphan cleanup
- restore: L1/L2/L3 최근 결과, measured RTO/RPO, validation failure reason
- 부하 테스트: 느린 host, 느린 destination, 디스크 부족, 프로세스 hang, dispatcher crash, 동일 예약 폭주를 주입하고 다른 host의 처리와 queue 복구를 확인

## 종합 계획 결정

1. Rust/Tokio는 다중 DB 관리에 적합하지만, 확장성의 핵심은 thread 수가 아니라 중앙 dispatcher와 계층별 bounded concurrency다.
2. Databasement의 엔진·도구 범위를 v2 제품 기준선으로 채택하되, 명칭 패리티가 아니라 adapter별 실제 restore E2E를 지원 정의로 삼는다.
3. SQL Server `.dacpac`, SQLite 일관성, Redis/Valkey restore는 의미론적 경계를 UI와 capability model에 명시한다.
4. 예약 요청은 영속 큐로 수용하되 중복 tick을 coalesce하고 missed run/RPO breach를 남긴다.
5. 같은 운영 restore 경로를 사용하는 주기적 L3 훈련을 출시 품질 게이트와 대시보드 핵심 지표로 둔다.

## 후속 조사 필요

필요하다. 구현 순서가 확정될 때 각 engine adapter별로 공급자 공식 문서를 다시 조사해 source server × dump tool × target server 호환성 표, 최소 권한, destructive semantics, Windows 배포 가능한 바이너리와 라이선스를 확정해야 한다.
