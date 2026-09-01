# Section 09 — New Engine Wave 1: SQL Server and SQLite

## Background

지원 DB 수를 늘리는 것은 제품 경쟁력에 중요하지만, export command가 있다는 이유만으로
지원이라고 표시하면 복구 제품의 신뢰를 훼손한다. Wave 1은 공통 provider conformance contract를
그대로 적용해 SQL Server BACPAC 후보와 SQLite online backup을 검증한다.

SQL Server DACPAC은 schema-only이므로 데이터 백업 수단으로 사용하지 않는다. `.bak`은
server-side path/permission과 향후 Agent 모델이 필요하므로 초기 desktop-first 범위에 넣지 않는다.

## Requirements

### SQL Server BACPAC candidate

- `sqlpackage` identity/version/hash/signature와 SQL Server version/edition 조합을 certification한다.
- export→Artifact/Manifest→새 DB import→content oracle를 실제 fixture에서 수행한다.
- schema/data는 포함하지만 login, server role, SQL Agent job, 일부 permission/unsupported object,
  encrypted object 등 BACPAC 제한을 Engine Matrix에 명시한다.
- 대용량/timeout/temp space/transaction consistency를 측정하고 지원 상한을 evidence로 둔다.
- fixture 전체가 통과하기 전 상태는 candidate/limited이며 기본 Plan 선택을 막거나 강한 제한을
  표시한다.
- DACPAC은 별도 `schema_export` capability일 뿐 backup/restore 지원 근거가 아니다.

### SQLite

- SQLite ConnectionProfile은 file path와 ACL/source identity를 사용하고 server discovery 대신
  파일 선택, header, lock/WAL preflight를 제공한다.
- live DB는 단순 파일 copy가 아니라 SQLite online backup API 또는 검증된 `.backup`을 사용한다.
- WAL, active writer/reader, long transaction, busy timeout, disk full을 fixture로 검증한다.
- restore는 기본 새 파일이며 integrity/FK/application oracle를 실행한다.
- overwrite는 source/target path identity, same-file, symlink/reparse, ACL, free space를 검사하고
  temp restore→fsync→atomic replace를 사용한다. 실패하면 원본을 보존한다.

### Engine Matrix

각 engine/format/server/tool/OS 조합에 probe, discovery, preflight, backup, L1/L2, restore,
validation, L3와 limitation/evidence를 기록한다. 지원 등급은 행의 최소 상태보다 높을 수 없다.

## Dependencies

- **Requires**: section-06 provider contract와 G2
- **Blocks**: section-10 expansion/launch
- **Parallel**: section-08 UI와 병렬; UI에는 data-driven capability만 추가한다.

## Flow Diagram Nodes

별도 신규 user flow를 만들지 않는다. section-06의 provider backend와
`snapshot-restore.mmd`를 그대로 통과해야 한다. 엔진별 예외를 UI 분기로 하드코딩하지 않고
Engine Matrix limitation으로 표시한다.

## Reference Libraries and Tools

| Tool/API | 검증 |
|---|---|
| Microsoft `SqlPackage` BACPAC | 구현 시 공식 문서/지원 버전/exit behavior 확인 |
| SQL Server test image/instance | edition/version/image digest pin |
| SQLite online backup API via `rusqlite` | 0.31 bundled API와 공식 SQLite 문서 확인 |
| filesystem atomic replace/Windows path APIs | 공식 문서와 reparse/ACL fixture |

## Implementation Details

### 1. Provider registration

`Engine` enum 추가만 하지 말고 provider factory, domain capability, Connection form schema,
tool inventory, certification fixture, Manifest engine metadata, restore mapping을 한 vertical
slice로 연결한다.

### 2. SQL Server prototype

non-production candidate flag 아래 probe/discovery/BACPAC export/import를 구현한다. fixtures는
기본 table/data, views/procedures, large object, unsupported objects, users/permissions,
encrypted features를 포함한다. import 뒤 schema/object/row/content oracle와 누락 report를 만든다.

### 3. SQLite provider

