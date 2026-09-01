# Implementation Skill Routing

이 문서는 구현을 시작할 때 어떤 검증 규율을 어느 단계에 적용할지 정한다. 스킬을 많이
호출하는 것이 목적이 아니라, 위험에 맞는 최소 절차를 빠뜨리지 않는 것이 목적이다.

| 작업 | 적용 스킬 | 적용 시점 | 기대 산출물 |
|---|---|---|---|
| 장기 구현 루프 | `auto-continue-loop` | section 단위 구현 시작 | 중단 없는 구현·검증 루프 |
| 상태 머신·queue·migration | `test-driven-development` | section 02–05 | 실패 테스트부터 시작하는 변경 |
| 재현 어려운 crash/fencing 오류 | `systematic-debugging` | fault injection 실패 시 | 재현, 가설, 원인, 회귀 테스트 |
| SQLite 스키마 변경 | `database-schema-designer` | section 02 이후 모든 migration | constraint/index/migration 검토 |
| React/Tauri 제품 UI | `frontend-design`, `react-dev`, `vercel-react-best-practices` | section 07–08 | DESIGN.md 준수 화면과 성능 기준 |
| UI 정적·렌더링 감사 | `ui-ux-auditor`, `web-design-guidelines` | 각 UI 화면 완료 후 | 접근성·반응형·시각 회귀 보고서 |
| 흐름 상태 전이 검증 | `flow-verifier` | section 02–08 완료 시 | Mermaid node-to-code 대응표 |
| QA 자동화 | `minos`, `api-tester` | IPC/화면 통합 이후 | Playwright와 IPC 통합 회귀 |
| 준공 감사 | `argos`, `verify-implementation` | release candidate | spec/section/QA 대비 누락 보고서 |
| 배포·단계적 출시 | `shipping-and-launch` | section 10 | rollout, rollback, monitoring gate |
| ADR·운영 문서 갱신 | `documentation-and-adrs` | 결정 변경 시 | superseded 이력이 보존된 ADR |

## 적용 원칙

1. section 파일을 구현의 단일 작업 입력으로 사용한다.
2. 라이브러리 API와 버전은 구현 직전 공식 문서로 다시 확인한다.
3. 테스트 실패가 원인 미상인 경우 무작정 수정하지 않고 `systematic-debugging`으로 전환한다.
4. UI는 root `DESIGN.md`를 정본으로 사용하며, Databasement 화면을 픽셀 복제하지 않는다.
5. 지원 엔진 승격은 코드 존재가 아니라 실제 restore fixture와 Engine Matrix 증거로 판단한다.
6. 모든 section gate 통과 후 `argos`와 `verify-implementation`을 실행하기 전에는 출시 완료로
   표시하지 않는다.

## 이번 설계에 적용된 스킬

- `zephermine`: 리서치부터 section, operation, QA까지 산출물 구조와 검토 순서를 고정
- `domain-dictionary`: Connection/Target, Recovery Point/Snapshot, Replica/Compliance 용어 확정
- `database-schema-designer`: SQLite Operational Store 정규화와 제약조건 검토
- `flow-verifier`, `mermaid-diagrams`: 정상·오류·crash·cutover 흐름의 node 소유권 고정
- `codex`: 독립 외부 계획 리뷰와 conditional-go 판정
