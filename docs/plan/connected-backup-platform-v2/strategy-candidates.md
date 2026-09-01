# Strategy Candidates

> 평가일: 2026-07-26
> 점수: 1(불리) ~ 5(유리), 가중 합계는 100점 환산

## Candidate A: Current Tauri Monolith Expansion

현재 config, CLI command, Tauri command에 화면과 엔진을 직접 추가한다. 가장 짧게 보이지만
`Job` 과적재, 개별 Task Scheduler process, 휘발성 UI 로그, 전역 concurrency 부재를 그대로
확장한다.

장점:

- 초기 코드 이동이 적다.
- 기존 팀이 구조를 바로 이해할 수 있다.

단점:

- SQL Server·SQLite를 넣는 순간 조건 분기가 빠르게 증가한다.
- UI, schedule, execution의 실행 경로가 계속 갈라진다.
- 여러 DB의 부하, 취소, 재시작 복구를 신뢰하기 어렵다.
- Databasement 수준의 agent/API 확장 seam을 만들지 못한다.

## Candidate B: Databasement Fork + Rust Executor

Databasement의 Laravel control plane을 포크하고 현재 Rust 코어를 agent 또는 external executor로
연결한다.

장점:

- 정보구조, 다중 엔진 화면, 팀/API 기능을 빠르게 확보할 가능성이 있다.
- Databasement 생태계 업데이트를 일부 따라갈 수 있다.

단점:

- PHP/Laravel과 Rust 두 제품의 배포·upgrade·domain model을 동시에 유지한다.
- 기존 Windows desktop-first 목표가 self-hosted server 운영으로 바뀐다.
- Databasement 내부 서비스 계약과 Rust 보안 pipeline 사이 adapter가 새 핵심 복잡성이 된다.
- 포크 divergence, upstream migration, 보안 책임 경계가 장기 비용이 된다.

## Candidate C: Desktop-first Modular Strangler

Tauri UI와 기존 Rust 코어를 유지하되 application service, operational SQLite, persistent queue,
dispatcher, Engine Adapter를 명확히 분리한다. 기존 명령을 새 use case 경로로 하나씩
전환하고 Databasement의 정보구조와 capability 모델을 재구현한다. 향후 Agent와 HTTP API는
동일 계약에 추가한다.

장점:

- 기존 보안 자산과 Windows 제품성을 모두 보존한다.
- 새로운 기능이 옛 실행 경로를 우회하지 않도록 단계적으로 수렴시킬 수 있다.
- DB 엔진, Tauri, CLI, Scheduler, 향후 Agent의 결합도를 낮춘다.
- 사용자에게 Connections·Plans·Snapshots 경험을 먼저 제공할 수 있다.

단점:

- 전환 기간에 기존 TOML/CLI와 새 SQLite 모델을 함께 다뤄야 한다.
- schema migration, dual-run 차단, compatibility adapter가 필요하다.
- 초기에 도메인 경계를 잡는 시간이 든다.

## Weighted Decision Matrix

| 기준 | 가중치 | A | B | C |
|------|--------|---|---|---|
| 사용자 원래 목적 적합성 | 20 | 3 | 4 | 5 |
| 기존 Rust 자산 보존 | 15 | 5 | 2 | 5 |
| 다중 DB 실행 안정성 | 15 | 2 | 3 | 5 |
| 초기 가치 전달 속도 | 10 | 4 | 3 | 4 |
| 장기 엔진 확장성 | 15 | 2 | 4 | 5 |
| agent/API 확장 경계 | 10 | 1 | 5 | 4 |
| 운영·배포 단순성 | 10 | 4 | 2 | 4 |
| 마이그레이션 위험 | 5 | 4 | 1 | 3 |
| **가중 점수 / 100** | **100** | **60** | **63** | **92** |

계산은 각 점수/5에 가중치를 곱했다.

## Decision

Candidate C를 채택한다.

이 결정은 “Databasement를 적게 가져온다”는 뜻이 아니다. Databasement의 사용자 흐름,
도메인 객체, capability 표현, 스냅샷·복원 운영 경험은 제품 요구사항으로 적극 흡수한다.
가져오지 않는 것은 PHP/Laravel 구현과 self-hosted control plane을 1차 런타임으로 삼는
결정이다.

## Guardrails

- 새 UI는 기존 config command를 직접 호출하지 않고 application service를 거친다.
- 기존 CLI와 Scheduler도 순차적으로 같은 Work Item 경로로 전환한다.
- TOML→SQLite migration은 reversible import와 자동 backup을 제공한다.
- dispatcher 전환 중 old scheduler와 new queue가 같은 occurrence를 실행하지 못하게 한다.
- 신규 엔진은 common Support Contract 밖의 ad-hoc 명령으로 출시하지 않는다.
