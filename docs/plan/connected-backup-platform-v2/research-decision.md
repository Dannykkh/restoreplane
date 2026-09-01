# Research Decision

## Selected

- Codebase: yes — 기존 Rust 워크스페이스, Tauri 콘솔, 코드맵, 완료된 v1 계획과 테스트가 있으며
  신규 구축이 아니라 확장·재정렬 작업이다.
- Web: yes — Tokio 실행 모델, 데이터베이스별 네이티브 백업·복원 도구, 중앙 큐와 운영 안전성은
  현재 공식 문서를 기준으로 확인해야 한다.
- GitHub: yes — Databasement가 직접 기준 제품이며, 유사 오픈소스의 메뉴·provider·agent·job
  구조가 아키텍처 선택에 영향을 준다.
- Academic: no — 신규 암호 알고리즘이나 최적화 알고리즘 연구가 아니라 제품·통합·운영 설계다.
- Competitors: yes — 지원 엔진, 연결 방식, 저장소, UI 정보구조와 복원 흐름이 제품 범위를 결정한다.

## Research Topics

1. `self-hosted database backup manager architecture 2026`
2. `Tokio bounded concurrency external process scheduler 2026`
3. `multi-database backup restore native tools compatibility 2026`
4. `database backup manager UI snapshots jobs agents 2026`
5. `Windows database backup manager SQL Server PostgreSQL MySQL 2026`

## Assumptions

- [inferred] 제품의 기본 배포 형태는 기존 요구와 투자 자산을 고려해 Windows Tauri 데스크톱을
  유지하되, 중앙 dispatcher와 원격 agent 경계는 처음부터 분리한다.
- [inferred] Databasement의 제품 기능은 패리티 기준선이며, 현재 Rust 보안 엔진은 폐기하지 않는다.
- [inferred] DB 지원은 이름·설정 폼 추가가 아니라 연결 테스트부터 실제 복원 E2E까지를 의미한다.
- [inferred] 기존 `docs/plan/db-backup-system/`은 v1 보안 엔진 설계 이력으로 보존한다.

## Questions Deferred

- 데스크톱 단독 제품과 웹 제어면을 동시에 제공할 시점은 단계별 전략 비교 후 결정한다.
- SQL Server, SQLite, MongoDB, Firebird, Redis/Valkey의 출시 순서는 사용자 수요와 구현 위험을
  함께 채점한다.
- 조직·RBAC·SSO는 개인·소규모 운영자용 v2에 즉시 포함할지 후속 제어면 단계로 미룰지 결정한다.
