# Section 02 — Domain and Operational Store

## Background

현재 v1은 Connection과 개별 Database를 사실상 한 설정으로 취급하고, TOML job과
Windows Task Scheduler를 실행 의도의 권위 데이터로 사용한다. Catalog는 signed Manifest에서
재구축할 수 있는 백업 projection인데도 제어 상태와 같은 의미로 소비되는 곳이 있다. 이 구조는
한 서버의 여러 DB, plan revision, schedule occurrence, retry Attempt, 다중 Destination Replica를
정확히 표현하지 못한다.

이 섹션은 source adapter나 Windows service를 만들지 않는다. 다음 섹션들이 공유할 불변 도메인과
단일 SQLite Operational Store를 먼저 만든다. production execution authority는 여전히 legacy
CLI/Task Scheduler에 남고, v2 import는 shadow/dry-run까지만 허용한다.

권위 경계는 다음과 같다.

| 데이터 | 권위 |
|---|---|
| Connection, Target, Destination, Plan, queue, Run, Activity | Operational Store |
| credential과 key payload | 외부 Secret Store; SQLite에는 Secret Ref만 저장 |
| 복구 artifact의 완전성·무결성 | Destination의 signed Manifest와 object |
| Snapshot 목록 | signed Manifest에서 재구축 가능한 `catalog_*` projection |

도메인 cardinality는 아래 순서를 사용한다.

```text
ConnectionProfile -> DatabaseTarget
BackupPlan -> immutable PlanRevision
PlanRevision -> PlanOccurrence
PlanOccurrence -> BackupRun(one Target)
Run -> WorkItem -> Attempt
BackupRun -> RecoveryPoint -> ArtifactPart -> ArtifactReplica -> StoredObject
```

Backup/Restore/Verification/Catalog Rebuild 외의 probe, discovery, repair, retention,
notification, migration, control/support 작업은 `OperationRun -> WorkItem`으로 같은 durable
실행 계약에 연결한다.

## Requirements

### Domain

- `DOM-01` ID는 application이 만든 typed UUID/ULID 문자열이며 raw `String`을 계층 사이에
  무분별하게 전달하지 않는다.
- `DOM-02` `ConnectionProfile`은 서버/파일 접속이고 `DatabaseTarget`은 발견 후 보호 대상으로
  선택된 개별 DB다. Target 이력은 discovery에서 사라져도 자동 삭제하지 않는다.
- `DOM-03` Plan 수정은 새 `PlanRevision`을 만들며 publish 뒤 revision과 Target/Destination
  membership은 불변이다.
- `DOM-04` scheduled/catch-up occurrence는 revision+UTC 예정 시각, manual occurrence는 client
  idempotency key로 식별한다.
- `DOM-05` 하나의 occurrence는 여러 Target Run을 가질 수 있고 결과를 `succeeded`,
  `partially_succeeded`, `failed`로 집계한다.
- `DOM-06` Work Item은 정확히 하나의 Backup/Restore/Verification/Catalog Rebuild/Operation
  Run만 가리킨다. retry는 같은 Work Item의 새 Attempt다.
- `DOM-07` Attempt는 at-least-once이고 claim마다 증가하는 Fencing Token을 가진다.
- `DOM-08` Snapshot은 complete Recovery Point의 UI/search projection이다. Recovery Point
  completeness, Replica durability, Protection Compliance, restore eligibility는 독립 축이다.
- `DOM-09` 허용 상태 전이는 `dbbackup-domain`의 pure transition table에서 정의하고 DB
  repository도 같은 전이를 검사한다.

### SQLite Operational Store

- `STORE-01` 정본 DDL은 `docs/plan/connected-backup-platform-v2/schema-v2.sql`의 53개
  `STRICT` table과 trigger/index다. 구현 migration은 그 의미를 축소하지 않는다.
- `STORE-02` `dbbackup-service`의 Transaction Owner만 read-write connection 하나를 가진다.
  UI/CLI는 SQL에 접근하지 않고 service IPC를 사용한다.
- `STORE-03` 모든 connection은 `foreign_keys=ON`, `busy_timeout=5000`; writer는 WAL과
  `synchronous=FULL`을 사용한다.
- `STORE-04` writer actor는 lease/heartbeat/fence/control transition을 high-priority lane,
  Activity append를 bounded low-priority batch로 처리한다.
