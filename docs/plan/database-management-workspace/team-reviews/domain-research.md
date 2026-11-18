# Domain Research — Database Operations Tooling

Complexity triage: MEDIUM — interactive DB changes and recovery workflows are safety-sensitive, but this is not a newly regulated industry workflow.
Research budget: no additional web search; research/web.md already covers the changing external facts. Input: spec, interview, research, docs/domain-dictionary.md and its feature v1 extension.

## Findings

- Critical D1: generic SQL cannot enlist the local operation journal and target DB in one transaction; the design must name the dual-write gap and preserve outcome_unknown.
- Critical D2: backup success is recovery evidence, not transaction isolation or undo for a later SQL mutation. An optional backup-before-change dependency does not close this gap.
- Important D3: data consistency is a capability with a scope: live page, explicit snapshot, single-target edit transaction, artifact consistency, semantic restore verification are different claims.
- Important D4: user profile metadata reuse and credential reuse are different operations. Machine-protected service credentials must not silently become interactive credentials.
- Important D5: SQL grid correctness includes exact value/type, key identity, collation/NULL ordering, original-row policy, and DB-specific matched/no-op counts.
- Important D6: stable keyset browsing and a fixed snapshot are independent guarantees. Whole-table export needs its own execution/time/consistency declaration.

## Domain recommendations

Adopt fail-closed capabilities, explicit partial/unknown states, readonly restored-target inspection, separate user/runtime and backup/service lifecycles. Do not add a distributed transaction coordinator or agent surface without a concrete need.
No private company fact blocks design. Engine/driver/live conformance remains an implementation gate; expert agreement is not execution evidence.

## Dictionary Updates

| Action | Term | Proposed definition | Evidence |
|---|---|---|---|
| REFINE | Consistency View | 페이지·트랜잭션·결과 집합 중 실제 보장 범위를 명시하며 백업 artifact 일관성과 구분 | spec F2/F7; research/web.md |
| REFINE | Outcome Unknown | 원격 실행 완료 여부가 미확정이며 관찰값만으로 과거 실행 횟수를 증명할 수 없는 상태 | spec consistency contract |
