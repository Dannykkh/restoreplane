# Blindspot Map

> [!IMPORTANT]
> 이 파일은 리서치 당시의 blindspot 기록이다. Task Scheduler wake 모델과 초기 UI 상태 설명은
> [ADR-001](../../adr/ADR-001-execution-authority.md) 및
> [Program-owned backup engine](../program-owned-backup-engine.md)의 현재 계약으로 대체됐다.

> 이 문서는 리서치 초기에 만든 질문 목록이다. 인터뷰·팀 리뷰·외부 리뷰 뒤 확정된 항목은
> 아래 `Resolved Decisions`에 반영했고, 현재 미확정인 경험적 변수만 Known Unknowns에 남겼다.

## Known Knowns

- 최초 목표는 HeidiSQL·DBeaver처럼 여러 DB 서버에 연결해 데이터베이스를 선택하고 주기적으로
  백업·복원하는 관리 도구다.
- 랜섬웨어 내성은 제품의 유일한 목적이 아니라 연결형 백업 관리 흐름에 내장할 필수 안전 속성이다.
- Databasement의 서버·DB·볼륨·백업·스냅샷·복원·로그·에이전트 정보구조와 엔진 지원 범위를
  제품 패리티 기준선으로 삼는다.
- 현재 Rust 코어에는 PostgreSQL, MySQL, MariaDB source, Local/S3/SFTP destination,
  봉투암호화, 서명 매니페스트, 불변성, 원격 reader, 카탈로그 rebuild의 재사용 가능한 구현이 있다.
- 현재 Tauri UI는 셋업·등록·잡·대시보드·복원·스케줄의 초기 흐름만 제공하며 연결 테스트,
  DB 탐색, 스냅샷 inventory, 구조화 활동 로그가 부족하다.
- 잡별 Task Scheduler 프로세스 구조에는 전역 동시성 제한이 없고 `max_parallel_dumps`는 실제
  실행 제한에 연결되지 않았다.
- “DB 지원”은 연결 테스트부터 실제 restore E2E까지 인증된 조합만 의미한다.

## Known Unknowns

- 실제 운영 DB 수·크기·백업 창·RPO/RTO가 없어 concurrency 기본값은 보수적 추정에 의존한다.
- SQL Server/SQLite 이후 MongoDB, Firebird, 추가 Destination의 실제 사용자 우선순위는 beta
  수요로 재정렬해야 한다.
- native tool 재배포 라이선스, Windows code signing, updater 운영 비용은 release 단계에서
  법무·운영 확인이 필요하다.
- performance/SLO 수치는 small/medium/large fixture benchmark와 dogfood 결과로 확정해야 한다.

## Resolved Decisions

| 질문 | 확정 |
|---|---|
| desktop인가 web인가 | 초기 v2는 Windows desktop-first+독립 local service; HTTP/RBAC는 후속 |
| dispatcher 위치 | Windows service singleton |
| Operational Store | SQLite write model이 권위, TOML은 export/import |
| cutover | shadow import→G1→single authority one-way cutover; 새 data 뒤 forward-fix |
| Databasement 활용 | fork/dependency가 아닌 제품 parity/UX 기준선 |
| 첫 엔진 확장 | 기존 3종 복원 계약 뒤 SQL Server BACPAC 후보+SQLite online backup |
| Agent | 초기 v2 제외; identity/offline queue/split-brain을 다루는 별도 ADR/protocol 전에는 구현 안 함 |
| 조직 기능 | public HTTP/RBAC/SSO/API/MCP는 local 제품 안정화 뒤 별도 범위 |

## Unknown Knowns

- 사용자는 기능 일부 참고보다 Databasement가 구현한 제품 전체를 현재 프로젝트에 흡수하는 것을
  기대한다.
- 사용자가 느끼는 완성도는 암호화 알고리즘의 깊이보다 “연결해 보이고, 예약하고, 이력을 찾고,
  선택해 복원되는가”에서 먼저 결정된다.
- 기존 Rust 투자와 Windows·데스크톱 원형을 폐기하는 빅뱅 포크보다, 기능 패리티를 단계적으로
  흡수하면서 실행 엔진을 보존하는 경로가 되돌리기 쉽다.
- 지원 엔진 수가 늘어날수록 enum 추가보다 공통 capability와 인증 매트릭스가 제품 신뢰를 좌우한다.

## Unknown Unknowns

- DB별 native tool 배포 라이선스, Windows 재배포 조건, 코드서명과 자동 업데이트 비용
- SQL Server `.dacpac`과 `.bak`, SQLite WAL, Redis·Valkey restore처럼 “백업” 의미가 엔진마다
  달라 UI가 잘못된 동일성을 만들 위험
- agent 탈취, lease 재전달, 동일 snapshot 중복 commit, 네트워크 분할 시 catalog truth 충돌
- queue migration 중 기존 Task Scheduler와 dispatcher가 동시에 실행되는 이중 백업 위험
- 장시간 `spawn_blocking` 압축의 취소 지연과 child process orphan
- 다수 스풀의 디스크 예약 경쟁과 목적지별 retry가 backlog를 악화시키는 상황
- UI가 과거 최고 검증 결과를 보여 최근 실패를 숨기는 false-green
- 광범위 엔진 지원을 서둘러 실제 restore 검증 없는 “명목 지원”이 되는 위험

## Architecture-Changing Questions

초기 질문 7개는 `Resolved Decisions`와 ADR-001~007에서 닫혔다. 이를 다시 열려면 기존 결정을
조용히 변경하지 말고 superseding ADR, migration 영향, rollback 경계를 함께 작성한다.

## Conservative Defaults

- [resolved] Windows desktop-first를 유지하되 application service와 dispatcher를 UI에서 분리한다.
- [resolved] Task Scheduler는 개별 job이 아니라 dispatcher 시작·깨우기만 담당한다.
- [resolved] SQLite를 로컬 운영 정본으로 전환하고, TOML은 버전 관리 가능한 export/import 형식으로
  제공한다.
- [resolved] Agent는 초기 v2에서 제외하고 별도 위협 모델 전에는 network protocol을 만들지 않는다.
- [resolved] 조직·RBAC·SSO는 HTTP control plane이 실제로 도입되는 단계까지 미룬다.
- [resolved] 첫 엔진 확장은 기존 3종 운영 완성 후 SQL Server BACPAC 후보와 SQLite online backup이다.
- [resolved] Databasement는 코드 dependency가 아니라 패리티 요구사항과 UX 기준선으로 사용한다.