- `STORE-05` network, child process, file upload를 기다리는 동안 SQLite transaction을 열지
  않는다.
- `STORE-06` Control State는 `catalog_*`를 FK로 참조하지 않는다. Catalog Projection 전체
  삭제/rebuild가 Plan, queue, Run, restore history를 cascade하지 않아야 한다.
- `STORE-07` live DB 파일 복사를 금지하고 SQLite online backup API 또는 명시적 quiesce를
  사용한다. 성공은 `integrity_check`, `foreign_key_check`, schema/관계 검증까지 포함한다.

### Enforced invariants

- published revision 직접 insert와 update/delete/membership 변경 거부
- publish 시 Target 1개 이상, Destination 1개 이상, required Destination 1개 이상
- scheduled/manual occurrence 중복 거부
- occurrence 안 Target별 Backup Run 1개
- Work Item 다중 subject 거부
- complete Recovery Point는 valid signature, required part 1개 이상, present count 충족
- Stored Object의 Part와 Replica는 같은 Recovery Point
- destructive confirmation binding 불변, issued에서 terminal로만 전이, terminal immutable
- overwrite Restore Run은 confirmation row 필수, 비파괴 restore는 confirmation 불필요
- L3 Verification Run은 실제 Restore Run evidence 필수
- Snapshot projection의 네 상태 축을 별도 enum으로 저장

여러 행의 permit 합계와 stale fencing은 단일 `CHECK`만으로 안전하게 표현하지 않는다. 반드시
`BEGIN IMMEDIATE` 안에서 관찰값을 포함한 CAS update를 사용하고 affected row 1개를 성공
조건으로 삼는다.

### IPC ledger and destructive confirmation metadata

- mutating request는 `SID hash + command + idempotency key + canonical payload digest`를
  `ipc_request_ledger`에 저장한다.
- 같은 key·같은 payload는 기존 result/Work Item을 반환하고 같은 key·다른 payload는
  `IPC_IDEMPOTENCY_CONFLICT`다.
- raw SID와 raw confirmation token은 저장하지 않는다.
- confirmation metadata는 SID, action, resource ID/fingerprint, payload digest, expiry를
  고정하며 Run/Work Item 생성 transaction에서 한 번만 소비한다.
- named-pipe 인증과 실제 command dispatch는 section-03이 구현한다. 이 섹션은 schema와
  repository operation을 제공한다.

### Migration and cutover preparation

- v1 TOML, keystore metadata, 기존 Catalog, Task XML/enabled 상태를 recovery bundle로
  수집하되 secret 원문을 report에 넣지 않는다.
- production과 다른 파일에 v2 schema를 만들고 `dry_run` 또는 `shadow` import한다.
- Connection과 Target을 분리하고 deterministic stable ID, Secret Ref, Plan Revision,
  Target/Destination membership을 만든다.
- signed Manifest/object와 대조되지 않은 v1 backup은 complete Snapshot으로 승격하지 않는다.
- diff는 source row, imported row, skipped/ambiguous item, next due, secret binding을 포함한다.
- G1 전에는 production cutover marker, scheduler freeze, service execute authority를
  변경하지 않는다.

## Dependencies

- **Requires**: section-01 Truth Baseline과 ADR-001~007 승인
- **Blocks**: section-03 Service/IPC/Secrets, section-04 Durable Dispatcher,
  section-07 Product UI Foundation
- **병렬 제한**: migration/도메인 public type과 DDL을 한 변경에서 먼저 안정화한 뒤 소비
  crate를 병렬화한다.

## Flow Diagram Nodes

### `connection-onboarding.mmd`

| Node | 책임 |
|---|---|
| `createConnection` | Connection과 Secret Ref binding을 transactionally 저장 |
| `runProbe`, `probeOk` | Probe Run 상태 envelope와 결과 저장; 실제 provider는 section-06 |
| `discoverTargets` | 시점별 Discovery Run/observed database 저장 |
| `discoveryChanged` | 최신 성공 observation과 stable Target identity 비교 |
| `recordDrift` | Target 자동 삭제 없이 Activity/Attention용 drift result 생성 |
| `keepProjection` | 차이가 없을 때 기존 Target projection 유지 |

### `config-cutover.mmd`

