# Section 05 — Recovery Point, Artifact, and Storage Lifecycle

## Background

v1의 signed Manifest, encryption pipeline, manifest-last fan-out, Local/S3/SSH reader는 유지할
가치가 높다. v2에서는 이를 단일 payload 성공이 아니라 복원 가능한 원자 단위와 다중 목적지
내구성으로 명확히 모델링한다.

- Recovery Point: 한 Target의 한 시점에 필요한 모든 Artifact Part를 묶은 복구 단위
- Artifact Replica: 한 Destination에 commit된 Recovery Point 사본
- Snapshot: complete Recovery Point의 UI/search projection

complete, durable, compliant, eligible은 같은 상태가 아니다. required NAS가 실패했더라도
valid Local Replica가 있으면 Recovery Point는 complete하고 복원 가능할 수 있지만
Protection Compliance는 실패한다.

## Requirements

### Artifact creation

- 한 Target dump는 한 번 만들고 encode 후 여러 Destination으로 fan-out한다.
- DB dump, PostgreSQL globals, metadata 등 engine policy가 요구하는 Part를 명시한다.
- 각 Part는 logical role, format, size, plaintext/cipher checksum, encryption metadata를 가진다.
- required Part 하나라도 누락·손상되면 complete Recovery Point를 만들지 않는다.
- spool path는 Attempt별 격리되고 section-04 reservation/fence를 사용한다.

### Manifest and commit

- payload/part manifest를 먼저 쓰고 signed set Manifest를 마지막 logical commit marker로 쓴다.
- signing key ID, schema version, source identity, occurrence/run, engine/tool/options, Part 목록,
  Destination role을 서명 범위에 포함한다.
- current fence와 valid signature, required Part를 다시 확인한 뒤에만 Run과 Catalog Projection을
  terminal commit한다.
- Manifest publish 뒤 DB crash는 동일 Recovery Point ID로 rebuild한다.
- stale Attempt와 invalid/unknown signing key는 commit하지 않는다.

### Multi-destination durability

- Plan membership은 `required` 또는 `best_effort`; 기본 정책은 모든 required 성공이다.
- Replica는 commit/missing/corrupt/quarantined, reader eligibility, immutability, retention을
  개별 기록한다.
- complete Recovery Point와 적격 Replica 하나 이상은 `degraded` Snapshot으로 검색·복원할
  수 있지만 required 실패 시 `non_compliant`이고 “protected”를 금지한다.
- Replica repair는 기존 적격 encoded object를 재사용하고 전체 DB dump를 다시 만들지 않는다.
- writer, restore reader, delete operator credential을 분리한다.

### Verification and catalog projection

- L1은 signature/schema/checksum/object inventory, L2는 engine tool 구조 검사, L3는 실제
  Restore Run과 content oracle evidence다.
- Snapshot projection은 completeness, durability, compliance, eligibility, highest level,
  freshness를 별도 컬럼으로 제공한다.
- Catalog Projection은 Destination inventory의 signed Manifest에서 전량 재구축할 수 있다.
- incomplete/orphan은 기본 복원 목록에서 숨기고 diagnostics에서만 노출한다.

### Retention and deletion

- retention은 dry-run→tombstone→grace/lock 확인→delete Work Item→observation/retry 순서다.
- pinned, legal hold, Object Lock, 마지막 compliant Recovery Point, active restore/repair source는
  자동 삭제하지 않는다.
- SQLite cascade로 실제 Destination object를 삭제하지 않는다.
- partial delete는 성공 객체만 projection에 반영하고 실패를 retry/Attention으로 남긴다.

## Dependencies

- **Requires**: section-03 service/secret boundary, section-04 durable runtime
- **Blocks**: section-06 existing engine certification
- **Parallel**: Destination adapter별 writer/reader/delete 구현은 공통 contract와 fixture를
  고정한 뒤 병렬화한다.

## Flow Diagram Nodes

### `scheduled-backup-dispatch.mmd`

- `stageArtifacts`: fenced spool을 Part/Manifest 후보로 전환
- `commitFlow`: `recovery-point-commit.mmd`로 인계

