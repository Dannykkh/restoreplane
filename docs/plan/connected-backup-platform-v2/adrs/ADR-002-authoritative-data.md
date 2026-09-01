# ADR-002: Authoritative Data Ownership by Bounded Context

- 상태: 승인됨
- **날짜**: 2026-07-26
- **결정자**: DbBackup project owner
- **태그**: operational-store, manifest, backup-scope, scheduler, retention

## Context

“Manifest가 진실 소스”와 “SQLite가 진실 소스”를 구분하지 않으면 Catalog rebuild가 Plan이나
queue까지 복구할 수 있다는 잘못된 기대가 생긴다. 프로그램 소유 scheduler와 durable runtime은
artifact만으로 재구성할 수 없는 실행 의도를 가진다. 반대로 Operational Store만 남고 signed
artifact가 없다면 복원 가능한 Recovery Point를 증명할 수 없다.

## Decision Drivers

- configuration/control state와 rebuildable artifact projection의 권위를 분리한다.
- Operational Store 손실 및 Destination 손실 각각의 복구 범위를 정직하게 설명한다.
- Scope, schedule, queue, retention 변경이 Catalog rebuild에 의해 덮이지 않게 한다.
- Recovery Point 승격은 signed Manifest와 destination evidence에 묶는다.

## Considered Options

| 옵션 | 장점 | 단점 |
|---|---|---|
| 모든 상태를 Operational Store만의 진실로 취급 | transaction과 query가 단순하다 | Store 손실 시 정상 artifact도 발견·복원할 근거가 사라진다 |
| signed Manifest를 시스템 전체의 진실로 취급 | Destination scan으로 artifact를 찾기 쉽다 | credential, Plan, Scope, schedule cursor, queue와 retention intent를 복구할 수 없다 |
| bounded context별 권위 소스를 분리 | 각 손실 시나리오와 rebuild 범위가 명확하다 | control backup, projection reconcile, format/schema version을 별도로 관리해야 한다 |

## Decision

bounded context별 권위 소스를 다음처럼 분리한다.

| 데이터 | 권위 소스 | 복구 경계 |
|---|---|---|
| Workspace, Connection, Database Target, Destination metadata | Operational Store | 암호화 export/import와 control backup |
| immutable Backup Scope와 discovered object inventory | Operational Store | control backup; Manifest는 실행 당시 Scope evidence만 보존 |
| published Plan Revision, schedule, scheduler cursor | Operational Store | control backup; Manifest scan으로 재구축하지 않음 |
| Work Item, Attempt, Lease, Run, resource permit, Activity | Operational Store | fencing reconciliation과 control backup |
| retention policy, tombstone, hold/lock 판단과 maintenance cursor | Operational Store | control backup과 destination observation |
| credential와 key material | service-owned Secret Store | DPAPI/KEK/Recovery Code 정책 |
| Recovery Point identity, part checksum, tool/engine/Scope evidence | trusted signed Set Manifest와 destination objects | bounded reader scan과 signature 검증 |
| Snapshot, Replica 검색 projection | Operational Store projection | signed Manifest와 destination evidence에서 rebuild/reconcile |
| verification 검색 projection | Operational Store projection | 해당 Recovery Point에 결합된 terminal local `verification_runs`를 replay; trusted external evidence가 없으면 unverified |

한 SQLite 파일을 사용할 수 있지만 control tables와 rebuildable projection tables의 migration,
삭제, rebuild 권한을 분리한다. `catalog rebuild`는 control tables를 수정하지 않는다. complete
Recovery Point를 Store에 반영할 때는 persisted Run/Scope intent와 verified Manifest를 함께
검증한다.

verification 완료는 fenced terminal Run 갱신과 deterministic evidence projection을 같은
transaction에서 기록한다. 성공한 evidence만 Snapshot의 verified 요약을 올리며, 실패·취소·
unsupported는 이력만 남긴다. Catalog rebuild는 재인입된 Recovery Point에 결합된 local terminal
Run을 replay하고, Manifest만으로 검증 성공을 추정하지 않는다.

## Consequences

### Positive

- artifact는 Operational Store 손실 뒤에도 trusted Manifest에서 검색·복원 후보로 재구축할 수 있다.
- Plan, Scope, schedule, queue, retention은 artifact projection 작업으로 변경되지 않는다.
- Manifest format version과 SQLite schema version을 독립 관리할 수 있다.

### Negative

- Operational Store control backup 없이는 프로그램 configuration과 schedule을 완전 복구할 수 없다.
- Store와 Destination 사이의 reconciliation 및 Attention 상태가 필요하다.

### Risks

- “Manifest가 진실”이라는 축약 표현이 전체 configuration 권위로 오해될 수 있다. 문서와 UI는
  artifact truth와 control-state authority를 구분한다.
- stale Manifest가 current Scope나 Plan을 덮을 수 있다. immutable revision/run binding 검증 후
  projection만 갱신한다.

## Related

- [ADR-001: Windows Service Process as the Single Execution Authority](ADR-001-execution-authority.md)
- [ADR-003: At-least-once Attempts with Fenced Commit](ADR-003-fenced-delivery.md)
- [ADR-004: Recovery Point and Artifact Replica Semantics](ADR-004-recovery-point-and-replicas.md)
- [ADR-009: Separate Install and Durable Data Roots](ADR-009-separate-install-and-data-roots.md)
- [ADR-010: Manifest-last Commit Recovery](ADR-010-manifest-last-commit-recovery.md)
- [Program-owned backup engine](../../program-owned-backup-engine.md)