| Node | 책임 |
|---|---|
| `exportLegacy` | recovery bundle metadata와 checksum 생성 |
| `shadowImport` | 별도 v2 DB에 멱등 import |
| `validateDiff` | stable ID, 관계, Secret Ref, schedule diff 검증 |
| `discardShadow` | production 변경 없이 정확한 shadow 파일만 폐기 가능 상태로 표시 |

`validateDiff=실패`는 `discardShadow -> stopped`, 성공은 section-01의 `durableGate`로 간다.
이 섹션은 `freezeLegacy`, `writeMarker`, `disableOldPath`를 실행하지 않는다.

## Reference Libraries

구현 직전 공식 문서와 lockfile을 확인하고, 아래 기존 pin을 임의 상향하지 않는다.

| Library | 기준 | 용도 |
|---|---|---|
| Rust | edition 2021, rust-version 1.91 | domain/repository/migration; OpenDAL 0.58의 실제 MSRV와 일치 |
| `rusqlite` | `=0.31.0`, `bundled` | SQLite, transaction, online backup |
| `serde`, `serde_json` | workspace pin | versioned DTO/policy/evidence |
| `uuid` 또는 현행 ID crate | workspace 확인 후 한 가지 | typed stable ID |
| `blake3` | workspace pin | schema/import/payload digest |
| `zeroize` | workspace pin | importer의 transient secret buffer |

## Implementation Details

### 1. `dbbackup-domain`

1. 새 crate에 ID newtype, enum, entity, immutable execution spec을 만든다.
2. `PlanRevision` publish validation과 occurrence identity builder를 pure function으로 만든다.
3. Work Item/Attempt/Run/Recovery Point 상태 전이를 `(current, command) -> next/error`로 만든다.
4. serialized enum 문자열은 DDL enum과 golden test로 비교한다.
5. domain crate는 rusqlite, Tauri, native process, Secret Store를 의존하지 않는다.

### 2. Migration runner

1. `dbbackup-catalog`에 embedded numbered migration과 checksum journal을 추가한다.
2. 이미 적용된 version의 checksum이 다르면 write mode 시작을 거부한다.
3. `PRAGMA user_version`은 빠른 guard이고 `schema_migrations`가 상세 권위 journal이다.
4. destructive migration은 new table→copy→row/FK 검증→rename→old archive 순서다.
5. production 파일이 아닌 in-memory/temp fixture에 `schema-v2.sql` 전체를 실행하는 test를 둔다.

### 3. Repository boundary

아래 repository 외에는 SQL을 노출하지 않는다.

| Repository | 소유 범위 |
|---|---|
| `ConfigurationRepository` | Connection, Target, Destination, Plan, Secret Ref binding |
| `RuntimeRepository` | Occurrence, Run, Work Item, Attempt, Resource, Spool, Process |
| `ActivityRepository` | Activity, Attention |
| `CertificationRepository` | Tool, Engine Certification |
| `NotificationRepository` | Channel, Rule, Outbox, Delivery |
| `CatalogProjectionRepository` | 모든 rebuildable `catalog_*` |
| `MigrationRepository` | migration/meta/import/control backup/IPC ledger/confirmation |

각 write method는 transaction closure로 실행하고 domain error를 stable application code로
매핑한다. raw SQLite error 문자열을 IPC/UI 계약으로 노출하지 않는다.

### 4. Critical transactions

- **Occurrence upsert**: scheduled 또는 manual identity partial unique index에
  insert-or-select하고 생성 여부를 반환한다.
- **Claim/fence**: due row와 observed attempt/fence를 읽고 같은 transaction에서 leased,
  incremented fence, Attempt insert를 수행한다.
- **Admission**: canonical resource key 순서로 capacity를 계산하고 전부 가능할 때만 allocation과
  spool reservation을 만든다.
- **Fenced mutation**: Work Item ID, attempt no, fence, owner, 허용 state를 모두 WHERE에 둔다.
- **IPC consume**: ledger insert/compare, confirmation CAS, Run/Work Item insert를 한 transaction에
  묶는다.
- **Recovery Point projection**: signed Manifest publish 뒤 current fence를 재확인하고 Run과
  catalog projection을 같은 transaction에 반영한다. publish 뒤 DB crash는 후속
  Reconciliation이 동일 ID로 재투영한다.

실제 dispatcher/Manifest publish는 section-04/05가 구현하며 이 섹션은 repository operation과
deterministic test double을 제공한다.

