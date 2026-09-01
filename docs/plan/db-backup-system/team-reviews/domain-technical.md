# Domain Technical Review — Backup/Restore Correctness

역할: DB 백업/복원 정합성(correctness) 전문가. 목표는 "백업은 있는데 복원이 안 되는" 최악의
시나리오를 설계 단계에서 차단하는 것. spec.md와 research/rust-stack.md를 전제로 검토함.

이 문서는 우선순위(🔴 필수/🟠 중요/🟡 권장) 순으로 정렬된 correctness 발견사항 + 백업
매니페스트 필드 목록 + 딕셔너리 업데이트 제안으로 구성됨.

---

## 요약 (Top 5, 반드시 v1에 반영)

1. **PostgreSQL 글로벌(role/tablespace) 미백업 시 복원 실패** — `pg_dump`는 DB당 객체만 덤프하고
   role/tablespace는 클러스터 전역 객체라 포함 안 됨. `pg_dumpall --globals-only`를 매 백업 실행마다
   (또는 최소 1일 1회) 별도로 떠서 함께 보관하지 않으면, 다른 클러스터/재설치 환경에서 복원 시
   `role "app_user" does not exist` 로 실패함.
2. **덤프 포맷은 `custom`(`-Fc`)을 기본값으로** — `plain`은 스트리밍 파이프엔 맞지만 부분 선택 복원
   불가·병렬 복원 불가·`pg_restore --list` 검증 불가. `directory`(`-Fd`)는 병렬(`-j`)에 좋지만
   단일 stdout 스트림이 아니라서 spec의 `pipe → zstd → age → dest` 파이프라인과 안 맞음(디렉토리
   출력이라 tar로 말아야 함). **stdout 파이프 + 사후 검증 가능성을 동시에 만족하는 건 `-Fc`.**
3. **`mysqldump --single-transaction`은 MyISAM/DDL에 대해 컨센시 보장이 없음** — 이 프로젝트는
   "개인/소규모 인프라"지만 레거시 MyISAM 테이블(특히 `mysql.*` 시스템 카탈로그 일부, 오래된
   워드프레스류 앱)이 섞여 있으면 조용히 비일관 덤프가 만들어짐. 사후에 알 방법이 없음 —
   덤프 전에 엔진 스캔을 해서 사용자에게 경고해야 함.
4. **버전 호환성은 "설치 시점"이 아니라 "매 백업 실행 시점"에 재검증해야 함** — 서버가 마이너/메이저
   업그레이드되면 번들된 클라이언트가 낡아질 수 있음. spec의 Risk Assessment에 "실행 전 서버 버전
   감지 후 클라이언트 선택"이 있지만, 이건 매 `run` 마다 수행되는 런타임 체크여야지 `init` 1회성
   설정이 아님.
5. **매니페스트(백업 메타데이터) 없이는 논리 덤프가 "복원 가능한 백업"이 아니라 "바이트 뭉치"임** —
   서버 버전, 덤프 도구 버전, 인코딩, 덤프 플래그, DB 이름, 포맷을 덤프 본체와 별도로(그리고
   같은 불변 목적지에) 저장하지 않으면, 몇 달 뒤 재해복구 시점에 "이걸 어떤 pg_restore로 풀어야
   하는지" 자체를 알 수 없음. catalog(SQLite)가 로컬에만 있으면 랜섬웨어/장비 전손 시 함께 사라짐 —
   매니페스트는 반드시 백업 오브젝트와 **같은 목적지**에 동봉되어야 함.

---

## 1. PostgreSQL

### 1.1 덤프 포맷 선택 (plain vs custom vs directory)

| 포맷 | stdout 스트리밍 | 부분/선택 복원 | 병렬 복원 | 사전 검증(`--list`) | 압축 |
|---|---|---|---|---|---|
| `plain` (`-Fp`, SQL 텍스트) | O (자연스러움) | X (전체 replay만) | X | X | 없음(zstd가 담당) |
| `custom` (`-Fc`) | O (`-f -`) | O (`pg_restore -t table`) | O (`pg_restore -j`) | **O** (`pg_restore --list`) | 자체 압축 내장하지만 0으로 끄고 zstd에 맡기는 게 나음 |
| `directory` (`-Fd`) | **X** (디렉토리 출력, 파일당 병렬 write) | O | O (덤프도 `-j`, 복원도 `-j`) | O | 파일별 |

- **spec의 파이프라인**(`dump stdout → zstd::Encoder → age StreamWriter → Destination writer`)은
  단일 stdout 스트림을 전제로 함 → `directory` 포맷은 이 파이프라인과 구조적으로 안 맞음(디렉토리를
  tar로 말면 되지만, 그러면 `pg_restore -j` 병렬 복원의 이점도 사라짐).
