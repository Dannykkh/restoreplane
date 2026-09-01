# Research Synthesis

> 조사일: 2026-07-26
> 상세 자료: [codebase](research/codebase.md), [web](research/web.md),
> [GitHub](research/github.md), [competitors](research/competitors.md)

## Executive Finding

현재 제품과 Databasement는 결이 다르지 않다. 둘 다 DB 연결을 기반으로 반복 백업과 복원을
관리하려는 제품이다. 차이는 현재 구현의 중심이 보안 실행 코어에, Databasement의 중심은
연결·대상·계획·스냅샷을 관리하는 완성된 제품 흐름에 있다는 점이다.

따라서 방향은 재개발이나 Databasement 포크가 아니다. 현재 Rust 코어를 보존하고 그 위의
도메인 모델과 UI를 Databasement 수준으로 재구성하는 `기능적 흡수`가 맞다.

## Current Product Truth

### Reusable strengths

- Rust 기반 PostgreSQL, MySQL, MariaDB dump/restore 실행 경로
- Local, S3, SSH/SFTP 목적지와 spool-once fan-out
- 백업별 암호화, 서명 매니페스트, 불변성 capability
- 원격 매니페스트 reader, catalog rebuild, 복원 gate
- Windows Task Scheduler와 Tauri 데스크톱 기반

### Blocking gaps

- 서버 연결과 개별 DB 대상이 한 설정 단위에 섞여 있다.
- 연결 테스트가 실제 probe가 아니며 DB 자동 탐색 흐름이 없다.
- 스냅샷 inventory, 실행 activity, 스냅샷 기반 복원 UX가 미완성이다.
- 잡은 한 프로세스 안에서 순차 실행되고, 여러 스케줄러 프로세스 사이 전역 제한이 없다.
- `max_parallel_dumps`는 선언돼 있지만 실행 permit에 연결되지 않았다.
- 지원·검증·복원 결과가 UI와 catalog에 일관되게 반영되지 않는 production gap이 남아 있다.
- 웹 제어면, 사용자·RBAC, agent protocol은 아직 없다.

## Runtime Finding

Rust와 Tokio는 여러 DB 백업을 감당할 수 있다. 위험은 Rust 스레드 수가 아니라 외부 dump
프로세스, DB 서버 I/O, 압축·암호화 CPU, 로컬 디스크, 네트워크, 목적지 API를 제한 없이
동시에 점유하는 것이다.

필요한 구조는 job마다 스레드를 만드는 방식이 아니라 persistent queue와 중앙 dispatcher다.
dispatcher는 다음 permit을 동시에 획득한 작업만 실행해야 한다.

- 전역 실행 수
- 원본 호스트별 dump 수
- CPU 집약 단계 수
- 목적지별 upload 수
- restore 수

초기 보수값은 전역 2, 호스트별 1, CPU 1~2, 목적지별 2, restore 1로 시작하고 부하
테스트에서 조정한다. child process 취소, stdout/stderr drain, lease 만료, 재시작 복구도
같은 경계에서 처리한다.

## Engine Support Contract

“지원”은 enum과 로고 추가가 아니다. 각 엔진·버전·도구 조합은 다음 전 과정을 통과해야 한다.

1. `probe`: 접속, 인증, 버전, 권한, native tool 확인
2. `discover`: 백업 가능한 DB와 제외 사유 탐색
3. `preflight`: 용량, 공간, 옵션, 일관성 방식 확인
4. `backup`: 취소·timeout·로그 redaction이 적용된 백업
5. `verify`: L1 무결성, L2 포맷, 가능한 경우 L3 restore
6. `restore`: 명시된 대상과 충돌 정책으로 복원
7. `validate`: 복원 후 엔진별 smoke query

지원 순서는 다음이 합리적이다.

1. PostgreSQL, MySQL, MariaDB 운영 수준 완성
2. SQL Server BACPAC 데이터 포함 export/import 후보와 SQLite online backup
3. MongoDB와 Firebird
4. restore runbook/testbed가 준비된 Redis와 Valkey experimental export
5. SQL Server `.bak`과 원격 agent가 필요한 고급 모드는 별도 단계

DACPAC은 schema-only이므로 데이터 백업 지원 근거에 포함하지 않는다.

## Product Position

제품 한 문장은 다음과 같다.

> 여러 종류의 데이터베이스를 연결하고 예약 백업하며, 실제 복원 가능성까지 증명하는
> Windows-first 백업 관리자.

Databasement는 정보구조와 기능 폭의 기준선이다. 차별화는 보안 코어와 restore drill이다.

## Architecture Decision Preview

우선 경로는 `desktop-first modular strangler`다.

- Tauri는 제어 UI로 유지한다.
- Rust application service와 dispatcher를 UI 프로세스에서 논리적으로 분리한다.
- 운영 상태의 진실 소스는 SQLite로 옮기고 TOML은 export/import로 유지한다.
- 같은 작업 계약을 향후 Windows service, CLI, remote agent, HTTP API가 재사용한다.
- 웹·RBAC·SSO·MCP는 로컬 제품 흐름과 queue가 안정된 뒤 도입한다.

## Non-negotiable Gates

- 기존 세 엔진의 실제 백업→검증→복원 E2E가 안정되기 전 대규모 엔진 확장을 시작하지 않는다.
- dispatcher가 없는 상태에서 여러 스케줄 작업의 병렬 실행을 허용하지 않는다.
- 최근 실패와 RPO 위반을 최신 성공으로 가리지 않는다.
- restore drill을 통과하지 않은 엔진·버전은 “지원”으로 표시하지 않는다.
- 기존 암호화·서명·불변성·원격 복구 회귀 테스트를 계속 통과해야 한다.
