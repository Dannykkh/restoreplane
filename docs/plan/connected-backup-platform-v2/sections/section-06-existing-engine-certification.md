# Section 06 — Existing Engine Certification and Real Recovery

## Background

PostgreSQL, MySQL, MariaDB source adapters와 L1/decrypt-only 기반은 존재하지만 “지원”이라고
말하려면 실제 server/tool matrix, backup, 구조 검사, DB restore, content oracle가 같은 build에서
통과해야 한다. 이 섹션은 새 엔진을 추가하기 전에 세 기존 엔진의 지원 계약을 완성한다.

## Requirements

### Common provider contract

각 engine provider는 다음 operation과 capability evidence를 제공한다.

1. non-destructive probe와 server identity/version
2. database discovery와 stable object identity
3. tool path/version/hash/signature preflight
4. backup Part specification과 Consistency Grade
5. L1/L2 verification
6. create-new restore와 overwrite safety policy
7. engine-specific validation/content oracle
8. L3 restore drill evidence와 limitation

`supported`는 요구 행이 모두 통과한 certification에만 사용한다. 그 외는 candidate,
experimental, limited, unsupported 중 정확한 상태를 표시한다.

### PostgreSQL

- `pg_dump`/`pg_restore` version compatibility와 custom/plain format을 구분한다.
- DB dump와 복원에 필요한 globals/role evidence를 동일 Recovery Point Part로 pairing한다.
- snapshot/transaction consistency, extension, ownership/ACL, large object 제한을 기록한다.
- create-new DB restore를 기본으로 하고 overwrite는 active session/target identity/preflight와
  confirmation을 요구한다.

### MySQL and MariaDB

- 두 engine을 같은 binary 이름만으로 합치지 않고 별도 server/tool certification으로 관리한다.
- routines, triggers, events, views, charset/collation, GTID/definer, single-transaction 조건을
  fixture로 검증한다.
- MyISAM 등 transactional consistency가 보장되지 않는 조합은 낮은 Consistency Grade와 제한을
  표시한다.

### Verification and restore

- L1: signature, Manifest schema, object/checksum/size
- L2: native tool이 실제 dump 구조를 읽고 engine-specific sanity check
- L3: 격리된 새 DB restore와 결정적 content oracle; Restore Run과 evidence 필수
- process exit 0 뒤 validation 실패는 성공이 아니다.
- 적격 reader Replica와 valid Manifest를 restore 직전에 다시 검사한다.
- overwrite는 target fingerprint 변경, token mismatch/expiry/replay에서 native process를
  시작하지 않는다.

### Notification and operational contract

- backup/verification/restore/compliance event와 notification rule을 durable outbox로 연결한다.
- delivery는 자체 lease/fencing/retry/dedupe를 사용하고 실패가 원 작업 결과를 바꾸지 않는다.
- RPO violation과 notification failure를 별도 Activity/Attention으로 유지한다.
- native tool trust, redacted diagnostics, support bundle evidence를 제공한다.

## Dependencies

- **Requires**: section-05 artifact/storage lifecycle
- **Blocks**: section-08 Recovery UI, section-09 new engine Wave 1
- **Parallel**: PostgreSQL, MySQL, MariaDB fixture는 공통 provider conformance suite가 고정된 뒤
  병렬 실행한다.

## Flow Diagram Nodes

`snapshot-restore.mmd`의 backend node를 소유한다.

| Node | 책임 |
|---|---|
| `loadEligibility`, `eligible` | fresh Manifest/Replica/certification/verification 조회 |
| `blockRestore` | 부적격 source를 process 시작 전 차단 |
| `runPreflight`, `preflightOk`, `recordPreflight` | tool/space/permission/version/target 검사 |
| `createRestore` | immutable Restore Run과 Work Item 생성 |
| `executeRestore` | provider와 Process Supervisor 실행 |
| `verifyResult`, `verified` | engine validation/content oracle |
| `markAttention`, `success`, `failed` | 성공·실패·cleanup required evidence |

UI 선택/overwrite 확인 node는 section-08이 소유한다.

## Reference Libraries and Tools

| Tool/Library | 검증 범위 |
|---|---|
| `pg_dump`, `pg_restore`, `psql` | 지원 server/client 조합을 Engine Matrix에 exact 기록 |
| `mysqldump`, `mysql` | MySQL certification |
| `mariadb-dump`, `mariadb` | MariaDB certification |
| `tokio` | process/timeout/stream |
| `rusqlite` | certification/run/evidence/outbox |
| Docker/Testcontainers 또는 승인 testbed | 구현 시 공식 버전과 image digest pin |

도구 옵션은 구현 직전 각 vendor 공식 문서를 확인하며 기억으로 지원 범위를 확장하지 않는다.

## Implementation Details

### 1. Provider interface

`Probe`, `Discovery`, `Backup`, `Verification`, `Restore`, `Validation` trait를 engine-neutral
request/result로 정의한다. UI나 pipeline이 engine option 문자열을 직접 조립하지 않는다.
capability와 limitation은 code+parameters로 반환한다.

### 2. Tool inventory and certification

실행 전 path canonicalization, file hash, Authenticode/publisher, version을 저장하고 certification과
매칭한다. path swap/unknown version/unsigned policy 위반은 process 시작 전에 차단한다.
certification evidence는 source revision, build ID, OS/architecture, server/tool, fixture, result를
가진다.

### 3. PostgreSQL implementation

Connection probe와 DB discovery를 실제 command/API로 연결한다. dump Part와 globals Part를
만들고 custom format L2를 `pg_restore --list` 계열 실제 구조 검사로 구현한다. L3는 격리
cluster/database에 restore하고 schema/object/row/content hash oracle을 검증한다.

