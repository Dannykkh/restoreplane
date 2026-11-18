# Interview and Inferred Decisions

Date: 2026-11-17. [Industry: database developer/operations tooling]
Question budget: 1/3 used. Silence is not approval.

## Given context
사용자는 DBDelve 참고 DB 관리 확장과 Tauri 유지, 페이징·인젝션·멱등성·분산 작업·중복 메시지·데이터 일관성을 요청했다. 최신 요청은 젭마인으로 설계를 구체화하는 것이다.

## Q1
첫 범위: 조회·SQL·편집+백업 연계 / 조회·SQL 우선 / 계정·권한·DDL 관리 포함.
Status: pending, optional. [inferred] 첫 선택으로 설계하며 답변이 오면 관련 산출물을 갱신한다.

## Soft Gate 1 — context
기존 Windows 서비스/Tauri 추가 기능. desktop_owner는 기존 Workspace 권한을 따른다. 새 SaaS/RBAC/플랫폼 포트는 아니다.

## Soft Gate 2 — problem and constraints
P1~P8은 spec 정본. 기한·인력·운영기기 미확인. 현재 Activity page 테스트가 대규모 interactive grid 성능 증거는 아니다.

## Soft Gate 3 — solution
[explicit] Tauri/React/Rust. [inferred] PostgreSQL/MySQL/MariaDB/SQLite 우선.
[inferred] Data Instrument형 dense explorer/editor/table, 기존 DESIGN.md를 유지한다. 추가 스타일 질문 없이 현재 방향을 구체화한다.
[inferred] MCP·신규 모든 엔진·권한관리·시각 DDL·XA/2PC는 제외. 편집 원자성은 검증된 단일 target transaction에 한정.
[inferred] named saved query만 명시적 encrypted 저장; automatic raw history/results persistence 없음.

## Authorization
프로젝트 내 설계 산출물 생성·검증은 요청 범위다. 실제 운영 DB 쓰기·배포·게시를 이 설계에서 수행하지 않는다.