- 🔴 **권고: 기본값 `custom`(`-Fc -f -`)**. `plain`은 "사람이 읽을 수 있어야 한다"는 명시 요구가
  있을 때만 옵션으로 제공. `pg_restore --list`로 목차만 읽어 검증할 수 있다는 게 custom의 결정적
  장점 — verify 단계(§4)에서 전체 복원 없이도 "이 파일이 손상되지 않았다"를 빠르게 확인 가능.
- **대용량 DB에서 병렬 복원이 필요해지면** `directory` + tar 래핑을 v1.x로 고려. v1은 custom으로 시작.

### 1.2 글로벌 객체 (roles, tablespaces) — `pg_dumpall --globals-only`

- `pg_dump`는 **한 DB 안의 객체만** 덤프한다. Role/Tablespace는 클러스터 전역(`pg_catalog`가 아니라
  `pg_authid` 등 공유 카탈로그)이라 `pg_dump` 결과물엔 **절대** 안 들어감.
- 복원 시 덤프 안의 `OWNER TO app_user` / `GRANT ... TO app_user` 구문이 role을 못 찾아 에러.
  `--no-owner --no-acl`로 회피할 수도 있지만, 그러면 권한 구조 자체가 유실됨(운영 목적의 백업이면
  보통 권한도 복원하고 싶어함).
- 🔴 spec.md의 아키텍처 다이어그램은 `BackupSource : trait { PostgresSource, MySqlSource }`로 DB당
  1개 소스만 표현하고 있음 — **클러스터 레벨 globals 덤프가 어느 소스에도 안 붙어 있음.** 설계에
  "PostgreSQL 서버 1대당 globals-only 덤프 1개"를 명시적으로 추가해야 함(DB별 덤프와 별개 산출물,
  같은 job 그룹으로 묶어 매니페스트에 "이 DB 덤프는 어느 globals 덤프와 짝인지" 기록).
- 실행: `pg_dumpall --globals-only -f globals.sql` (plain SQL만 지원, custom 포맷 없음 — 별도 텍스트
  파일로 취급해 zstd+age는 동일하게 적용 가능).
- 복원 순서: **globals 먼저 → 그 다음 DB별 덤프.** 순서가 바뀌면 role 없는 상태에서 스키마 복원이
  실패함. 이 순서 규칙은 restore 커맨드/문서에 명시돼야 함.

### 1.3 Large Objects (LO / `pg_largeobject`)

- `pg_dump`는 기본적으로 large object를 포함하지만, **`--schema=`나 `--table=`로 특정 테이블만
  선택 덤프하면 LO는 통째로 빠지거나 통째로 포함되는 all-or-nothing** 동작을 함(LO는 특정 스키마에
  안 묶여 있어서 테이블 단위 필터링 대상이 아님). v1은 "DB 전체" 백업이 기본이라 당장은 안전하지만,
  향후 "테이블 선택 백업" 기능을 추가할 때 이 함정을 반드시 재검토해야 함 — `[확인 필요: 향후 기능]`.
- LO를 많이 쓰는 DB(이미지/파일을 bytea 대신 LO로 저장)는 `-Fc`가 LO를 OID 참조로 담기 때문에
  `pg_restore` 시 LO가 정상 복원되는지 검증 스텝(§4)에서 별도 확인 필요 — 단순 row count만으로는
  LO 누락을 못 잡음(LO 자체는 `pg_largeobject`라는 별도 시스템 테이블에 있고 참조 테이블의 row
  수는 그대로임).

### 1.4 확장(Extensions)

- `pg_dump`는 `CREATE EXTENSION IF NOT EXISTS`를 덤프에 포함하지만, **확장의 바이너리(.so/.dll)와
  제어 파일은 포함하지 않음** — 복원 대상 서버에 동일 확장이 설치돼 있어야 함(예: `postgis`,
  `pg_trgm`, `uuid-ossp`). 확장이 없는 서버로 복원하면 `CREATE EXTENSION` 라인에서 실패.
- 🟠 **매니페스트에 "사용 중인 확장 목록"을 기록**해서, 복원 대상 인스턴스(특히 verify용 임시
  인스턴스, §4)에 사전 설치해야 할 목록을 자동/수동으로 알 수 있게 해야 함. `SELECT extname,
  extversion FROM pg_extension;`을 백업 시점에 함께 캡처.
- 확장 **버전** 불일치도 문제: 덤프 시점 `postgis 3.4`, 복원 대상에 `postgis 3.2`만 있으면 실패하거나
  스키마가 다를 수 있음 — extversion까지 기록.

