# Discovery Interview Record

> 작성일: 2026-07-26
> 방식: 이번 대화와 과거 Claude 대화에서 이미 확인된 의도를 사용했다. 추가 질문은 제품
> 방향을 바꾸지 않는 세부 선택뿐이므로, 작업을 멈추지 않고 보수적 기본값을 채택했다.

## Questions Asked

- 실시간 추가 질문: 0개
- 근거가 된 사용자 진술:
  - 원래 목적은 HeidiSQL·DBeaver처럼 DB에 연결해 주기적으로 백업하는 도구였다.
  - Databasement의 UI와 지원 DB를 현재 제품에 폭넓게 흡수할 필요가 있다.
  - 관리 DB가 여러 개일 때 Rust가 백업 부하를 견딜 수 있어야 한다.
  - 크로노스 방식으로 끝까지 구현하고 검증하는 수준을 원한다.

## Problem Gate

통과. 사용자가 해결하려는 문제는 보안 설정 자체가 아니라 여러 DB 연결의 반복 백업과
복원 운영이다. 현재 제품은 실행 코어는 강하지만 연결·탐색·계획·스냅샷·활동 관리 흐름이
얕아 원래 목적이 화면에서 드러나지 않는다.

## User and Outcome Gate

통과. 1차 사용자는 Windows에서 여러 DB를 직접 관리하는 운영자다. 성공은 사용자가
연결을 등록한 뒤 DB를 선택하고, 일정과 목적지를 지정하고, 실패 여부와 복원 가능성을
한 화면에서 판단하며, 특정 스냅샷을 안전하게 복원할 수 있는 상태다.

## Scope Gate

통과. 이번 v2의 중심은 데스크톱 제품 완성과 중앙 dispatcher다. 원격 agent, 웹 제어면,
조직·RBAC·SSO·MCP는 확장 경계는 설계하되 첫 완성 범위에서는 제외한다.

## Inferred Decisions

| 결정 | 기본값 | 근거 |
|------|--------|------|
| 1차 제어면 | Windows Tauri desktop | 기존 자산과 사용자의 원래 목표 |
| 실행 구조 | persistent queue + 중앙 dispatcher | 다중 DB 부하와 재시작 복구 |
| 운영 저장소 | SQLite 진실 소스, TOML export/import | 관계형 도메인과 안전한 마이그레이션 |
| Databasement 활용 | 코드 포크가 아닌 요구사항·IA 기준 | Rust 코어 보존과 유지비 |
| 첫 엔진 범위 | 기존 3종 완성 후 SQL Server·SQLite | 회귀 위험과 사용자 가치 균형 |
| agent/API/RBAC | 후속 단계 | 로컬 운영 흐름이 먼저 |

## Remaining Validation Questions

구현 착수 전 사용자에게 다시 물어야 할 blocker는 없다. 아래 항목은 구현 중 텔레메트리와
사용 데이터로 조정한다.

- 실제 관리 DB 수와 평균 dump 크기
- 원하는 RPO/RTO와 야간 백업 창
- SQL Server BACPAC 후보와 native `.bak` 중 실제 우선순위
- 추가 저장 목적지의 실사용 순서
