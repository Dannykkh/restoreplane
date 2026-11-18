# Database Management Context — feature v3 finalized

Date: 2026-11-17. Parent: [domain-dictionary.md](domain-dictionary.md). Existing v4 terms preserved.

| Term | Identifier | Definition |
|---|---|---|
| Database Workbench | workbench | 기존 Workspace 안의 대화형 DB 관리 과업. Workspace 소유권 경계와 구분 |
| Workbench Session | workbench_session | owner/window/workspace/target/revision/generation-bound live connection |
| Workbench Credential Reference | workbench_credential_ref | 사용자 interactive secret 참조; service backup secret과 별개 |
| Connection Generation | connection_generation | 재접속·무효화 때 바뀌는 세대; stale query/cancel/result 거부 |
| Query Run | query_run | 요청한 SQL 실행 1회의 identity/state/resource contract |
| Result Set | result_set | 한 Query Run의 한 결과; 재실행 결과와 합치지 않음 |
| Result Batch | result_batch | generation/sequence/bytes를 갖는 bounded typed rows |
| Typed Cell | typed_cell | NULL/정밀숫자/bytes/시간을 표시 문자열과 별개로 보존 |
| Browse Cursor | browse_cursor | target/schema/filter/order/typed last key에 결합한 opaque handle |
| Edit Draft | edit_draft | 전송 전 원본 키·값·변경값 묶음 |
| Verified Apply | verified_apply | 단일 transaction에서 identity/schema/원본 행을 확인한 generated edit |
| Operation Journal | operation_journal | durable admission/dispatch/outcome; target exactly-once 보장 아님 |
| Outcome Unknown | outcome_unknown | 전송/commit 뒤 결과를 증명하지 못함; rollback/실패와 구분 |
| Row Conflict | row_conflict | 현재 원본 값/버전/존재가 편집 기준과 불일치 |
| Consistency View | consistency_view | live-per-page/transaction snapshot 등 실제 조회 일관성 범위 |
| Credit Window | credit_window | 소비 ACK 전까지 허용하는 encoded bytes/rows |
| Outbox Event | outbox_event | local state transaction과 함께 기록한 재전달 event |
| Inbox Receipt | inbox_receipt | 소비 event ID와 projection version의 durable 중복 처리 기록 |

## Invariants
Read-only mode is not DB authority. Cursor is not a snapshot. Matching final data is not exactly-once proof.
Backup Run and Verified Apply do not form a global transaction.

## Changes
2026-11-17: feature v1 draft from spec/interview.

## Expert refinements (v2)
| Term | Identifier | Definition |
|---|---|---|
| Workbench Access Lease | workbench_access_lease | service가 읽기에 발급하는 principal/target/revision/epoch-bound 승인; writes는 fresh check |
| Workbench Transaction | workbench_transaction | 한 live connection의 transaction identity/isolation/outcome; reconnect 복원 불가 |
| Execution Epoch | execution_epoch | physical session 실행 세대와 cancel settlement barrier |
| Statement Outcome | statement_outcome | script 한 문장의 실행·효과 근거; 최종 오류와 별도로 선행 commit 보존 |
| Operator Acknowledgment | operator_acknowledgment | 불확실성을 사람이 인지함; commit/rollback 증거가 아님 |
| Journal Epoch | journal_epoch | 사용자 장부 식별/복구 세대; BootEpoch와 오래된 dispatch 권한 거부 |
| Target Context Label | target_context_label | Workspace/실제 endpoint/database/role/purpose 표시, authority token 아님 |
| Result Completeness | result_completeness | complete/capped/cancelled/interrupted/partial; consistency와 별개 |
| Selection Anchor | selection_anchor | result/row key/column identity 기반 viewport-independent focus |
| Transaction Disposition | transaction_disposition | keep-open/rollback-request와 실제 확인 상태; buffer discard와 구분 |

Refinements: Credit Window는 encoded accounting이며 decoded heap과 별개. Verified Apply는 인증된 transactional effects만 보증. Draft는 conflict 후 보존하되 invalidation 뒤 새 review 필요.
Conflict resolved: 기존 Lease/Transaction Owner는 service 의미를 보존하고 Workbench 접두사로 구분.

## Phase B final additions
| Term | Identifier | Definition |
|---|---|---|
| Workbench Capability Matrix | workbench_capability_matrix | 엔진/server/driver/settings별 typed keys/isolation/edit/cancel 인증·시각·증거; 기존 Backup Engine Matrix와 별개 |
| Deduplication Retention | deduplication_retention | attempt retry horizon/키 만료/tombstone/inbox·outbox 보존 계약; 기존 artifact Retention과 구분 |

2026-11-17: feature v3 finalized; 30 core terms, six expert inputs integrated. External claims are source-backed design constraints, not runtime certification. Changes after this point are explicit design revisions.