### 1.5 Per-database vs cluster-wide

- PostgreSQL 클러스터는 여러 DB를 가짐. "백업"이 의미 있으려면 (a) 클러스터 내 모든 DB 목록을
  알아야 하고(`\l` 상당, `pg_database`에서 `datistemplate = false`인 것들), (b) 새 DB가 생기면
  백업 job 목록에 자동/수동으로 반영되는 정책이 있어야 함.
- 🟠 spec의 `add-db` 커맨드는 DB를 개별 등록하는 구조로 보이는데, **신규 DB가 서버에 생겨도
  자동으로 백업 대상에 안 잡히는 게 기본 동작**이라면 이는 P2(백업 누락)를 재현하는 함정임.
  최소한 `run` 시점에 "서버에는 있는데 add-db로 등록 안 된 DB"를 감지해 경고하는 기능이 필요.

### 1.6 버전 호환성 (client >= server) 강제

- `pg_dump`는 **하위 호환**(신버전 클라이언트가 구버전 서버를 덤프)은 되지만 **상위 비호환**(구버전
  클라이언트가 신버전 서버를 덤프)은 즉시 에러로 실패함 — "aborting because of server version
  mismatch". 이건 실패가 명확해서 오히려 안전한 쪽.
- 🔴 **진짜 위험은 침묵 실패 쪽**: 클라이언트가 서버보다 오래됐지만 사소한 마이너 차이라 에러 없이
  돌아가는데, 신버전 서버의 신규 객체 타입(신규 데이터 타입, 신규 파티셔닝 문법 등)을 구버전
  `pg_dump`가 이해 못 해서 **조용히 누락**하는 케이스. 메이저 버전 차이가 클수록 위험 커짐.
  → 매 `run` 시작 시 `pg_dump --version`과 서버의 `SELECT version()`을 비교해 로그에 남기고,
  클라이언트 메이저 버전 < 서버 메이저 버전이면 **경고가 아니라 하드 실패**로 처리할 것을 권고
  (spec의 Risk Assessment에 이미 "실행 전 서버 버전 감지 후 클라이언트 선택"이 있으나, 매칭되는
  번들 클라이언트가 없을 때의 동작이 "가장 가까운 걸로 시도"면 안 되고 "실패 + 명시적 알림"이어야 함).
- 번들 버전 매트릭스는 최소 PG 13~17(현재 지원 버전 범위) 커버 필요. `[확인 필요: 실제 대상 서버
  버전군 — spec Open Questions에 미확정]`.

---

## 2. MySQL / MariaDB

### 2.1 `--single-transaction`의 컨센시 경계

- `--single-transaction`은 **InnoDB에서만** REPEATABLE READ 스냅샷으로 일관성을 보장함. **MyISAM은
  트랜잭션이 없으므로 이 플래그와 무관하게 덤프 도중 변경되면 비일관** — 그리고 이건 에러 없이
  조용히 발생함(불일치 데이터가 그냥 덤프에 들어감). `mysql.*` 시스템 테이블 일부, 오래된 앱의
  레거시 테이블이 MyISAM인 경우가 실무에서 드물지 않음.
- 🔴 **덤프 실행 전 엔진 스캔을 권고**: `SELECT table_schema, table_name, engine FROM
  information_schema.tables WHERE engine NOT IN ('InnoDB') AND table_schema NOT IN
  ('mysql','information_schema','performance_schema','sys');` 로 non-InnoDB 테이블을 찾아 매니페스트에
  기록하고, 있으면 사용자에게 "이 테이블들은 일관성 보장 안 됨" 경고. (spec의 Risk Assessment에
  "mysqldump 덤프중 DDL 비일관"은 있지만 MyISAM 자체 언급은 없음 — 추가 권고.)
- **DDL 이슈**: MySQL의 DDL(예: `ALTER TABLE`)은 트랜잭션이 아니라 즉시 커밋되는 성격이라
  `--single-transaction` 도중 DDL이 실행되면 `--single-transaction`이 잡고 있던 스냅샷과 무관하게
  스키마가 바뀔 수 있고, 최악의 경우 `mysqldump`가 "table structure changed" 에러로 죽거나 일부
  테이블만 새 스키마로 덤프됨. 백업 창(window) 동안 DDL 회피는 spec에 이미 문서화 권고로 있음(Risk
  Assessment) — 이를 넘어서, **DDL 감지 시 mysqldump가 에러 종료하면 "부분 성공"으로 처리하지 말고
  전체 실패로 catalog에 기록**해야 함(부분 스키마로 만들어진 덤프는 복원 시 예측 불가능한 상태를
  만들 수 있음).

