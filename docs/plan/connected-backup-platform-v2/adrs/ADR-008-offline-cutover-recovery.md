# ADR-008: Offline singleton cutover recovery

- 상태: 승인
- 날짜: 2026-07-29
- 관련 결정: ADR-001, ADR-005, ADR-007

## Context

clean cutover를 되돌릴 때 Operational Store의 execution authority와 Windows Task Scheduler의
enablement를 함께 복원해야 한다. SQLite transaction과 Task Scheduler API는 하나의 분산
transaction으로 묶을 수 없다. 실행 중인 service IPC에서 rollback을 제공하면 DB commit과 Task
복원 사이의 crash 또는 부분 실패가 service와 legacy task의 동시 실행으로 이어질 수 있다.

## Options

| 선택지 | 장점 | 치명적 한계 |
|---|---|---|
| service IPC에서 online rollback | UI에서 편리함 | SQLite와 Task Scheduler를 원자 commit할 수 없어 dual-run 가능 |
| service와 동일한 singleton을 점유하는 offline recovery | service claims와 scheduler mutation을 시간적으로 분리, crash 후 재개 가능 | service를 먼저 정지해야 하며 owner의 명시적 명령 필요 |
| 수동 SQL 삭제 후 `schtasks` 실행 | 구현이 단순해 보임 | audit, bundle binding, 순서, 재시도, forward-fix 경계를 우회 |

## Decision

offline singleton recovery를 선택한다.

1. 서비스와 같은 global singleton을 획득하지 못하면 복구를 시작하지 않는다.
2. protected `service-identity.json`의 설치 owner와 일치하는 elevated local token만 허용한다.
3. operator는 cutover marker 전체, rollback UUID, 최초 timestamp를 명시하고 marker를 한 번 더
   입력한다.
4. protected evidence root 안의 recovery bundle과 모든 item digest, scheduler inventory, 현재
   disabled 상태를 다시 검증한다.
5. production epoch가 `rollback_eligible`일 때만 SQLite transaction으로 authority를 legacy로
   전환하고 audit를 남긴다. 첫 v2 write 이후에는 `CONFIG_CUTOVER_FORWARD_FIX_REQUIRED`다.
6. DB commit이 끝난 뒤에만 이전 enablement를 복원한다. 복원 중 실패하면 service는 정지되어 있고
   persisted authority는 legacy이므로 claims가 살아나지 않는다.
7. 같은 rollback UUID, marker, bundle, owner SID, reason, timestamp를 사용한 재실행만 기존 audit를
   replay하고 남은 Task를 idempotent하게 복원한다.

## Consequences

- service authority와 legacy Task가 동시에 production execution authority가 되는 경로가 없다.
- DB rollback 직후 crash하면 backup이 잠시 중단될 수 있지만 중복 실행보다 안전하며 같은 명령으로
  재개할 수 있다.
- approved administrator의 online destructive command 권한과 별개로 offline authority rollback은
  설치 owner에게만 제한한다. identity file의 기존 최소 DACL을 넓히지 않는다.
- Task definition은 live XML의 task-level `Settings/Enabled`만 비교에서 제외한다. Trigger별
  `Enabled`를 포함한 나머지 definition은 bundle 원본과 다시 대조한다. definition drift가 있으면
  enable하지 않으며, definition 재등록이 필요한 손상은 installer/incident forward-repair 범위다.