### 4. MySQL/MariaDB implementation

각 tool/version 옵션 builder를 분리한다. transactional option, routines/triggers/events,
charset/definer fixture를 포함하고 restore 뒤 table/view/routine/row oracle을 실행한다.
stderr warning을 성공으로 숨기지 않고 분류한다.

### 5. Restore Safety Policy

provider가 create-new/overwrite mapping, forbidden system DB, connection kill/maintenance 요구,
cleanup policy를 선언한다. application preflight는 snapshot source와 target identity가 혼동되지
않는지 검사한다. 실패/취소 시 생성 target을 자동 파괴하지 않고 `cleanup_required`와 runbook을
남긴다.

### 6. Verification scheduler and outbox

Plan verification policy가 backup commit 뒤 실제 Verification Run을 enqueue한다. L2/L3
unsupported면 조용히 L1로 낮추지 않는다. state/compliance transaction에서 outbox intent를
같이 생성하고 delivery worker는 별도 result를 기록한다.

## Test Scenarios

### Provider conformance

| ID | 입력 | 기대 |
|---|---|---|
| S06-N01 | 지원 PG fixture backup→L1/L2→새 DB restore→oracle | certification passed, L3 evidence |
| S06-N02 | 지원 MySQL fixture 전체 object | routines/triggers/events 포함 oracle 통과 |
| S06-N03 | 지원 MariaDB fixture | 별도 tool/server certification 통과 |
| S06-N04 | required Replica 중 적격 reader 선택 | fresh Manifest 재검증 후 restore |
| S06-N05 | backup commit event | policy에 따른 verification/outbox enqueue |

### Error

| ID | 입력 | 기대 |
|---|---|---|
| S06-E01 | unsupported server/tool 조합 | preflight 차단, supported claim 없음 |
| S06-E02 | tool hash/path 변경 | process 0회, Tool Health Attention |
| S06-E03 | L2/L3 요청에 실행 경로 없음 | explicit unsupported/non-zero, L1 downgrade 없음 |
| S06-E04 | restore exit 0, oracle mismatch | Restore Run 실패/cleanup required |
| S06-E05 | Manifest signature/reader eligibility 실패 | restore process 0회 |
| S06-E06 | notification endpoint 실패 | 원 작업 결과 불변, delivery retry/Attention |

### Edge/Fault

| ID | 입력 | 기대 |
|---|---|---|
| S06-X01 | PG globals 필요한 role/ownership | paired Part 없으면 eligibility 차단 |
| S06-X02 | MySQL non-transactional table | 낮은 Consistency Grade와 limitation |
| S06-X03 | overwrite 전 target fingerprint 변경 | token 무효, process 0회 |
| S06-X04 | restore 중 service kill | Reconciliation과 cleanup evidence |
| S06-X05 | outbox delivery 뒤 DB commit crash | dedupe/provider 결과로 bounded 재전송 |
| S06-X06 | server/tool patch version 경계 | 미인증이면 candidate/limited, 자동 supported 없음 |

## Implementation Strategy

### Red

공통 conformance suite, unsupported downgrade, tool swap, actual restore/oracle, globals pairing,
outbox crash test를 먼저 실패시킨다.

### Green

세 provider의 probe/discovery/backup/L2/create-new restore/validation과 policy-driven verification,
outbox worker를 연결한다.

### Refactor

option builder, process result classification, fixture oracle, certification publisher를 engine별로
분리하면서 공통 contract 결과 shape는 유지한다.

## Quality Gate G2

- [ ] PostgreSQL/MySQL/MariaDB 각각 실제 backup과 create-new restore/oracle 통과
- [ ] required provider capability의 Engine Matrix evidence 완전
- [ ] L2/L3 silent downgrade 0
- [ ] tool path/hash/signature swap 실행 0
- [ ] invalid Manifest/ineligible Replica restore 0
- [ ] overwrite binding mismatch 실행 0
- [ ] notification failure가 backup/restore 성공을 바꾼 사례 0
- [ ] fixture evidence가 같은 revision/build/tool/server를 기록

## Risk and Rollback

| Risk | Mitigation | Rollback |
|---|---|---|
| 광범위한 version claim | certification allowlist | 해당 조합 experimental 강등 |
| restore 파괴성 | create-new default+confirmation+preflight | overwrite disable |
| exit 0 false success | content oracle | failed/cleanup required 유지 |
| tool supply-chain | hash/signature inventory | tool 차단/known bundle로 복귀 |
| notification 폭증 | dedupe/cooldown/retry budget | channel disable, outbox 보존 |

## Acceptance Criteria

- [ ] 세 기존 엔진의 지원 범위가 실제 restore evidence와 일치한다.
- [ ] Connection probe/discovery가 stub이 아니다.
- [ ] Plan verification policy가 실제 Run을 만든다.
- [ ] Snapshot에서 ID/connection string 수동 입력 없이 안전한 restore가 가능하다.
- [ ] unsupported/limited 조합이 UI에서 명확하다.

## Files to Create/Modify

```text
crates/restoreplane-core/src/provider.rs
crates/restoreplane-source-pg/src/{probe,discover,backup,restore,validate}.rs
crates/restoreplane-source-mysql/src/{probe,discover,backup,restore,validate}.rs
crates/restoreplane-verify/src/{policy,l1,l2,l3}.rs
crates/restoreplane-application/src/use_cases/{connection,verification,restore}.rs
crates/restoreplane-runtime/src/notification.rs
crates/restoreplane-*/tests/provider_conformance/
scripts/test-existing-engines.ps1
```
