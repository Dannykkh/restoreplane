# Connected Backup Platform v2 Flow Index

이 디렉터리는 구현, 섹션, IPC, QA가 공유하는 상태 전이의 정본이다. 각 흐름은
정상 경로뿐 아니라 실패, 재시도, stale worker, 부분 artifact, 안전한 rollback을 포함한다.

| ID | 흐름 | 시작 | 성공 종점 | 주요 실패·경계 |
|---|---|---|---|---|
| F01 | [Connection Onboarding](connection-onboarding.mmd) | 연결 추가 | Connection 사용 가능 | probe 실패, discovery drift |
| F02 | [Scheduled Backup Dispatch](scheduled-backup-dispatch.mmd) | scheduler tick | Recovery Point commit 진입 | 중복 occurrence, permit 대기, native process 실패 |
| F03 | [Recovery Point Commit](recovery-point-commit.mmd) | artifact staging 완료 | 검색·복원 가능한 Snapshot | 누락 part, required destination 실패, stale fencing |
| F04 | [Crash Reconciliation](crash-reconciliation.mmd) | service boot 또는 sweep | 정상 감시·terminal 상태 수렴 | 만료 lease, 고아 process, partial artifact |
| F05 | [Snapshot Restore](snapshot-restore.mmd) | Snapshot 선택 | 검증된 Restore Run | 부적격 replica, preflight 실패, overwrite 불일치 |
| F06 | [Configuration Cutover](config-cutover.mmd) | v2 전환 시작 | v2 execution authority 확정 | shadow diff, G1 미통과, service unhealthy |

## 공통 불변조건

- Plan Occurrence는 멱등이며 Attempt는 at-least-once로 중복될 수 있다.
- Fencing Token이 stale인 Attempt는 heartbeat, 상태 전이, commit을 수행할 수 없다.
- Recovery Point completeness, Artifact Replica durability, Protection Compliance를 한 상태로
  축약하지 않는다.
- complete Recovery Point와 적격 Replica가 하나 이상인 경우에만 Snapshot을 검색·복원할 수
  있다.
- 덮어쓰기 복원과 cutover는 대상과 만료가 바인딩된 명시적 확인 및 감사 Activity를 남긴다.
- service와 legacy scheduler가 동시에 production execution authority가 되는 순간은 없다.

## 후속 산출물 매핑

| Flow | 구현 섹션 | 주요 IPC | QA 축 |
|---|---|---|---|
| F01 | Domain/Store, UI Foundation | `connection.*` | 연결 실패, capability, drift |
| F02 | Durable Dispatcher | `plan.run-now`, `work.*` | 동시 due, admission, retry |
| F03 | Recovery/Storage | `snapshot.*`, `replica.repair` | partial upload, compliance |
| F04 | Durable Dispatcher | `service.status`, `work.*` | kill/restart, orphan, fencing |
| F05 | Existing Engine Certification, Recovery UI | `restore.*`, `confirmation.*` | preflight, overwrite, verification |
| F06 | Truth/Release Gates, Service/IPC | `migration.*`, `service.status` | rehearsal, power loss, old-binary guard |