file identity를 canonical path+volume/file identity로 만들고 online backup progress/cancel을
Process Supervisor가 아닌 bounded operation으로 실행한다. output도 일반 Part/Manifest/Replica
pipeline으로 넘긴다. restore atomic replace는 confirmation 뒤에도 마지막 순간 identity를
재확인한다.

### 4. Certification promotion

managed evidence publisher가 Engine Matrix row를 생성한다. candidate를 supported로 바꾸는
변경은 claims matrix, docs/UI, fixture evidence를 같은 source revision에서 갱신한다.

## Test Scenarios

### SQL Server

| ID | 입력 | 기대 |
|---|---|---|
| S09-S01 | 기본 fixture BACPAC round trip | row/object oracle 통과 |
| S09-S02 | login/Agent job 포함 | limitation에 누락 명시, false full recovery 없음 |
| S09-S03 | unsupported/encrypted object | preflight 또는 export 분류, 지원 범위 축소 |
| S09-S04 | DACPAC만 생성 | data backup claim/Recovery Point 없음 |
| S09-S05 | tool hash/version mismatch | process 0회 |
| S09-S06 | 대용량/timeout/disk pressure | bounded resource와 정확한 실패/evidence |

### SQLite

| ID | 입력 | 기대 |
|---|---|---|
| S09-L01 | WAL active writer 중 backup | 일관된 새 파일 restore/oracle |
| S09-L02 | long transaction/lock | busy policy대로 대기/실패, 손상 artifact 없음 |
| S09-L03 | backup 중 disk full | incomplete 격리, source 불변 |
| S09-L04 | 새 파일 restore | integrity/FK/oracle 통과 |
| S09-L05 | overwrite identity mismatch/symlink | replace 0, 원본 보존 |
| S09-L06 | atomic replace 중 fault | 원본 또는 완전 새 파일 중 하나, partial 없음 |

## Implementation Strategy

### Red

provider conformance와 limitation fixture, DACPAC false-claim, SQLite WAL/lock/disk/atomic fault를
먼저 실패시킨다.

### Green

candidate SQL Server BACPAC과 SQLite create-new backup/restore를 구현하고 Engine Matrix에
실제 evidence를 기록한다.

### Refactor

tool invocation, file identity, certification publishing을 공통 provider contract로 정리한다.
엔진 특수 옵션은 typed config로 제한한다.

## Quality Gate G4

- [ ] SQL Server BACPAC 실제 export/import/oracle 통과 범위가 문서와 일치
- [ ] DACPAC data backup claim 0
- [ ] unsupported/login/Agent/permission/encryption limitation 표시
- [ ] SQLite WAL/lock/long transaction backup/restore 통과
- [ ] SQLite overwrite fault에서 원본 손상 0
- [ ] 두 엔진 모두 Manifest/Replica/verification/restore 공통 흐름 사용
- [ ] evidence 없는 조합의 supported 승격 0

## Risk and Rollback

| Risk | Mitigation | Rollback |
|---|---|---|
| BACPAC 과대 기대 | candidate/limitation/oracle | engine experimental 강등 |
| `.bak` 무리한 포함 | Agent 전 명시 제외 | capability unavailable |
| SQLite live copy 손상 | online backup only | provider disable |
| overwrite 원본 손실 | temp+fsync+atomic+identity | overwrite disable/create-new 유지 |
| tool version drift | certification allowlist | known tool로 pin |

## Acceptance Criteria

- [ ] SQL Server와 SQLite가 기존 엔진과 같은 지원 계약으로 평가된다.
- [ ] UI가 엔진 수보다 실제 지원 수준과 제한을 정직하게 보여준다.
- [ ] SQL Server BACPAC과 SQLite online backup의 복원 evidence가 있다.
- [ ] 미통과 조합은 supported가 아니다.

## Files to Create/Modify

```text
crates/restoreplane-core/src/provider.rs
crates/restoreplane-source-sqlserver/src/{lib,probe,discover,bacpac,restore,validate}.rs
crates/restoreplane-source-sqlite/src/{lib,probe,backup,restore,validate}.rs
crates/restoreplane-source-{sqlserver,sqlite}/tests/conformance/
apps/console/src/screens/{Connections,Databases,BackupPlans}.tsx
scripts/test-engine-wave1.ps1
Cargo.toml
Cargo.lock
```
