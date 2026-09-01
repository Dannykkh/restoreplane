# ADR-003: At-least-once Attempts with Fenced Commit

- 상태: 승인됨
- 날짜: 2026-07-26

## Context

백업은 SQLite 밖의 DB process와 원격 저장소를 사용하므로 exactly-once 실행을 보장할 수
없다. Lease 만료 뒤 stale worker가 늦게 commit하면 중복 Recovery Point가 생긴다.

## Options

| 옵션 | 장점 | 단점 |
|------|------|------|
| exactly-once 표방 | 단순한 사용자 설명 | 외부 process/storage 때문에 실현 불가 |
| at-most-once | 중복 시도 억제 | crash 시 실행 누락·RPO 위반 |
| at-least-once + fencing | 재시작 복구와 유실 방지 | idempotency와 Reconciliation 필요 |

## Decision

- Plan Occurrence는 `plan_id + plan_revision + scheduled_for_utc`로 유일하다.
- Backup Run은 occurrence 내 Database Target당 하나다.
- Work Item은 여러 Attempt를 가질 수 있다.
- claim마다 Fencing Token을 발급한다.
- heartbeat, 상태 전이, final commit은 current token CAS를 통과해야 한다.
- destination publish는 occurrence/run identity 기반 멱등 key를 사용한다.
- crash가 `committing` 중 발생하면 재실행 전에 Reconciliation한다.

## Consequences

중복 Attempt는 가능하지만 occurrence당 complete Recovery Point는 하나만 인정된다. 테스트는
stale token, crash point, destination partial publish를 반드시 주입한다.