### `recovery-point-commit.mmd`

`staged`, `validateParts`, `quarantine`, `incomplete`, `signManifest`, `uploadReplicas`,
`evaluateRequired`, `fencedCommit`, `anyEligible`, `scheduleRepair`, `fencedDegraded`,
`rejectStale`, `reconcile`, `commitProtected`, `commitDegraded`, `protectionFail`,
`publishSnapshot`, `complete` 전체를 소유한다. `validateParts=아니요`는 quarantine/incomplete,
required Replica가 부족하지만 적격 사본이 있으면 fenced degraded commit, 적격 사본도 없으면
repair 후에도 기본 복원 목록에서 제외한다. `fencedCommit/fencedDegraded=아니요`는 stale
reject와 Reconciliation이다.

## Reference Libraries

| Library | 기준 | 용도 |
|---|---|---|
| `blake3` | workspace pin | content/Manifest digest |
| 현행 ChaCha20 STREAM encryption | 기존 artifact 호환 유지 | per-backup DEK encryption |
| `zstd` | workspace pin | compression |
| `ed25519-dalek` 또는 현행 signer | lockfile pin | Manifest signature |
| `tokio`/`async-trait` | workspace pin | bounded fan-out/adapters |
| S3/SSH crates | 현행 exact/lockfile | writer/reader/delete |
| `serde` | workspace pin | versioned Manifest |

## Implementation Details

### 1. Provider-neutral artifact contract

`restoreplane-core`에 `ArtifactSpec`, `PartSpec`, `ConsistencyGrade`를 두고
`restoreplane-pipeline`에 staged/encoded/committed state를 둔다. engine provider는 필요한 Part와
restore pairing을 선언하고 storage adapter는 engine을 알지 않는다.

### 2. Manifest v2

v1 reader 호환 정책을 명시하고 v2 set Manifest를 additive version으로 만든다. canonical
serialization golden vector, signature/key rotation, unknown field/version behavior를 테스트한다.
Manifest/object key는 occurrence/Target/Recovery Point 기반 deterministic prefix를 사용한다.

### 3. Encode and fan-out

dump stream→hash→compression→encryption을 bounded pipeline으로 처리한다. encoded spool을
required/best-effort adapter에 보내고 각 결과를 독립 기록한다. Destination 성공은 payload와
Manifest reader round-trip이 모두 통과해야 한다.

### 4. Commit and projection

adapter 결과에서 signed set Manifest를 읽어 검증한 뒤 current fence 조건으로
`backup_runs`, `catalog_recovery_points/parts/replicas/objects`, Snapshot/Target protection을
한 transaction에 반영한다. DB 실패가 artifact truth를 지우지 않게 한다.

### 5. Repair/rebuild

repair는 source Replica eligibility와 Destination preflight를 다시 검사하고 Operation Run으로
enqueue한다. rebuild는 dry-run에서 accepted/rejected/unknown key를 보여준 뒤 merge 또는
projection replace를 수행하되 Control State를 건드리지 않는다.

### 6. Retention

policy evaluator와 destructive executor를 분리한다. dry-run 결과에 reason과 compliance impact를
담고 confirmation 이후 tombstone을 기록한다. delete credential은 backup writer와 분리하며
Object Lock 거부를 실패가 아닌 보존 근거로 분류한다.

## Test Scenarios

### Normal

| ID | 입력 | 기대 |
|---|---|---|
| S05-N01 | required DB dump+globals와 Local/NAS 성공 | complete, healthy, compliant, eligible |
| S05-N02 | Local 성공/NAS required 실패 | complete, degraded, non-compliant, 적격 Local 복원 가능 |
| S05-N03 | NAS 복구 후 repair | 재덤프 없이 Replica commit과 compliant 전환 |
| S05-N04 | Catalog 전량 삭제 후 rebuild | 동일 RP/Part/Replica와 Control State 불변 |
| S05-N05 | retention dry-run과 승인 delete | 후보/실제 삭제 일치, 감사 evidence |

### Error