### 2.2 routines/triggers/events/`--column-statistics`

- `mysqldump` 기본값은 **routines(저장 프로시저/함수)와 events를 덤프하지 않음** — `--routines`,
  `--events`를 명시적으로 켜야 함. **triggers는 기본으로 포함됨**(`--triggers`가 기본 on, 끄려면
  `--skip-triggers`). 세 개의 기본값이 서로 다르다는 게 함정 포인트 — 명시적으로 전부 켜서
  일관되게 관리하는 게 안전(`--routines --triggers --events`).
- `--column-statistics`: MySQL 8.0의 `mysqldump`가 기본으로 컬럼 통계를 `SET` 문으로 덤프하려
  시도하는데, **MariaDB 서버 대상이거나 구버전 MySQL 서버 대상이면 해당 통계 테이블이 없어서
  에러**남(`mysqldump: Couldn't execute 'SELECT ... column_statistics ...'`). MySQL↔MariaDB 혼용
  환경(spec 제목이 "MySQL, MariaDB; 버전 상이"라 명시)에서는 **기본적으로
  `--column-statistics=0`을 강제**하는 걸 권고 — 이 기능은 옵티마이저 힌트 재현용이라 백업
  목적에선 거의 무의미하고, 실패 리스크만 있음.
- 🟠 사용 중인 DB에 어떤 routines/triggers/events가 있는지 사전 스캔해서, 덤프 플래그 누락으로
  **조용히 빠지는** 걸 방지 — 덤프 후 `information_schema.ROUTINES` 등과 대조하는 사후 검증도 가능.

### 2.3 GTID / binlog 좌표

- 이 프로젝트는 논리 덤프만(v1) + PITR 보류로 확정(spec 확정 결정 표) — 즉 **binlog 기반 point-in-time
  복구는 스코프 밖**. 그럼에도 `--source-data=2`(MySQL 8.x 신문법, 구 `--master-data`) 또는
  `--gtid`로 **덤프 시점의 binlog 좌표/GTID 세트를 주석으로 덤프에 남겨두는 것 자체는 저비용으로
  가치가 있음** — PITR을 실제로 구현하지 않더라도, "이 논리 백업 시점 이후의 변경분이 있다면
  binlog 어디서부터 replay해야 하는지"의 앵커 포인트가 됨. 향후 PITR 확장 시 이 좌표가 없으면
  과거 백업들은 전부 앵커 없는 스냅샷으로 남아 하이브리드 복구(논리 백업 + 이후 binlog replay)가
  불가능해짐.
- 🟡 권고: v1에서 PITR을 구현하지 않더라도 `--source-data=2`(또는 GTID 모드면 `--set-gtid-purged=ON`
  기본 동작 활용)로 좌표만 캡처해 매니페스트에 저장해두면 미래 확장 비용이 낮아짐. 필수는 아니나
  "지금 안 하면 과거분은 영구히 재현 불가"라는 비가역성이 있어 우선순위를 올림.
- GTID 모드 서버(`gtid_mode=ON`)와 비GTID 서버가 섞인 환경이면 `--set-gtid-purged` 기본값이 서버
  설정에 따라 자동 분기되므로, 매니페스트에 "이 덤프가 GTID 기반인지 파일 기반 binlog 좌표인지"
  구분 기록 필요.

### 2.4 문자셋/콜레이션 복원 함정

- `mysqldump`는 기본적으로 클라이언트 연결의 `character_set_results`(보통 `utf8mb4`)로 덤프를
  생성하는데, **원본 테이블이 `latin1`이나 `utf8`(3바이트, mb4 아님) 등 다른 문자셋이면** 덤프
  텍스트 파일 자체는 지정된 결과셋으로 재인코딩되어 나가고, 복원 시 `SET NAMES`가 그 인코딩을
  선언함 — 이 과정에서 원본이 이미 깨진 바이트(mojibake)를 담고 있었다면 재인코딩 시 **복구 불가능한
  손상**이 발생할 수 있음(특히 `latin1`로 저장된 걸 실제로는 다른 인코딩 바이트가 들어있던 레거시
  DB에서 흔함).
- 🟠 권고: `--default-character-set=utf8mb4`를 명시적으로 고정하거나, 최소한 원본 DB/테이블의
  실제 charset/collation(`SHOW CREATE DATABASE`, `information_schema.SCHEMATA.DEFAULT_CHARACTER_SET_NAME`)을
  매니페스트에 기록해서 복원 후 "글자가 깨졌다"는 문제가 생겼을 때 원인 추적이 가능하게 해야 함.
  덤프 자체가 이미 손상됐다면 검증 단계(§4)에서 문자열 컬럼 샘플 비교로 잡아낼 여지를 마련.

