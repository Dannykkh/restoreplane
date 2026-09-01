# Connected Backup Platform v2: Overall Direction

## 한 문장 정의

여러 서버와 여러 데이터베이스를 연결해 발견하고, 정책 기반으로 백업하며, 어느 복구
지점이 실제로 복원 가능한지 증명하는 Windows 중심 데스크톱 백업 관리 도구를 만든다.

## 제품 판단

현재 제품과 Databasement는 결이 다르지 않다. 차이는 방향이 아니라 완성된 제품 표면과
실행 모델의 깊이다.

- Databasement에서 직접 배울 것: 연결 중심 탐색, 다중 DB 관리, 데이터베이스별 계획,
  목적지·보존 설정, 실행 이력과 복원까지 이어지는 정보 구조
- 현재 코드에서 지킬 것: Rust 기반 암호화·Manifest·fanout·native tool 실행·검증·복원
  기반, 로컬 보안 모델
- 그대로 가져오지 않을 것: 다른 런타임과 저장 모델을 통째로 이식하거나 현재 엔진을
  폐기하는 방식

따라서 선택한 전략은 `desktop-first modular strangler`다. UI와 도메인 모델은
Databasement 수준으로 재구성하고, 실행권은 현재 코어를 확장한 Windows service로 단계적으로
옮긴다.

## 최종 제품 구조

```text
Tauri Control Plane
    |
    | authenticated local IPC
    v
Windows Service Singleton
    |
    +-- Application Use Cases
    +-- Durable Dispatcher / Resource Admission
    +-- Process Supervisor / Reconciliation
    +-- Engine Providers
    +-- Artifact / Verification / Restore
    |
    +-- Operational Store
    +-- Secret Store
    +-- Signed Manifest + Artifact Replicas
```

### 권위 소스 분리

| 데이터 | 권위 소스 |
|---|---|
| 연결, 계획, queue, run, activity | Operational Store |
| 비밀번호, token, key material | Secret Store |
| 복구 artifact의 완전성과 무결성 | signed Manifest |
| UI의 Snapshot 목록 | 위 권위 소스에서 만든 projection |

### 실행 단위

```text
Connection
  -> Target
  -> Backup Plan Revision
  -> Plan Occurrence
  -> Backup Run
  -> Work Item
  -> Attempt
  -> Recovery Point
  -> Artifact Part
  -> Artifact Replica
```

Connection은 서버 접속 정보이고 Target은 그 서버에서 발견된 개별 데이터베이스다. 이
구분을 먼저 바로잡아야 “서버 하나에 DB 여러 개”가 예외가 아니라 기본 모델이 된다.

## 다중 DB 부하에 대한 원칙

Rust와 Tokio 자체가 병목은 아니다. 무게가 큰 것은 `pg_dump`, `mysqldump`, 압축,
암호화, 디스크 I/O, 네트워크 업로드 같은 외부 작업이다. 따라서 thread 수를 늘리는 대신
중앙 dispatcher가 다음 permit을 동시에 만족할 때만 작업을 시작한다.

| Permit | 초기 보수값 | 보호 대상 |
|---|---:|---|
| global backup | 4 | PC 전체 부하 |
| source host | 1 | 원본 DB 서버 |
| CPU-heavy | 1–2 | 압축·암호화 |
| destination | 2 | NAS·object storage |
| restore | 1 | 파괴적·고부하 복원 |

초기값은 제품 기본값일 뿐이며 실제 fixture benchmark 결과로 조정한다. queue는 메모리가
아니라 SQLite에 지속하고, 모든 claim은 lease와 Fencing Token을 사용한다. service가
죽어도 재시작 시 프로세스, DB 상태, Manifest와 목적지 객체를 대조해 수렴한다.

