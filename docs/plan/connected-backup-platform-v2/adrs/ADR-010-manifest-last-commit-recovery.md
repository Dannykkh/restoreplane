# ADR-010: Recover Committing Attempts from Manifest-last Evidence

- **상태**: accepted
- **날짜**: 2026-08-24
- **결정자**: DbBackup project owner
- **태그**: crash-recovery, signed-manifest, metadata-probe, fencing, recovery-point

## Context

DB dump와 Destination publish는 SQLite transaction 밖에서 수행된다. payload가 게시된 뒤 또는
signed Set Manifest가 게시된 직후 service가 중단되면 Operational Store의 claim은
`committing`에 남을 수 있다. 이 상태에서 무조건 dump를 재실행하면 duplicate/orphan을 만들 수
있고, artifact를 확인하지 않고 complete로 바꾸면 복원 불가능한 Recovery Point를 노출한다.
대형 payload 전체를 service startup마다 읽고 hash하는 방식도 startup 시간을 데이터 크기에
종속시킨다.

## Decision Drivers

- occurrence당 하나의 complete Recovery Point와 current fence를 유지한다.
- signed artifact evidence가 충분할 때 이미 끝난 publish를 다시 실행하지 않는다.
- 대형 DB 크기와 무관한 bounded startup 시간을 유지한다.
- 모호한 evidence는 자동 추정하지 않고 operator Attention으로 보낸다.

## Considered Options

| 옵션 | 장점 | 단점 |
|---|---|---|
| expired `committing` claim을 항상 requeue해 dump 재실행 | recovery 구현이 단순하다 | 이미 게시된 artifact와 중복 작업, source 부하, orphan이 생긴다 |
| startup에서 모든 payload를 전체 read/hash | content까지 강하게 재검증한다 | startup latency와 원격 I/O가 DB 크기·claim 수에 비례한다 |
| persisted intent, signed last-marker, payload metadata를 bounded 검증 | 이미 게시된 commit을 안전하게 정산하고 payload-sized read를 피한다 | metadata 확인만으로는 L1 content verification을 대체할 수 없고 불확실한 claim은 Attention이 필요하다 |

## Decision

normal worker는 fenced claim을 `committing`으로 옮길 때 Recovery Point ID와 signed Manifest
object key를 Operational Store에 원자적으로 기록한다. 각 Destination은 payload를 먼저
게시·검증하고 canonical signed Set Manifest를 마지막 logical commit marker로 게시한다.

startup reconciliation은 최대 1,000개 expired claim을 살피며 commit-evidence 검증 전체가
공유하는 20초 budget을 사용한다. budget은 claim마다 다시 시작하지 않는다. `committing`
claim은 다음 조건을 모두 충족해야 정상 commit과 같은 immutable validator로 정산된다.

1. 모든 required Destination에서 bounded read한 canonical signed Manifest bytes가 동일하다.
2. trusted signature와 Recovery Point, occurrence, Run, Backup Scope, engine, Manifest key가
   persisted intent와 일치한다.
3. Manifest의 replica 집합이 immutable Plan Revision의 current Destination binding과 일치한다.
4. 각 payload를 metadata-only stat하고 object 존재와 signed ciphertext byte 수가 정확히
   일치한다. payload 본문은 startup에서 read/hash하지 않는다.

필수 evidence의 누락, mismatch, reader 불가, timeout은 complete나 automatic requeue 근거로
사용하지 않고 Attention으로 남긴다. metadata-only recovery는 이미 정상 publish 과정에서
검증된 commit을 bounded하게 정산하는 절차이며 L1 checksum verification의 대체가 아니다.

## Consequences

### Positive

- crash가 Manifest publish와 Store commit 사이에 발생해도 같은 Recovery Point ID로 정산할 수 있다.
- 대형 payload를 startup에서 다시 전송하거나 읽지 않는다.
- required replica와 Scope evidence가 다른 artifact를 잘못 승격하지 않는다.

### Negative

- Destination reader와 trusted signing key가 startup에 준비되지 않으면 자동 정산할 수 없다.
- 20초 shared budget과 1,000개 scan bound를 넘은 claim은 그 startup에서 해결되지 않을 수 있다.
- metadata는 payload bytes의 존재와 크기를 확인하지만 content checksum을 다시 계산하지 않는다.

### Risks

- signed Manifest를 “파일이 있으니 성공”으로 오해할 수 있다. signature, identity, binding,
  required-replica equality, payload metadata를 모두 검사한다.
- Attention claim을 운영자가 수동 complete/requeue하면 fencing과 artifact truth가 깨질 수 있다.
  incident runbook은 immutable evidence 보존과 reader/trust 조사만 허용한다.

## Related

- [ADR-002: Authoritative Data Ownership](ADR-002-authoritative-data.md)
- [ADR-003: At-least-once Attempts with Fenced Commit](ADR-003-fenced-delivery.md)
- [ADR-004: Recovery Point and Artifact Replica Semantics](ADR-004-recovery-point-and-replicas.md)
- [Recovery Point, Artifact, and Storage Lifecycle](../sections/section-05-recovery-point-and-storage.md)
- [Program-owned backup engine](../../program-owned-backup-engine.md)
- [Production incident runbook](../../../runbooks/incident.md)

## 2026-08-24 runtime amendment

Reconciliation is not limited to process startup. The service performs the readiness-gating pass
and then repeats a bounded pass every five seconds. Every pass creates a fresh recovery source and
therefore a fresh shared 20-second verification budget; the budget is shared by at most 1,000
claims in that pass and is never restarted per claim.

Before consuming PostgreSQL, MySQL, or MariaDB dump output, the worker records the child PID, OS
creation time, service boot ID, absolute executable path, and SHA-256. A later pass re-reads the
live process path and hash. A live child without recoverable Job Object ownership remains
`Attention`; only confirmed exit, absence, or PID reuse permits automatic requeue. This preserves
at-most-one active native dump when a service process crashes.

The six-hour absolute pre-commit deadline includes Manifest-last publication. The worker checks the
deadline before `begin_backup_claim_commit` and bounds the complete publish future by the remaining
time. Before that boundary, timeout is retryable and prepared spool is cleaned. After the boundary,
timeout or publish failure has an unknown external outcome: the worker returns
`AwaitingReconciliation`, keeps claim/attempt and intent in `committing`, and preserves spool and
reservation. It never calls failed completion or retry for that outcome.