### 2.5 MariaDB vs MySQL 분기

- **`--column-statistics`**: 위 2.2 참조. MySQL 8.0 client가 MariaDB 서버 대상으로 뜨면 기본값
  때문에 에러 — 서버 타입 감지(`SELECT VERSION()`에 `MariaDB` 문자열 포함 여부) 후 클라이언트
  플래그를 분기해야 함.
- **`mysqldump` 바이너리 자체의 출처**: MariaDB 배포판의 `mysqldump`와 Oracle MySQL 배포판의
  `mysqldump`는 이름은 같지만 지원 플래그 집합이 다름(예: MariaDB는 `--dump-slave` 유지, MySQL 8은
  `--source-data`로 개명). 번들 시 "MySQL용 mysqldump.exe"와 "MariaDB용 mysqldump.exe"를 **별도
  바이너리로 매트릭스 관리**해야 함 — research.md의 "번들 버전 매트릭스"가 PostgreSQL 메이저 버전
  축만 고려하고 있다면 MySQL/MariaDB 분기 축을 놓칠 위험. `[확인 필요: 번들 매트릭스에 MariaDB
  별도 축 포함 여부]`.
- **시스템 카탈로그 차이**: `information_schema.ROUTINES`/`EVENTS` 등의 컬럼 구성이 버전별로 약간
  다를 수 있어 사전 스캔 쿼리를 MySQL/MariaDB 양쪽에서 테스트 필요.

---

## 3. 백업 매니페스트 — 덤프 본체와 별도로 반드시 캡처해야 할 것

핵심 원칙: **매니페스트가 없으면 덤프 파일은 "언젠가 무엇으로 복원해야 할지 모르는 바이트 뭉치"임.**
카탈로그(로컬 SQLite)는 참조용 캐시로만 쓰고, 매니페스트 자체는 **암호화된 형태로 백업 오브젝트와
같은 목적지에 동봉**되어야 함(로컬 SQLite가 랜섬웨어/장비 전손으로 사라져도 오프사이트에서 단독
복원 가능해야 하므로).

### 3.1 필드 목록

**식별/버저닝**
- `manifest_version` (매니페스트 스키마 자체의 버전 — 향후 필드 추가 대비)
- `backup_id` (UUID)
- `job_name` / `db_alias` (spec의 `add-db`로 등록한 이름)
- `created_at` (UTC, RFC3339)

**원본 DB 식별**
- `db_engine` (`postgresql` | `mysql` | `mariadb`)
- `server_version` (`SELECT version()` 전체 문자열 원문 + 파싱된 semver)
- `server_version_detected_at` (버전 감지가 실제로 이 백업 실행 시점에 이뤄졌는지 타임스탬프로 증명)
- `database_name` (또는 globals 덤프면 `cluster` 표시)
- `charset` / `collation` (PG: `SELECT current_setting('server_encoding')` 등, MySQL: DB 기본 charset/collation)
- `extensions` (PG 전용: `[{name, version}]` — §1.4)
- `non_transactional_tables` (MySQL 전용: MyISAM 등 비InnoDB 테이블 목록 — §2.1, 비어있으면 명시적으로 `[]`)

**덤프 도구/명령**
- `dump_tool` (`pg_dump` | `pg_dumpall` | `mysqldump`)
- `dump_tool_version` (`pg_dump --version` 출력 원문)
- `dump_format` (`custom` | `plain` | `directory`)
- `dump_flags` (실제 실행된 전체 커맨드라인, secret 제외하고 원문 그대로 — 재현성을 위해)
- `pairs_with_globals_backup_id` (PostgreSQL 전용: 이 DB 덤프가 어느 `pg_dumpall --globals-only`
  산출물과 짝인지 — §1.2)
- `binlog_coordinates` / `gtid_set` (MySQL 전용, §2.3, 캡처했다면)

**파이프라인 처리 이력**
- `compression` (`zstd`, level)
- `encryption` (`age`, recipient 방식, wrap 종류 — 어떤 경로로 복호화 가능한지: 비번/복구코드/DPAPI)
- `plaintext_size_bytes` / `compressed_size_bytes` / `encrypted_size_bytes`
- `checksum_plaintext` (덤프 직후, 압축/암호화 전 — 이게 있어야 "복원한 걸 다시 덤프해서 비교"가 아니라
  단순 재복호화+재해제만으로 원본과 대조 가능)
- `checksum_final` (목적지에 실제 업로드된 최종 바이트의 체크섬 — 전송 중 손상 탐지용)

