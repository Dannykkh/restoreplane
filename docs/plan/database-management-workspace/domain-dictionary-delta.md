# Domain Dictionary Delta — database-management-workspace

Master: docs/domain-dictionary.md; extension: docs/domain-dictionary-database-management.md.
기존 v4 보안/백업 용어 보존. 이번 feature v1→v2→v3는 별도 진행 이력이다.

## v1 draft
ADD Workbench, Session, Credential Reference, Generation, Query Run, Result Set/Batch, Typed Cell, Cursor, Draft, Verified Apply, Journal, Unknown, Conflict, Consistency View, Credit, Outbox/Inbox.
Workspace(소유권 경계)와 Workbench(신규 관리 과업)를 분리한다.

## Global Dictionary Sync
inferred-skip: matching global seed 없음. 프로젝트 의미에 결합하므로 프로젝트 범위만 갱신한다. 글로벌 폴더·새 기억을 만들지 않는다.

## v1 → v2
- Phase A 4개 리뷰 ADD/REFINE으로 Workbench Access Lease/Transaction, Execution Epoch, Statement Outcome, Operator Acknowledgment, Journal Epoch, Target Context Label, Result Completeness, Selection Anchor, Transaction Disposition 추가 (28개 core terms).
- CONFLICT Lease/Transaction Owner는 기존 정의를 수정하지 않고 Workbench 접두사로 분리, accepted-by-default.
- UX Add/Clarify는 ADD/REFINE으로 정규화. MERGE none은 변경 없음.

## v2 → v3
- Codex domain-process / Antigravity domain-technical independent processes completed. ADD Workbench Capability Matrix and Deduplication Retention, total30 core terms.
- 중복 제안은 기존 terms REFINE으로 통합. Lease/Transaction Owner 충돌은 기존 service 의미 보존+Workbench qualification으로 해결.
- JournalEpoch(장부 identity/복구)와 process BootEpoch(매 launch 실행권)를 구분한다. BootEpoch는 Journal Epoch의 구현 요소로 설명하며 별도 core term을 늘리지 않는다.
- accepted-by-default: commit uncertainty, execution cancellation barrier, per-statement outcome, truthful trigger/partial guarantees, pre-persistence encryption, unknown operator acknowledgment.
- unresolved-conflict: none. Global sync skipped as project-specific; no user approval was inferred.
