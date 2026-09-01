# Program-owned backup engine rollback playbook

## First rule

실패했다고 legacy CLI/Task Scheduler를 먼저 켜지 않는다. service와 legacy 실행 경로가 동시에
동작하면 duplicate dump와 Recovery Point를 만들 수 있다. 먼저 새 claim을 닫고 현재 authority,
cutover epoch, schema minimum reader, native child, `committing` claim과 signed artifact evidence를
보존한다.

## Immediate containment

1. read-only `service status`와 `service diagnostics`를 보존한다.
2. service가 응답하면 `service drain`으로 새 claim을 닫는다.
3. update journal, control backup manifest, cutover marker, recovery bundle, affected IDs와 Activity를
   incident record에 연결한다.
4. native child가 살아 있거나 `committing` 상태라면 PID/path/hash, recovery intent, destination
   objects를 삭제하거나 수동 재큐하지 않는다. 5초 periodic reconciliation의 Attention 결과를
   확인한다.
5. signer/hash 불일치가 있으면 package와 native-tool tree를 격리하고 해당 provider를 비활성화한다.

## Disposition decision

| 관찰 상태 | 허용 disposition | 금지 |
|---|---|---|
| 기존 Store, cutover marker 전 | legacy authority를 그대로 유지하고 rehearsal 실패를 수정 | service claims 강제 활성화 |
| marker 뒤 첫 v2 control/queue/run write 전 `rollback_eligible` | 검증된 recovery bundle로 offline clean cutover rollback | marker 수동 삭제, 임의 Task enable |
| 첫 v2 write 또는 schema minimum reader 상승 뒤 | compatible signed build로 claims-stopped forward-fix | 구 binary 강제 실행, DB version 하향 |
| update의 schema migration 전 | journal disposition에 따른 previous signed binary 복구 | unsigned binary 복사 |
| update의 schema migration 이후 | control backup과 journal을 보존하고 forward-fix | Operational Store를 임의 backup 파일로 덮어쓰기 |
| `committing` + 완전한 required Manifest evidence | normal validator/periodic reconciler가 동일 Recovery Point ID로 정산 | 새 ID로 dump 재실행 |
| `committing` + 누락·불일치·timeout evidence | Attention 유지, reader/trust/binding 복구 | complete 추정, payload/marker 삭제 |
| `APPLICATION_OUTCOME_UNKNOWN` | inventory와 Activity로 domain outcome 확인 | 새 idempotency key로 즉시 반복 |

clean cutover rollback의 실제 명령과 멱등 replay 값은
[`runbooks/rollback.md`](runbooks/rollback.md)를 사용한다. update 실패는
[`runbooks/update.md`](runbooks/update.md)의 14단계 journal disposition을 사용한다.

## Data preservation

rollback/forward-fix 중 다음은 삭제하지 않는다.

- `DataRoot`의 Operational Store, secret, control backup, update journal, support bundle
- managed spool과 quarantine
- required Destination의 payload와 signed Set Manifest
- recovery bundle, freeze receipt, G0~G5 evidence
- previous signed package와 native-tool audit record

정상 uninstall도 `DataRoot`와 backup artifact를 보존한다. program binary만 유실되고 service entry가
남아 있으면 같은 roots와 승인된 package로 repair한다. 정상 uninstall 뒤 재등록은 repair가 아니라
같은 `DataRoot`를 지정한 install이다.

## Recovery verification

복귀 또는 forward-fix 뒤 다음을 모두 확인하기 전 claim을 다시 열지 않는다.

- execution authority가 정확히 하나다.
- stale lease/allocation과 ambiguous live child가 없다.
- duplicate Recovery Point와 stale fence commit이 0이다.
- Operational Store integrity/FK/schema/minimum-reader 검사가 통과한다.
- required replica와 signed Manifest binding이 일치한다.
- 영향 받은 engine/Scope의 create-new restore와 content oracle이 통과한다.
- scheduler cursor가 missed/duplicate occurrence 없이 이어진다.
- support bundle secret canary가 0이고 current revision의 release gate가 통과한다.

재개는 owner가 incident 원인, 수정 build, 남은 risk와 rollout stop metric을 검토한 뒤 명시적으로
승인한다.
