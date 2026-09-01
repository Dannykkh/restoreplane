# Connected Backup Platform v2 Planning Index

## Decision

Databasement의 연결 중심 다중 DB 제품 경험을 기준선으로 사용하고, 현재 Rust
암호화·Manifest·fan-out·검증·복원 코어를 Windows service 기반 durable execution engine으로
승격한다. 전략은 Databasement fork나 현재 코드 폐기가 아닌 `desktop-first modular strangler`다.

설계 산출물은 GO다. source 구현은 section-01 truth gate를 실제 실행해 통과하기 전까지
CONDITIONAL GO다.

## Start Here

| 목적 | 문서 |
|---|---|
| 전체 방향 5분 요약 | [direction.md](direction.md) |
| 최종 구현 순서와 gate | [plan.md](plan.md) |
| 기능·비기능 요구사항 | [spec.md](spec.md) |
| 구현 section 실행 순서 | [sections/index.md](sections/index.md) |
| 요구사항→section→QA 추적 | [traceability.md](traceability.md) |

## Research and Product

| 문서 | 내용 |
|---|---|
| [research.md](research.md) | Databasement, 경쟁 제품, 코드베이스 조사 통합 |
| [interview.md](interview.md) | 사용자 의도와 확인된 결정 |
| [strategy-candidates.md](strategy-candidates.md) | monolith/fork/strangler 후보 비교 |
| [personas-and-journeys.md](personas-and-journeys.md) | 운영·복구 사용자 journey |
| [design-system.md](design-system.md) | root DESIGN.md 기반 제품 UI 규칙 |

## Architecture and Contracts

| 문서 | 내용 |
|---|---|
| [team-review.md](team-review.md) | UX/아키텍처/적대적/도메인 리뷰 통합 |
| [db-schema.md](db-schema.md) | 53-table SQLite Operational Store 설계 |
| [schema-v2.sql](schema-v2.sql) | 실행 검증된 planning DDL |
| [ipc-spec.md](ipc-spec.md) | Windows local named-pipe command 계약 |
| [flow-diagrams/index.md](flow-diagrams/index.md) | 6개 정상·오류·crash·cutover 흐름 |
| [adrs/](adrs/) | execution, authority, fencing, artifact, cutover, secret, IPC, runtime root, crash commit 결정 |
| [ADR-008](adrs/ADR-008-offline-cutover-recovery.md) | owner 전용 offline cutover rollback과 forward-fix 경계 |
| [ADR-009](adrs/ADR-009-separate-install-and-data-roots.md) | non-nested InstallRoot/DataRoot와 repair·uninstall·reinstall 보존 경계 |
| [ADR-010](adrs/ADR-010-manifest-last-commit-recovery.md) | persisted commit intent와 signed Manifest-last startup 정산 경계 |

초기 v2는 public HTTP API가 없으므로 REST `api-spec.md`를 만들지 않았다. local IPC를 그대로
외부 protocol로 노출하지 않으며 Agent/HTTP/RBAC는 section-10 이후 별도 ADR 대상이다.

## Implementation Sections

| Order | Section |
|---:|---|
| 01 | [Truth and Release Gates](sections/section-01-truth-and-release-gates.md) |
| 02 | [Domain and Operational Store](sections/section-02-domain-and-operational-store.md) |
| 03 | [Service, IPC, and Secrets](sections/section-03-service-ipc-and-secrets.md) |
| 04 | [Durable Dispatcher](sections/section-04-durable-dispatcher.md) |
| 05 | [Recovery Point and Storage](sections/section-05-recovery-point-and-storage.md) |
| 06 | [Existing Engine Certification](sections/section-06-existing-engine-certification.md) |
| 07 | [Product UI Foundation](sections/section-07-product-ui-foundation.md) |
| 08 | [Snapshots and Recovery UI](sections/section-08-snapshots-and-recovery-ui.md) |
| 09 | [New Engine Wave 1](sections/section-09-new-engine-wave-one.md) |
| 10 | [Expansion and Launch](sections/section-10-expansion-and-launch.md) |

## Operations and Verification

| 문서 | 내용 |
|---|---|
| [operation-scenarios.md](operation-scenarios.md) | 9개 메뉴와 14개 E2E 운영 흐름 |
| [qa-scenarios.md](qa-scenarios.md) | 85개 제품 수준 QA 케이스 |
| [reviews/codex-review.md](reviews/codex-review.md) | 독립 외부 리뷰 원문 |
| [reviews/review-resolution.md](reviews/review-resolution.md) | correction과 conditional-go 해소표 |
| [verification.md](verification.md) | DDL·Mermaid·section·QA·문서 최종 검증 결과 |
| [skill-discovery.md](skill-discovery.md) | 구현·검증 단계의 스킬 routing |
| [rollback runbook](../../runbooks/rollback.md) | clean cutover rollback, 재시도, forward-fix 운영 절차 |

## Verified Planning Artifacts

- `schema-v2.sql`: bundled `rusqlite 0.31` in-memory 실행, 53 table, integrity/FK와 핵심
  trigger/constraint smoke test 통과
- `flow-diagrams/*.mmd`: Mermaid 11.16 CLI로 6개 모두 parse/render 통과
- `sections/index.md`: SECTION_MANIFEST 10개와 section file 10개 일치
- `qa-scenarios.md`: menu 49 + E2E 14 + IPC 10 + NFR 12 = 85

이는 planning artifact 검증이며 source 구현 완료를 뜻하지 않는다. 실제 구현 완료 판정은 각
section Quality Gate와 최종 Argos/verify-implementation evidence를 요구한다.
