# Blindspot Map

## Known knowns
Tauri/Rust 유지, 백업 보존, DBDelve 기능 참고. 일반 SQL/typed grid는 신규이며 기존 console_query는 운영 카탈로그 조회다. service secret은 machine DPAPI; 현재 UI는 로드 목록을 누적한다.

## Known unknowns
Q1 첫 완성 범위 선호 pending(optional). 실제 기기/서버/데이터 규모·기한은 미확인. 특정 인증·확장 타입은 driver certification 대상이다.

## Unknown knowns
기존 Workspace 소유권과 백업 증거 계약, SQL 원문 실행 기대, 정확한 NULL/숫자, 저장하지 않은 draft 손실 안내가 중요하다. raw query/result 자동 캐시는 비밀을 보이지 않게 저장할 수 있다.

## Unknown unknowns
commit 뒤 로컬 디스크 오류; triggers/외부 효과/ABA; nullable unique key/collation/precision; no ACK/거대 셀/취소 경합; 긴 snapshot과 WAL; MySQL implicit commits.

## Architecture-changing questions
| 후보 | disposition |
|---|---|
| service 신원으로 사용자 SQL 실행? | 보수적 기본: user-owned engine+별도 credentials. 기존 service registry 대안과 strategy에서 비교 |
| 연결 재사용=암호 재사용? | metadata/identity만; secret 자동 export 금지 |
| 모든 페이지 한 snapshot? | 기본 live-per-page; 검증된 explicit snapshot만 별도 |
| 임의 SQL 자동 retry? | 금지; admission dedup와 target execution 분리 |
| cross-DB atomicity? | 명시 사례 없음; per-target outcomes, XA/2PC 제외 |
| 첫 기능 범위? | Q1 pending; source of truth 가정으로 유지 |
| agent/MCP? | 요구 없음·upstream draft; 후순위 |
Critical private blocker: none for planning. Actual credentials/production writes are unnecessary.
