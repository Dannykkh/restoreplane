# Database Workbench Design Contract

정본: ../../../DESIGN.md 및 현재 Console tokens.css. 기능 확장 설계이며 렌더 검증은 구현 단계다.
Module loaded: frontend-design exact catalog path + ai-slop-blacklist + coder-interface-pattern-playbook.

## Vision and Experience Contract
Data Instrument. 첫 과업: 현재 Workspace에서 Target identity를 확인하고 schema/table을 찾아 읽는다.
대상·mode·transaction·consistency·freshness·result cap을 작업 영역에서 항상 식별할 수 있어야 한다.
기존 teal/slate, Pretendard/Space Grotesk/JetBrains Mono, light/dark, reduced-motion 유지. 토큰 본문을 복사해 별도 정본으로 만들지 않는다.

## Spatial model
기존 App navigation에 Database Workbench 추가. 좌측 lazy explorer, 중앙 SQL tabs/editor/results, 우측 선택적 row/protection inspector.
상단: Workspace/connection/database 실제 identity + connection mode + transaction state.
하단: 실행 상태, 받아온 행/bytes, capped/complete, isolation/time, cancel.
좁은 창: explorer/inspector를 접고 결과·row detail에 순차 접근. 대규모 표를 전체 화면 폭에 억지 압축하지 않는다.

## Components and states
- SchemaExplorer: lazy paged tree, bounded cache, keyboard expansion/search, loading/partial/stale/error.
- QueryTabs: target binding, dirty draft, explicit run/stop; close while transaction pending confirms disposition.
- VirtualResultGrid: row+column virtualization, pinned keys, selection independent from viewport, NULL/empty/unsupported exact labels.
- RowInspector: bounded long values, binary/JSON typed view; truncated cell never editable by display text.
- EditReview: original/current/draft, source target, affected count semantics; conflict/unknown separate from failure.
- OperationPanel: versioned status, dedup/gap recovery; unknown requires explicit inspection.
- ProtectionPanel: existing evidence/actions, backup-before-change not a global atomic transaction.

## Accessibility and interaction
ARIA grid coordinates reflect logical rows/columns, roving focus survives virtualization/eviction, keyboard paging and screenreader alternative row view.
No query/results/credentials in URL/localStorage. No hover-only core actions. Focus restored after dialogs and reconnect.
Theme contrast, Korean glyph loading, keyboard, responsive behavior and reduced-motion are actual-render gates in section-01/04.

## Anti-patterns
No card-wall dashboard, decorative animation, broad backdrop filters, generic hero, misleading green success for capped/unknown/partial results.
No benchmark assertions before Windows/WebView measurement. Motion restricted to meaningful short feedback; no live-count animation.
