# External Review Resolution

> Provider: Codex CLI
> Verdict: CONDITIONAL GO
> Gemini: 설치되지 않아 미실행

## Critical Findings Resolved

| Finding | Resolution |
|---------|------------|
| Service cutover before durable runtime | Phase 2를 non-executing service shell로 변경하고 actual cutover를 Phase 3 G1 뒤로 이동 |
| Local IPC authorization under-specified | ADR-007, FR-SVC, SID impersonation, command class, replay/size/fuzz test 추가 |
| Secret custody unresolved | ADR-006, service-owned DPAPI/ACL, Secret Ref, canary test 추가 |
| Recovery Point vs required destination conflict | completeness/durability/compliance 3축과 degraded Snapshot 규칙 확정 |
| Restore-as-exec not enforceable | 엔진별 Restore Safety Policy와 dangerous fixture gate 추가 |
| Post-cutover rollback incomplete | cutover point별 rollback/forward-fix matrix와 runbook rehearsal 추가 |

## Important Findings Resolved

- SQLite WAL, busy timeout, writer/read pool, transaction budget, checkpoint, online backup 규칙 추가
- Activity burst와 heartbeat 우선순위·bounded batch test 추가
- scheduled/manual/retry/catch-up occurrence identity 분리
- Process Supervisor에 no-shell, restricted env, pipe drain, breakaway, SCM stop 규칙 추가
- retention을 dry-run/tombstone/delete의 독립 destructive subphase로 승격
- engine fixture matrix와 content oracle 구체화
- benchmark hardware/data/measurement profile 추가
- `team-review.md`의 오래된 phase 번호를 최종 plan과 동기화
- notification FR, outbox/delivery/retry/redaction/test 추가
- Operational Store live backup을 SQLite online backup 또는 quiesce로 명시
- v2 계약을 internal Rust DTO와 local IPC로 제한하고 public Agent/API protocol은 G5로 연기

## Remaining Status

외부 리뷰의 minimum corrections는 사양, 계획, ADR에 반영됐다. 계획 판정은 `GO for design
artifacts`, 구현 판정은 Phase 0의 truth gate를 통과하기 전까지 `CONDITIONAL GO`다.

## Post-review Schema Reconciliation

외부 계획 리뷰 뒤 `ipc-spec.md`, flow와 Operational Store DDL을 교차 검토해 다음 누락을
추가로 보정했다.

| 발견 | 보정 |
|---|---|
| mutating IPC replay와 destructive token이 선언만 있고 durable row가 없음 | `ipc_request_ledger`, digest-only `confirmation_tokens`, terminal immutability trigger |
| backup/restore 외 장기 command가 Work Item에 연결될 수 없음 | generic `OperationRun -> WorkItem` subject 추가 |
| Plan Occurrence가 단일 completed 상태로 부분 성공을 숨김 | `succeeded`, `partially_succeeded`, `failed` 분리 |
| Snapshot projection에 Protection Compliance 축이 없음 | completeness/durability/compliance/eligibility 4축 분리 |
| notification outbox reclaim이 crash-safe하지 않음 | delivery lease, owner, boot ID, fencing token과 sweep index 추가 |

최종 DDL은 53개 table이며 bundled `rusqlite` in-memory 실행, table count, integrity/FK,
required Destination trigger, confirmation immutability, generic Work Item, overwrite/L3,
Recovery Point와 Snapshot 상태 smoke test를 통과했다.
