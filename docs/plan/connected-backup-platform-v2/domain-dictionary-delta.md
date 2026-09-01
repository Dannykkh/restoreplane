# Domain Dictionary Delta — Connected Backup Platform v2

> 생성일: 2026-07-26
> 마스터 사전: `docs/domain-dictionary.md`
> 상태: Phase 3 v3 확정

## v1 Additions

- ADD `Control Plane`, `Data Plane`: 제품 제어와 실제 DB 작업 실행 경계
- ADD `Connection Profile`, `Database Target`: 서버 접속과 선택된 DB 분리
- ADD `Engine Adapter`, `Capability`, `Support Contract`: 엔진별 기능과 지원 판정
- ADD `Backup Plan`, `Backup Run`, `Snapshot`, `Restore Run`: 선언·실행·산출물·복원 분리
- ADD `Dispatcher`, `Work Item`, `Lease`, `Resource Permit`: 중앙 queue와 부하 제어
- ADD `Activity Event`, `Restore Eligibility`: 이력과 복원 가능성의 명시적 상태
- ADD `Agent`: 향후 원격 Data Plane 실행기

## v1 Refinements

- REFINE `Job`: 신규 코드에서는 사용하지 않고 `Backup Plan` 또는 `Backup Run`으로 분리한다.
- REFINE `BackupSource`: dump 전용 기존 trait로 보존하고, 제품 지원 경계는 `Engine Adapter`가
  감싼다.
- REFINE `Catalog`: 매니페스트 검색 인덱스뿐 아니라 제어 상태를 담는 운영 SQLite와
  분리해 명명한다. 구현 설계에서 table ownership을 확정한다.
- REFINE `Test-restore`: `Restore Run`의 목적이 `verification`인 경우로 모델링한다.

## v1 → v2 Team Review Merge

- ADD `Plan Revision`, `Plan Occurrence`, `Attempt`, `Lease Heartbeat`, `Fencing Token`
- ADD `Operational Store`, `Process Supervisor`, `Source Resource Identity`, `Spool Reservation`,
  `Reconciliation`, `Transaction Owner`
- ADD `Recovery Point`, `Artifact Part`, `Artifact Replica`, `Required Destination`
- ADD `Protection Compliance`, `Protection Status`, `Attention State`, `Consistency Grade`
- ADD `Engine Matrix`, `Probe Result`, `Discovery Drift`, `RPO Violation`
- ADD `Restore Drill`, `Verification Policy`, `Preflight`, `Support Bundle`, `Orphan Artifact`
- REFINE `Lease`, `Snapshot`, `Restore Eligibility`, `Activity Event`, `Support Contract`

## v2 → v3 Conflict Resolution

- accepted-by-default: `Plan Occurrence`를 규범 용어로 채택하고 `Scheduled Occurrence`를 병합
- accepted-by-default: `Artifact Replica`를 규범 용어로 채택하고 `Destination Copy`를 병합
- inferred: required part의 원자 단위는 `Recovery Point`, UI·검색 표현은 `Snapshot`
- inferred: 하나의 Plan Occurrence가 Target별 Backup Run을 묶으며 occurrence 자체에
  `target_id`를 넣지 않는다.
- accepted-by-default: 기본 목적지 정책은 `all-required`
- accepted-by-default: Activity Event는 운영 append-only record이며 tamper-evident 감사
  로그를 의미하지 않는다.
- unresolved-conflict: 없음

## Resolved Conflicts

- `Snapshot`은 DB 엔진 자체 snapshot과 중의성이 있다. 사용자 UI에서는 익숙한 표현을
  유지하되 내부 복원 원자 단위는 `RecoveryPoint`를 사용한다.
- `Target`은 DB와 restore 목적 DB 모두에 쓰일 수 있다. 기본 명사는 `Database Target`,
  복원 문맥에서는 `Restore Destination Database`를 사용한다.

## Global Dictionary Sync

- inferred-skip: 모든 신규 용어는 이 프로젝트의 백업 운영 모델에 결합되어 있어
  `~/.agent-memory/domain-dictionaries/general.md`로 자동 반영하지 않는다.

## Completion

- 마스터 `docs/domain-dictionary.md`는 v4로 확정했다.
- 신규 용어는 프로젝트 특화이므로 글로벌 사전 동기화는 `inferred-skip`으로 유지한다.
