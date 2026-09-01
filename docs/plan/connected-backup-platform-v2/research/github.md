# GitHub Ecosystem Research

> 조사일: 2026-07-26
> 범위: 공개 저장소와 각 프로젝트의 공식 문서
> 주의: 일부 자동 조사 작업이 시간 제한 안에 결과를 남기지 못해, 확인 가능한 공식
> 저장소와 문서만으로 보수적으로 정리했다.

## 1. Databasement

- 저장소: https://github.com/David-Crty/databasement
- 문서: https://david-crty.github.io/databasement/
- 라이선스: MIT
- 구현: Laravel 12, PHP 8.4, Livewire, Mary UI, daisyUI, Tailwind CSS 4
- 핵심 모델: `DatabaseServer`, `Volume`, `Backup`, `Snapshot`, `BackupJob`
- 핵심 서비스: 연결 확인, 백업, 복원, 스냅샷, 원격 에이전트 실행

Databasement의 가장 큰 가치는 특정 덤프 알고리즘이 아니라 제품 경계다. 서버 연결을
등록하고, DB를 탐색하고, 저장 볼륨과 백업 계획을 연결한 뒤, 스냅샷·로그·복원을 같은
제어면에서 운영한다. 원격 에이전트, REST API, MCP, 팀 기능까지 확장되는 구조도 확인된다.

### 직접 가져올 기준

- 연결 프로필과 백업 대상 DB의 분리
- 데이터베이스·저장 볼륨·백업 계획·스냅샷·실행 로그의 독립 메뉴
- 엔진별 capability와 사전 점검
- 로컬 및 원격 실행기의 공통 작업 계약
- 스냅샷에서 시작하는 복원 흐름

### 그대로 가져오지 않을 부분

- PHP/Laravel 코드를 제품 코어 의존성으로 채택하지 않는다.
- 현재 Rust 코어의 암호화·서명·불변성·다중 목적지·원격 복구 기능을 폐기하지 않는다.
- Databasement의 지원 표를 검증 없이 복제하지 않는다. 엔진별 실제 복원 테스트가
  끝난 조합만 지원으로 표시한다.

## 2. Databasus

- 저장소: https://github.com/RostislavDugin/postgresus

Postgresus에서 Databasus로 확장된 프로젝트다. PostgreSQL 중심에서 MySQL, MariaDB,
MongoDB 등으로 범위를 넓히고 S3, Google Drive, FTP와 여러 알림 채널을 제공한다.
특히 PITR과 복원 검증을 제품 신뢰의 핵심으로 다룬다.

시사점은 엔진 수보다 복원 가능성의 지속 검증이 더 강한 차별점이라는 것이다.
우리 제품도 `마지막 백업 성공`과 `마지막 복원 검증 성공`을 별도 상태로 보여야 한다.

## 3. pgBackWeb / UFO Backup

- 릴리스: https://github.com/eduardolat/pgbackweb/releases

Go 기반의 웹 백업 관리자이며 역사적으로 PostgreSQL에 집중했다. 단일 엔진 제품은
연결·예약·로그·복원 경험을 빠르게 다듬을 수 있지만, 다중 엔진 제품은 capability
차이를 숨기면 안 된다는 비교 기준을 준다.

## 4. Portabase

- 공식 사이트: https://portabase.io/

자체 호스팅 제어면과 에이전트를 사용하고 PostgreSQL, MySQL, MongoDB, SQLite 등의
백업·보존·복원을 표방한다. 공개 페이지에서 확인되는 핵심 패턴은 제어면과 DB 인접
에이전트의 분리다. 세부 구현과 모든 지원 조합은 공개 자료만으로 확인되지 않아
요구사항 참고 수준으로만 사용한다.

## 5. Reuse Decision

| 후보 | 결정 | 이유 |
|------|------|------|
| Databasement 코드 포크 | 채택하지 않음 | 현재 Rust 보안 코어와 런타임을 이중화하고 장기 유지비가 커짐 |
| Databasement 정보구조·도메인 모델 | 채택 | 사용자의 원래 목적과 직접 일치하며 제품 경계가 검증됨 |
| 기존 Rust 코어 | 유지·정리 | 보안, 원격 reader, fan-out, catalog 자산을 재사용 가능 |
| 공통 엔진 어댑터 계약 | 신규 도입 | 엔진별 구현을 제품 지원 계약으로 통제하기 위해 필요 |
| 중앙 dispatcher와 persistent queue | 신규 도입 | 여러 DB의 동시 실행을 안전하게 제한하고 재시작 복구하기 위해 필요 |
| 원격 agent | 후속 도입 | 로컬 제품을 먼저 완성한 뒤 동일 작업 계약으로 확장 가능 |

## Conclusion

Databasement 전체 코드를 이식하는 것보다, 그 제품 모델과 화면 구조를 기준선으로 삼아
현재 Rust 실행 코어를 `ConnectionProfile → DatabaseTarget → BackupPlan → BackupRun →
Snapshot` 흐름에 맞게 재구성하는 편이 위험과 비용이 낮다. 에이전트와 웹 제어면은 이
도메인 경계가 안정된 뒤 같은 계약 위에 얹는다.
