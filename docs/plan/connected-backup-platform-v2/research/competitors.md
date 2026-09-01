# Competitor Research

> 조사일: 2026-07-26
> 원칙: 경쟁사의 공식 사이트·공식 문서에서 확인되는 내용만 사용한다.

## Product Matrix

| 제품 | 제어면 | 확인된 DB 범위 | 저장·운영 강점 | 우리에게 주는 기준 |
|------|--------|----------------|----------------|--------------------|
| Databasement | 자체 호스팅 웹, agent | PostgreSQL, MySQL/MariaDB, SQL Server, MongoDB, SQLite, Firebird, Redis/Valkey 등 | 볼륨, 스냅샷, 복원, agent, API/MCP, 팀 | 제품 구조와 지원 폭의 직접 기준선 |
| SQLBackupAndFTP | Windows 데스크톱 | SQL Server, MySQL, PostgreSQL | 로컬/NAS/FTP/SFTP/다수 클라우드, 예약, 알림, 스크립트, 암호화 | Windows 운영자의 짧은 설정 동선 |
| Databasus | 자체 호스팅 웹 | PostgreSQL, MySQL/MariaDB, MongoDB | PITR, 복원 검증, S3/Drive/FTP, 알림 | 검증 상태를 백업 성공과 분리 |
| Portabase | 자체 호스팅 웹, agent | PostgreSQL, MySQL, MongoDB, SQLite 등 | 자체 저장소, 예약, 보존, 복원 | agent 기반 원격망 확장 |
| GNIZA | Web/TUI, SSH pull | MySQL, PostgreSQL | agentless SSH, rclone 기반 70여 목적지, 로그·스냅샷 | 목적지 폭은 어댑터 생태계로 확장 가능 |
| pgBackWeb/UFO Backup | 웹 | PostgreSQL 중심 | 단순한 연결·예약·복원 UX | 단일 엔진 수준의 흐름 완성도 |

## Official Sources

- Databasement: https://github.com/David-Crty/databasement
- SQLBackupAndFTP: https://sqlbackupandftp.com/ 및 https://sqlbackupandftp.com/docs/
- Databasus: https://github.com/RostislavDugin/postgresus
- Portabase: https://portabase.io/
- GNIZA: https://gniza.app/
- pgBackWeb/UFO Backup: https://github.com/eduardolat/pgbackweb/releases

## Shared Product Pattern

성숙한 제품은 대체로 다음 흐름을 공유한다.

1. 연결을 등록하고 실제 접속과 도구를 점검한다.
2. 백업할 데이터베이스를 발견하거나 명시적으로 선택한다.
3. 저장 목적지와 보존 정책을 등록한다.
4. 소스·대상·일정을 백업 계획으로 결합한다.
5. 실행 이력과 스냅샷을 독립적으로 탐색한다.
6. 스냅샷을 선택해 복원하거나 정기 복원 검증을 수행한다.
7. 실패 알림과 마지막 성공/검증 시각을 한눈에 보여준다.

현재 제품은 3~4번의 실행 코어와 보안 깊이가 강하지만, 1~2번과 5~7번의 운영 경험이
얕다. 따라서 다음 투자 순서는 엔진을 한꺼번에 늘리는 것보다 운영 흐름을 먼저 완성하는
것이 맞다.

## Differentiation

단순한 “지원 DB가 많은 무료 백업 도구”만으로는 Databasement와 비슷해진다. 현재 자산을
살린 차별점은 다음 세 가지다.

- 보안 기본값: 백업별 DEK, 서명 매니페스트, 목적지 불변성, 읽기·쓰기 자격증명 분리
- 증명 가능한 복구: L1/L2/L3와 정기 restore drill, 결과의 변조 방지 기록
- 로컬 우선 운영: Windows 데스크톱에서 시작해 필요할 때만 agent와 웹 제어면으로 확장

## Avoided Traps

- 지원 엔진 로고를 먼저 늘리고 복원 E2E를 뒤로 미루지 않는다.
- 엔진마다 의미가 다른 기능을 하나의 공통 체크박스로 위장하지 않는다.
- Windows Task Scheduler가 여러 독립 프로세스를 무제한 실행하게 두지 않는다.
- 최신 성공 한 건으로 최근 실패나 RPO 위반을 가리지 않는다.
- 경쟁 제품의 UI 외형만 복제하지 않고 연결·계획·실행·스냅샷의 도메인 분리를 먼저 한다.

## Conclusion

제품 방향은 “Databasement보다 넓다”가 아니라 “Databasement 수준으로 쉽고, 복구 가능성을
더 강하게 증명하는 Windows-first 연결형 백업 관리자”가 적절하다. 첫 출시는 기존 세
엔진의 완성도와 공통 운영 흐름으로 평가하고, 이후 엔진은 동일한 지원 계약을 통과할
때마다 추가한다.