**목적지/불변성**
- `destination_type` (`s3` | `ssh` | `local`)
- `destination_path` / `bucket`
- `object_lock_mode` (`governance` | `compliance` | `none`) + `retain_until`

**검증 상태 (verify 실행 후 갱신)**
- `verified` (bool)
- `verified_at`
- `verify_method` (`pg_restore --list` only | full restore + row count | full restore + schema hash)
- `row_counts` (테이블별, 검증 시 실제 복원본에서 집계한 값 — §4)
- `restore_target_version` (검증에 쓴 임시 인스턴스의 서버 버전 — 원본과 다르면 "다른 버전으로
  검증했다"는 사실 자체를 기록해야 함, 원본과 동일 버전 검증이 아니면 완전한 보증이 아님)

### 3.2 이 필드들이 없을 때 실제로 벌어지는 일

- `dump_tool_version` 없음 → 몇 달 뒤 복구 시점에 어떤 `pg_restore`로 열어야 custom 포맷 호환이
  되는지 추측만 가능(custom 포맷은 포맷 자체 버전이 있어 대체로 하위호환이지만, 극단적 구버전
  `pg_restore`는 신버전 custom 포맷을 못 읽음).
- `pairs_with_globals_backup_id` 없음 → globals 덤프가 여러 개 쌓였을 때 "이 DB 덤프 시점에 유효했던
  role 정의가 어느 globals 스냅샷인지" 매칭 불가 — role 권한이 시간에 따라 바뀌는 환경에서 잘못된
  버전의 globals로 복원하면 권한이 틀어진 채로 "성공"해버림(에러 없이 조용히 틀림).
- `non_transactional_tables` 없음 → 복원 후 데이터가 이상해도, 애초에 덤프 시점에 비일관 가능성이
  있었다는 사실 자체를 사후에 알 방법이 없음(디버깅 시간 낭비 + 오탐으로 "복원 프로세스 버그"를
  의심하게 됨).
- `charset`/`collation` 없음 → §2.4의 mojibake 문제 발생 시 원인 추적 불가.

---

## 4. 복원 검증(Verify) 설계 — "검증됨"의 실제 의미

spec의 Verifier는 "임시 인스턴스로 test-restore + 체크섬 대조"로만 짧게 정의돼 있음(+0 가중치,
즉 우선순위 낮게 표시돼 있는데 — P5 문제의식과 모순되므로 아래에서 재검토 권고).

### 4.1 검증 레벨을 명확히 계층화할 것

단일 "verified: true/false"가 아니라, 아래 3단계로 나눠 매니페스트에 어느 레벨까지 통과했는지
기록하는 걸 권고 (레벨이 올라갈수록 비용은 크지만 보증도 강함):

1. **레벨 1 — 파일 무결성**: 재복호화(age) + 재압축해제(zstd)가 에러 없이 끝까지 됨 +
   `checksum_plaintext`와 일치. **비용 거의 없음(매 백업 직후 자동 수행 가능 — 실제로 spec의 Risk
   Assessment에 이미 "백업 후 즉시 복호화 헤더 검증"이 있음, 이걸 전체 콘텐츠 체크섬까지 확장 권고).**
2. **레벨 2 — 덤프 포맷 유효성**: `pg_restore --list`(custom 포맷 목차 파싱, 실제 데이터를 안 풀고도
   덤프 파일 헤더/TOC가 손상되지 않았는지 확인) 또는 MySQL은 덤프 SQL을 `mysql --execute="SOURCE
   ...` 없이) 문법 파서로만 통과시키는 dry-run(완전한 등가물은 없어서 MySQL은 레벨 2가 약함 —
   대안으로 최소한 파일 앞부분에 예상 헤더(`-- MySQL dump`, 버전 문자열)가 있는지 정도). **비용
   낮음, 매 백업마다 수행 가능.**