| ID | 입력 | 기대 |
|---|---|---|
| S05-E01 | required Part 누락 | incomplete, Snapshot 기본 목록 제외 |
| S05-E02 | signature invalid/unknown key | quarantine, restore blocked |
| S05-E03 | payload 성공/Manifest upload 실패 | Replica commit 아님, orphan 진단 |
| S05-E04 | writer 성공/reader probe 실패 | Replica ineligible, plan/restore 차단 |
| S05-E05 | stale fence commit | DB terminal/projection update 0 |
| S05-E06 | last compliant RP retention 후보 | delete 제외와 reason |

### Edge/Fault

| ID | 입력 | 기대 |
|---|---|---|
| S05-X01 | Manifest publish 직후 service kill | rebuild로 하나의 RP |
| S05-X02 | multi-part 중 한 object corrupt | complete 금지 또는 기존 RP degraded/blocked |
| S05-X03 | Destination duplicate response/version | object/Replica idempotent |
| S05-X04 | repair source가 작업 중 unavailable | bounded retry, 기존 Snapshot 상태 유지 |
| S05-X05 | partial retention delete | 성공/실패 객체별 추적과 retry |
| S05-X06 | Object Lock retention 이후 삭제 요청 | 보존, 정책 위반/오류 구분 |

## Implementation Strategy

### Red

canonical Manifest vector, Part completeness, required Replica truth, stale commit, crash gap,
rebuild/control isolation, retention safety test를 먼저 실패시킨다.

### Green

v2 artifact/Manifest state와 current adapters를 연결하고 all-required projection, repair,
dry-run retention을 구현한다.

### Refactor

engine/storage 책임, signer/serializer, projection calculator, retention evaluator를 pure boundary로
분리한다. adapter별 성공 의미를 공통 conformance suite로 통일한다.

## Quality Gate

- [ ] required Part 누락/invalid signature의 false Snapshot 0
- [ ] occurrence/Target당 complete Recovery Point 최대 1
- [ ] required Replica 실패를 protected로 표시한 케이스 0
- [ ] writer와 reader credential 분리 fixture 통과
- [ ] crash gap 뒤 Manifest rebuild 통과
- [ ] Catalog rebuild의 Control State mutation 0
- [ ] repair가 기존 encoded artifact를 사용
- [ ] retention dry-run/hold/lock/partial delete matrix 통과
- [ ] L1과 실제 reader round-trip evidence 통과

## Risk and Rollback

| Risk | Mitigation | Rollback/Forward action |
|---|---|---|
| Manifest 호환 파괴 | versioned additive schema/golden vector | 구 reader 유지, 새 writer disable |
| false completeness | required Part+signature+fenced commit | quarantine/rebuild |
| false protection | 4축 projection | compliance 재계산 |
| orphan 증가 | manifest-last, sweep, diagnostics | 안전한 cleanup Work Item |
| retention 과삭제 | dry-run/tombstone/confirmation/hold | delete 중지; immutable object는 유지 |
| adapter 의미 불일치 | conformance suite | 해당 adapter experimental 강등 |

## Acceptance Criteria

- [ ] Recovery Point, Part, Replica, Stored Object가 실제 artifact와 일치한다.
- [ ] completeness/durability/compliance/eligibility가 독립 계산된다.
- [ ] degraded Snapshot의 복원 가능성과 보호 위반을 동시에 정직하게 표시할 수 있다.
- [ ] repair/rebuild/retention이 durable Operation Run으로 감사된다.
- [ ] Catalog 손실로 control/restore history가 손상되지 않는다.

## Files to Create/Modify

```text
crates/restoreplane-core/src/{artifact,capability}.rs
crates/restoreplane-pipeline/src/{manifest,encode,fanout,commit,repair,retention}.rs
crates/restoreplane-pipeline/tests/{manifest_v2,commit_faults,replica_truth,retention}.rs
crates/restoreplane-catalog/src/repositories/catalog.rs
crates/restoreplane-dest-*/src/{writer,reader,delete}.rs
crates/restoreplane-application/src/use_cases/{snapshot,repair,retention}.rs
```