### 5. WAL, backup, archive

- read pool은 `mode=ro`, bounded connection/query timeout을 사용한다.
- idle/WAL threshold에서 passive checkpoint, drain/control backup 전 조건부 truncate
  checkpoint를 사용한다.
- Activity/outbox prune은 keyset 작은 batch와 incremental vacuum을 사용한다.
- `VACUUM`은 maintenance mode에서만 실행한다.
- control backup은 별도 경로에 online backup 후 integrity/FK/schema 검사하고 production
  파일 overwrite를 절대 수행하지 않는다.

### 6. Shadow importer

1. v1 parser 결과를 canonical intermediate model로 만든다.
2. server endpoint와 database name을 분리해 Connection/Target stable key를 계산한다.
3. credential은 Secret Store import interface에 넘기고 SQLite에는 returned Secret Ref만 쓴다.
4. job을 Plan+revision 1+membership으로 만들되 validation 전 publish하지 않는다.
5. Task XML은 schedule seed가 아니라 recovery/cutover evidence로 저장한다.
6. v1 catalog는 signed Manifest/object 대조 결과에 따라 complete/incomplete/quarantined로
   분류한다.
7. 동일 source checksum의 shadow import 재실행 결과가 byte-stable relation/diff인지 검증한다.

## Test Scenarios

### Normal

| ID | 입력·행동 | 기대 결과 |
|---|---|---|
| S02-N01 | Connection 1개에서 발견 DB 3개 중 2개 선택 | Connection 1, Target 2, observation 3 |
| S02-N02 | draft revision에 Target 2, required Destination 1을 넣고 publish | 불변 revision, current pointer 갱신 |
| S02-N03 | 같은 scheduled occurrence 두 번 upsert | 같은 Occurrence ID, 생성 1회 |
| S02-N04 | 같은 manual key/payload 재전송 | 같은 Occurrence/Work Item result |
| S02-N05 | generic tool rescan Operation Run enqueue | Work Item subject 정확히 1개 |
| S02-N06 | live WAL writer 중 online backup | integrity/FK/schema 검사 통과 |
| S02-N07 | Catalog Projection 전량 삭제 후 signed Manifest ingest | Control State 불변, 동일 Recovery Point 재구축 |
| S02-N08 | v1 fixture shadow import 두 번 | stable ID/relation/diff 동일, production 불변 |

### Error

| ID | 입력·행동 | 기대 결과 |
|---|---|---|
| S02-E01 | best-effort Destination만 있는 revision publish | application과 trigger 모두 거부 |
| S02-E02 | published revision/membership 변경 | immutable trigger 거부 |
| S02-E03 | Work Item에 Backup Run과 Operation Run 동시 연결 | `CHECK` 거부 |
| S02-E04 | overwrite Restore Run에 confirmation 없음 | `CHECK` 거부 |
| S02-E05 | L3 Verification에 Restore Run 없음 | `CHECK` 거부 |
| S02-E06 | complete RP에 required part 0 또는 invalid signature | `CHECK`/commit 거부 |
| S02-E07 | 같은 IPC key에 다른 payload digest | `IPC_IDEMPOTENCY_CONFLICT`, 첫 result 불변 |
| S02-E08 | consumed token binding 변경 또는 issued로 역전 | trigger/CAS 거부 |
| S02-E09 | migration checksum drift | service write mode 시작 거부 |
| S02-E10 | control backup disk full/integrity 실패 | 성공 row 없음, production DB 정상 |

### Edge and Fault

| ID | 입력·행동 | 기대 결과 |
|---|---|---|
| S02-X01 | DST 반복 시각과 catch-up | UTC identity 충돌 없이 bounded latest-one |
| S02-X02 | 동일 timestamp Activity 여러 건 | `(occurred_at_ms, seq)` cursor 결정적 |
| S02-X03 | Manifest publish 뒤 catalog transaction crash | artifact truth 보존, rebuild 가능 |
| S02-X04 | stale fence로 heartbeat/commit | affected row 0 |
| S02-X05 | resource 하나 부족 | allocation 0개, wait reason만 기록 |
| S02-X06 | discovery에서 기존 DB 누락 | Target 보존, missing/drift 상태 |
| S02-X07 | catalog child가 다른 RP의 Part/Replica 연결 | composite FK 거부 |
| S02-X08 | shadow import 중 전원 차단 | production 불변, shadow 재실행 가능 |