3. **레벨 3 — 실제 복원(Full Restore)**: 임시 인스턴스(로컬 컨테이너 또는 spare 인스턴스)에 실제로
   복원 실행 → (a) 스키마 해시 대조(원본 DB에서 `pg_dump --schema-only`를 뜨서 정규화 후 해시,
   복원본에서도 동일하게 떠서 비교 — 데이터가 아니라 구조가 그대로 살아났는지), (b) 테이블별
   row count 대조(원본 백업 시점 값 vs 복원본 값 — 원본 DB의 "현재" row count와 비교하면 안 됨,
   백업 시점 이후 원본이 계속 바뀌므로 **백업 시점에 캡처한 row count를 매니페스트에 저장**해두고
   그것과 비교해야 함), (c) 가능하면 샘플 row의 체크섬/해시 대조(문자셋 손상 등 "row 수는 맞는데
   내용이 깨짐" 케이스를 잡기 위함, §2.4). **비용 큼 — 이게 spec에서 "test-restore"라 부르는 것.**

- 🔴 **"verified"라는 단어를 카탈로그/매니페스트/알림에 쓸 때는 반드시 어느 레벨인지 같이 표기할
  것.** 레벨 1만 통과한 걸 "verified: true"로 표시하면, 운영자가 실제 재해복구 상황에서 "검증됐다고
  했는데 스키마가 깨져 있다"를 겪을 수 있음 — 이게 바로 P5("죽은 백업")이 재해 시점에 재현되는
  경로임.
- 🟠 **주기 정책 권고**: 레벨 1/2는 매 백업 실행 직후 자동(비용 거의 없으므로). 레벨 3(full
  restore)는 spec의 Open Questions에 "매 백업 vs 주간"이 미확정으로 남아있는데, **최소 권고는
  "각 DB job당 최소 주 1회 + 매니페스트/스키마가 바뀐 첫 백업(=DDL 변경 감지 시)엔 즉시"**.
  스키마 변경은 덤프 자체가 새 구조를 담게 되므로, 변경 후 첫 백업이 실제로 복원되는지 확인 안 하고
  다음 주까지 기다리면 그 사이의 모든 백업이 잠재적으로 복원 불가 상태로 방치될 수 있음(스키마 변경
  감지는 §3.1의 스키마 해시를 매 백업마다 계산해 이전 값과 비교하는 걸로 저비용 구현 가능).
- **임시 인스턴스의 버전이 원본과 다르면 안 됨(원칙)**: 검증용 임시 인스턴스가 원본과 다른 메이저
  버전이면, "복원됨"이 곧 "재해 시 실제 프로덕션 버전으로도 복원된다"를 보증하지 않음. 최소한
  매니페스트의 `restore_target_version`에 기록해 그 갭을 투명하게 남길 것 — spec의 "임시 인스턴스
  실체(로컬/Docker)"가 Open Question으로 남아있는데, Docker 기반이면 원본과 동일 메이저 버전
  이미지를 동적으로 골라 쓰는 걸 권고.

---

## 5. Retention/Rotation (GFS) 및 RPO/RTO — 논리 백업의 한계

### 5.1 GFS(Grandfather-Father-Son) 로테이션

- 논리 덤프는 스냅샷 단위(파일 하나 = 특정 시점 전체)라 증분이 없음 — GFS는 "몇 개를 얼마나
  오래 보관하냐"의 문제로 단순화됨(디스크 기반 백업처럼 블록 레벨 증분에서 체인이 끊기는 문제는
  없음, 각 파일이 독립적으로 복원 가능하다는 게 논리 덤프의 장점).
- 권고 정책 예시(개인/소규모 인프라 맥락에 맞게): **일간(daily) 7개 + 주간(weekly) 4개 + 월간
  (monthly) 12개**를 유지하고, 그 이상은 prune. Object Lock Governance 모드와 결합 시 **각 오브젝트의
  `retain_until`을 GFS 등급에 맞춰 다르게 설정**해야 함 — daily는 7일 후 prune 가능해야 하는데
  retain_until을 1년으로 걸어버리면 storage 비용이 무한 누적되고, 반대로 monthly를 daily와 같은
  짧은 retain으로 걸면 랜섬웨어 창(dwell time, 통상 며칠~몇 주)을 못 버팀.
- 🟠 **prune 신원(identity) 분리는 spec에 이미 있음**("prune는 별도 격리 신원") — 이건 맞는 설계.
  추가로 **prune 로직 자체가 "retain_until이 지난 것만" 삭제 가능한지를 실행 전 always dry-run으로
  먼저 리스트업**하는 안전장치를 권고(실수로 GFS 계산 버그가 아직 살아있어야 할 백업을 지우는 걸
  방지 — 삭제는 되돌릴 수 없으므로 이 레이어의 실수가 가장 치명적).
- **매니페스트의 `retain_until`과 실제 S3 Object Lock의 `retain_until`이 반드시 일치해야 함** —
  둘이 따로 관리되면(예: 매니페스트는 로컬 로직으로 GFS 판단, S3 헤더는 업로드 시점 고정값) 드리프트
  가능. 단일 진실 소스(source of truth)를 정하고 다른 쪽은 그걸 참조만 하게 설계할 것.

### 5.2 논리 백업(v1) vs PITR — RPO/RTO 차이를 사용자에게 명시적으로 알릴 것