현재 구현은 `ServiceBackupWorker` OS thread를 총 4개로 고정하고 Connection Profile ID별
permit을 1개로 제한한다. 각 worker는 native dump child를 비동기로 감시하고 압축·암호화는
blocking encoder로 분리한다. 따라서 DB 수가 늘어도 thread를 DB 수만큼 만들지 않으며,
같은 원본 서버에는 dump 하나만 가하고 서로 다른 원본은 최대 4개까지 진행한다. 위 CPU,
destination, restore permit은 향후 자원 admission 계층에서 더 세분화할 조정 지점이다.

## 구현 우선순위

### 0. 진실성 게이트

stub, false-success, 검증 결과 누락, native tool 신뢰 경계, 로그 redaction, CI baseline을
먼저 바로잡는다. 이 게이트 전에는 새 UI가 기존 위험을 가리지 못하게 한다.

### 1. 도메인과 저장소

Connection/Target 분리, 불변 Plan Revision, Plan Occurrence 멱등성, Run/Attempt,
Recovery Point/Replica를 Operational Store에 정규화한다. legacy TOML은 shadow import와
diff까지만 준비한다.

### 2. 서비스 경계

Windows service singleton, SID 기반 local IPC, Secret Store, Tauri/CLI client를 만든다.
이 시점에는 production backup 실행권을 아직 옮기지 않는다.

### 3. 내구 실행 코어

persistent dispatcher, resource admission, Process Supervisor, lease/fencing, spool reservation,
crash reconciliation, atomic Recovery Point commit을 구현하고 fault injection을 통과한다.
그 뒤에만 legacy scheduler를 freeze하고 실제 execution authority를 전환한다.

### 4. 기존 엔진을 먼저 완성

PostgreSQL, MySQL, MariaDB의 실제 backup/verification/restore fixture를 통과시키고 retention,
replica repair, notification outbox를 운영 수준으로 만든다.

### 5. 제품 UI

Connections, Databases, Backup Plans, Activity, Snapshots, Recovery 순서의 연결 중심 IA로
바꾼다. Snapshot에는 성공 배지만 보여주지 않고 completeness, replica durability,
Protection Compliance, verification freshness를 구분해 표시한다.

### 6. 지원 DB 확장

Wave 1은 SQL Server BACPAC 후보 검증과 SQLite online backup이다. SQL Server DACPAC은
데이터 백업이 아니므로 대체 수단으로 쓰지 않는다. 이후 MongoDB, Firebird, 추가 목적지를
Engine Matrix와 실제 restore fixture를 통과한 순서로 승격한다. Redis/Valkey export는
restore runbook과 testbed 전까지 experimental이다.

### 7. 배포와 확장 경계

설치, service upgrade/drain, Operational Store online backup, support bundle, 단계적
rollout을 완성한다. Remote Agent와 HTTP/RBAC는 초기 v2에 섣불리 포함하지 않고 로컬
제품이 안정된 뒤 별도 프로토콜과 위협 모델로 연다.

## 출시 판단 기준

- 같은 Plan Occurrence에서 complete Recovery Point가 둘 이상 생기지 않는다.
- stale Fencing Token의 heartbeat와 commit 허용이 0건이다.
- required destination 실패를 “보호됨”으로 표시하지 않는다.
- 지원으로 표시한 엔진마다 backup과 실제 restore fixture가 통과한다.
- service kill, PC restart, disk full, network interruption 이후 queue와 artifact가
  설명 가능한 terminal 상태로 수렴한다.
- overwrite restore는 preflight와 대상 바인딩 confirmation 없이는 시작되지 않는다.
- cutover 전후 어느 순간에도 legacy scheduler와 service가 동시에 production 작업을
  실행하지 않는다.

## 범위 통제

v2의 성공은 “지원 DB 개수”보다 “연결한 DB들이 실제로 보호되고 복구 가능함을 한 화면에서
증명”하는 데 있다. 따라서 UI와 엔진 확장을 병행하되 다음 순서는 바꾸지 않는다.

```text
truth -> durable execution -> existing engine recovery -> product UI -> new engines -> remote scale
```