## Implementation Strategy

### Red

DDL execution, 53 table count, trigger/FK, domain state transition, migration golden, Catalog rebuild,
online backup, IPC/token invariant test를 먼저 작성하고 실패를 확인한다.

### Green

domain types와 embedded DDL, single-writer repository, 최소 shadow importer를 구현해 test를
통과한다. 이 단계에서 service나 native process를 연결하지 않는다.

### Refactor

row mapping, transaction helper, error translation, ID generation, import normalization을 중복 없이
분리한다. query plan과 writer latency를 기록하고 heartbeat lane을 굶기는 broad transaction을
제거한다.

## Quality Gate

- [ ] section-01 G0-A 통과
- [ ] `schema-v2.sql`이 bundled rusqlite in-memory DB에서 실행되고 table count 53
- [ ] `integrity_check=ok`, `foreign_key_check` 0
- [ ] 모든 published revision/occurrence/work subject/token/RP composite invariant test 통과
- [ ] domain enum과 DDL enum golden test 통과
- [ ] Catalog rebuild 전후 control table row/hash 불변
- [ ] online backup restore rehearsal 통과
- [ ] shadow import가 production file/scheduler를 변경하지 않음
- [ ] fmt, clippy, workspace tests와 migration power-loss matrix 통과
- [ ] production execution authority는 여전히 `legacy_direct`

## Risk and Rollback

| Risk | Mitigation | Rollback/Forward action |
|---|---|---|
| 53-table 초기 복잡도 | authority/repository별 migration test와 single writer | marker 전 shadow DB 폐기 |
| enum drift | domain↔DDL golden test | migration 전 build block |
| WAL 성장/heartbeat starvation | bounded read/write, priority lane, checkpoint metric | admission 중지 후 passive checkpoint |
| v1 identity 오매핑 | deterministic key와 operator diff | production 불변, mapping 수정 후 재import |
| secret payload 유입 | typed Secret Ref와 canary scan | 오염 shadow 폐기, Secret Store rotate |
| catalog/control cascade | FK 방향 test | catalog만 rebuild, control DB rollback 금지 |
| applied migration 오류 | online backup, checksum journal, forward-only migration | cutover 전 shadow 폐기; cutover 후 forward-fix |

## Acceptance Criteria

- [ ] Connection과 DatabaseTarget이 별도 ID/테이블/API model이다.
- [ ] published Plan Revision과 membership이 불변이다.
- [ ] scheduled/manual occurrence가 각각 멱등하다.
- [ ] Plan Occurrence가 부분 성공을 명시적으로 표현한다.
- [ ] 모든 장기 operation을 Work Item에 연결할 수 있다.
- [ ] stale fence와 confirmation replay가 DB/repository에서 거부된다.
- [ ] Snapshot의 네 상태 축이 독립적으로 query된다.
- [ ] Catalog Projection 전체 rebuild가 Control State를 변경하지 않는다.
- [ ] live Operational Store backup과 별도 경로 restore rehearsal이 통과한다.
- [ ] v1 shadow import/diff가 멱등이고 actual cutover는 수행하지 않는다.

## Files to Create/Modify

```text
crates/dbbackup-domain/Cargo.toml
crates/dbbackup-domain/src/{lib,ids,entities,state,errors}.rs
crates/dbbackup-catalog/src/{lib,connection,writer}.rs
crates/dbbackup-catalog/src/migrations/{mod,v2}.rs
crates/dbbackup-catalog/src/repositories/{configuration,runtime,activity,certification,notification,catalog,migration}.rs
crates/dbbackup-catalog/tests/{schema_v2,domain_invariants,repository_cas,catalog_rebuild,online_backup}.rs
crates/dbbackup-config/src/{migrate,schema}.rs
crates/dbbackup-config/tests/v1_to_v2_golden.rs
crates/dbbackup-cli/src/commands/{config_migrate,control_store}.rs
Cargo.toml
Cargo.lock
```

정본 planning DDL `docs/plan/connected-backup-platform-v2/schema-v2.sql`은 migration source와
checksum test의 입력이다. 구현 중 의미가 바뀌면 DDL, 이 섹션, ADR, domain dictionary를 같은
변경에서 갱신한다.
