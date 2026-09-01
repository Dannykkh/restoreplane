<!-- SECTION_MANIFEST
section-01-truth-and-release-gates
section-02-domain-and-operational-store
section-03-service-ipc-and-secrets
section-04-durable-dispatcher
section-05-recovery-point-and-storage
section-06-existing-engine-certification
section-07-product-ui-foundation
section-08-snapshots-and-recovery-ui
section-09-new-engine-wave-one
section-10-expansion-and-launch
END_MANIFEST -->

# Connected Backup Platform v2 Implementation Sections

## Dependency Graph

| Section | Depends On | Blocks | Parallelizable |
|---|---|---|---|
| section-01-truth-and-release-gates | - | 02 | 아니요, 최초 게이트 |
| section-02-domain-and-operational-store | 01 | 03, 04, 07 | 아니요, 공통 모델 |
| section-03-service-ipc-and-secrets | 02 | 05, 07 | 04와 병렬 |
| section-04-durable-dispatcher | 02 | 05 | 03과 병렬 |
| section-05-recovery-point-and-storage | 03, 04 | 06 | 아니요, runtime 통합 |
| section-06-existing-engine-certification | 05 | 08, 09 | 07과 병렬 |
| section-07-product-ui-foundation | 02, 03 | 08 | 06과 병렬 |
| section-08-snapshots-and-recovery-ui | 06, 07 | 10 | 09와 병렬 |
| section-09-new-engine-wave-one | 06 | 10 | 08과 병렬 |
| section-10-expansion-and-launch | 08, 09 | - | 아니요, 최종 게이트 |

## Execution Order

1. section-01-truth-and-release-gates
2. section-02-domain-and-operational-store
3. section-03-service-ipc-and-secrets와 section-04-durable-dispatcher 병렬
4. section-05-recovery-point-and-storage
5. section-06-existing-engine-certification과 section-07-product-ui-foundation 병렬
6. section-08-snapshots-and-recovery-ui와 section-09-new-engine-wave-one 병렬
7. section-10-expansion-and-launch

병렬 구현은 파일 소유권을 분리한다. 공통 DTO나 migration 변경이 필요하면
section-02를 먼저 수정하고 소비 섹션은 그 공개 계약만 사용한다.

## Flow Diagram Mapping

아래 노드 매핑은 구현 검증 시 각 노드의 단일 주 소유 섹션을 정한다. 다른 섹션은 해당
계약을 호출할 수 있지만 상태 전이 구현을 중복하지 않는다.

| Section | Flow Diagram | Owned Nodes |
|---|---|---|
| section-01-truth-and-release-gates | `config-cutover.mmd` | `start`, `durableGate`, `rehearsalOnly`, `stopped` |
| section-02-domain-and-operational-store | `connection-onboarding.mmd`, `config-cutover.mmd` | `createConnection`, `runProbe`, `probeOk`, `discoverTargets`, `discoveryChanged`, `recordDrift`, `keepProjection`, `exportLegacy`, `shadowImport`, `validateDiff`, `discardShadow` |
| section-03-service-ipc-and-secrets | `connection-onboarding.mmd`, `config-cutover.mmd` | `saveSecret`, `recordFailure`, `freezeLegacy`, `serviceHealthy`, `unfreezeLegacy`, `writeMarker`, `disableOldPath`, `newData`, `rollbackBundle`, `forwardFix`, `complete` |
| section-04-durable-dispatcher | `scheduled-backup-dispatch.mmd`, `crash-reconciliation.mmd` | `tick`부터 `processExit`, `releaseAndRetry`, `recordTerminal`, `failed`; F04 전체 |
| section-05-recovery-point-and-storage | `scheduled-backup-dispatch.mmd`, `recovery-point-commit.mmd` | `stageArtifacts`, `commitFlow`; F03 전체 |
| section-06-existing-engine-certification | `snapshot-restore.mmd` | `loadEligibility`, `eligible`, `blockRestore`, `runPreflight`, `preflightOk`, `recordPreflight`, `createRestore`, `executeRestore`, `verifyResult`, `verified`, `markAttention`, `success`, `failed` |
| section-07-product-ui-foundation | `connection-onboarding.mmd` | `start`, `enterProfile`, `showAction`, `endFailed`, `chooseTargets`, `profileReady` |
| section-08-snapshots-and-recovery-ui | `snapshot-restore.mmd` | `selectSnapshot`, `chooseMode`, `overwrite`, `issueToken`, `confirmIdentity`, `stopped` |
| section-09-new-engine-wave-one | - | Engine Matrix provider 계약과 SQL Server/SQLite fixture 흐름 |
| section-10-expansion-and-launch | - | Agent·추가 engine·destination은 별도 승인 게이트; release/rollback orchestration |

## Section Summaries

### section-01-truth-and-release-gates

현재 false-green 경로를 제거하고 baseline, feature flag, release gate, rollback 증거를 먼저
고정한다.

### section-02-domain-and-operational-store

Connection과 Target을 분리하고 Plan Revision부터 Artifact Replica까지의 정규화된 도메인,
SQLite Operational Store, legacy shadow importer를 구현한다.

### section-03-service-ipc-and-secrets

Windows service singleton, SID 기반 local IPC, Secret Store custody, execution authority
cutover를 구현한다.

### section-04-durable-dispatcher

Plan Occurrence 멱등성, persistent queue, lease/fencing, resource admission, Process Supervisor,
crash reconciliation을 구현한다.

### section-05-recovery-point-and-storage

multipart artifact, signed Manifest, destination replica, spool, durability/compliance, retention,
repair를 하나의 복구 지점 생명주기로 묶는다.

### section-06-existing-engine-certification

PostgreSQL, MySQL, MariaDB의 backup/verify/restore를 실제 fixture와 native tool로 재인증하고
알림 outbox까지 운영 계약을 완성한다.

### section-07-product-ui-foundation

Databasement 수준의 연결 중심 IA, 디자인 토큰, 접근성, IPC data layer, Connection/Database/
Plan/Activity 화면 기반을 구현한다.

### section-08-snapshots-and-recovery-ui

Snapshot inventory, artifact/replica 검사, 안전한 복원 wizard, preflight, overwrite 확인,
repair와 restore drill UX를 구현한다.

### section-09-new-engine-wave-one

같은 provider 계약으로 SQL Server BACPAC 후보와 SQLite 온라인 백업을 검증하고 Engine
Matrix에서 지원 수준을 정직하게 노출한다.

### section-10-expansion-and-launch

MongoDB/Firebird/추가 목적지와 실험적 Redis/Valkey, 향후 Agent/HTTP 경계를 별도 게이트로
관리하고 설치·업데이트·관측·지원 번들·단계적 출시를 완료한다.

## Ecosystem Coverage

| 기능 생태계 | 담당 섹션 | 상태 |
|---|---|---|
| Connections, Discovery, Targets | 02, 07 | 커버됨 |
| Destinations, Artifact, Retention | 05 | 커버됨 |
| Plans, Scheduling, Queue | 02, 04 | 커버됨 |
| Service, Local IPC, Secrets | 03 | 커버됨 |
| Backup, Verification, Restore | 05, 06, 08 | 커버됨 |
| Snapshots, Activity, Attention | 04, 05, 07, 08 | 커버됨 |
| Notifications | 06 | 커버됨 |
| Configuration Migration/Cutover | 01, 02, 03 | 커버됨 |
| Engine Expansion | 09, 10 | 커버됨 |
| Remote Agent, HTTP/RBAC | 10 | 명시적 후속 범위; 초기 v2에서는 제외 |