- **RPO(복구 시점 목표)**: 논리 덤프만 쓰는 v1의 RPO는 **"마지막 백업 실행 시각까지"**임 — 백업이
  일 1회면 최악의 경우 최대 24시간(+덤프 소요 시간)의 데이터 유실 가능. PITR(WAL/binlog 연속
  아카이빙)이 있어야 RPO를 "초 단위"로 줄일 수 있는데, 이는 spec에서 명시적으로 Out of Scope로
  뺀 부분 — **설계상 맞는 스코프 결정이지만, 사용자(단일 운영자)가 이 RPO 한계를 인지하고 있어야
  함.** 🟡 권고: CLI `init` 또는 `run` 완료 메시지에 "이 백업 방식의 RPO는 최대 [백업 주기]입니다"를
  명시적으로 출력하는 걸 권고(문서에만 적어두면 실제 재해 상황에서 잊혀짐).
- **RTO(복구 소요 시간 목표)**: 논리 복원은 전체 스키마+데이터 replay라서 **DB 크기에 비례해
  선형적으로 느림**(물리 백업/PITR 대비 확연히 느림 — 특히 인덱스 재생성이 큰 비중을 차지). 대용량
  DB(수십~수백 GB)라면 RTO가 시간 단위로 늘어날 수 있음. `mydumper`/`myloader`(병렬)나 `pg_restore
  -j`(병렬)로 완화 가능하지만 v1 기본 경로(`mysqldump` 단일 스레드, `pg_dump -Fc` 단일 스레드 덤프)는
  이 이점을 못 받음 — research.md에 이미 "mydumper는 대용량 opt-in"으로 정리돼 있으나, **RTO 관점에서
  "얼마나 큰 DB부터 mydumper/parallel restore로 전환해야 하는지"의 임계값이 spec Open Questions의
  "DB 실제 대수/최대 크기"가 확정돼야 정해질 수 있음** — 이 값이 나오면 RTO 목표(예: "4시간 이내
  복구")를 역산해 병렬 복원 전환 임계 크기를 정할 것을 권고.
- **핵심 메시지**: 논리 백업은 "죽은 백업"을 막는 데는 충분하지만(P5 해결), "RPO/RTO를 최소화"하는
  솔루션은 아님 — 이 프로젝트의 성공 기준("복원 가능한 신뢰 가능한 백업")과 "빠른 복구"는 다른
  목표이고, spec은 전자만 약속하고 있다는 걸 문서 어딘가(README/운영 가이드)에 명확히 해야
  기대치 불일치를 막을 수 있음.

---

## Dictionary Updates

- **매니페스트(manifest)** — 신규 규범 용어. 정의: 백업 오브젝트 본체와 별도로, 같은 목적지에
  암호화되어 동봉되는 백업 메타데이터 집합(서버/도구 버전, 덤프 플래그, charset, 체크섬, globals
  페어링 정보, 검증 상태 등 — §3.1 필드 목록 참조). **카탈로그(로컬 SQLite)와 구분**: 카탈로그는
  로컬 조회용 캐시/색인이고, 매니페스트는 목적지에 동봉되는 진실 소스(source of truth). 카탈로그가
  소실돼도 매니페스트로 복구 가능해야 함.
- **globals 덤프** — 신규 용어. `pg_dumpall --globals-only`의 산출물(role/tablespace 등 클러스터
  전역 객체). DB별 `pg_dump` 산출물과는 별개 산출물이며, 매니페스트의 `pairs_with_globals_backup_id`
  로 짝을 명시해야 함. "PostgreSQL 백업"이라는 용어를 쓸 때 globals 포함 여부를 항상 명시할 것
  (예: "DB 덤프만" vs "클러스터 전체(DB 덤프 + globals)").
- **검증 레벨(verify level)** — 신규 용어, 3단계: 레벨 1(파일 무결성) / 레벨 2(포맷 유효성) /
  레벨 3(실제 복원). 기존 spec의 "verified: true/false" 이진 상태 표현은 이 3단계 중 어느 것을
  가리키는지 모호하므로, 앞으로 "verified"라는 단어를 단독으로 쓰지 말고 항상 레벨과 함께 쓸 것을
  제안(예: "verified(L3)").
- **RPO/RTO** — spec에는 등장하지 않던 용어. 이 프로젝트의 성공 기준(§Overview)이 "무인 백업 성공
  + 불변성 + 정기 test-restore 통과"로 정의돼 있는데, 이는 사실상 **RPO=백업 주기, RTO=미정(논리
  복원 소요시간)**이라는 명시적 목표로 번역 가능함. 향후 스펙 문서에 이 두 용어를 정식 채택해
  "확정 결정" 표에 RPO/RTO 목표치를 명문화할 것을 제안.
