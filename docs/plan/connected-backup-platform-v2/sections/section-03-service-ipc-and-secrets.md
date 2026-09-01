# Section 03 — Windows Service, Local IPC, and Secret Custody

## Background

현재 Tauri와 CLI는 configuration library를 직접 호출하고 backup/verify/restore에서는
`restoreplane.exe` sidecar나 native tool을 시작한다. 앱 종료, 복수 GUI, Task Scheduler overlap,
service upgrade 상황에서 execution authority가 분산된다.

이 섹션은 Windows service singleton과 application boundary를 만들지만 production 작업을
아직 claim하지 않는다. section-04의 durable queue/fencing/supervisor/Reconciliation G1을
통과한 뒤에만 legacy scheduler를 freeze하고 service execution을 활성화한다.

## Requirements

### Service boundary

- `SVC-01` SCM이 관리하는 `restoreplane-service` instance 하나만 singleton lock을 소유한다.
- `SVC-02` service shell은 start/stop/preshutdown, health, schema/cutover 상태, IPC를 제공한다.
- `SVC-03` G1 전 execute capability는 `rehearsal_only`; backup/restore claim과 native process
  시작을 거부한다.
- `SVC-04` Tauri와 CLI는 application command client다. validation/domain logic을 복제하거나
  SQL/native tool에 직접 접근하지 않는다.
- `SVC-05` UI process가 종료돼도 service의 queued/running 작업에는 영향이 없다.

### Named-pipe authentication and command authorization

- pipe DACL은 service SID, 설치 owner SID, 승인된 administrator만 연결 가능하다.
- server는 client token을 impersonate해 실제 SID/session을 확인하고 read/operate/admin을
  command catalog대로 인가한다.
- envelope는 protocol version, request ID, idempotency key, issued/expiry, body length를
  검증한 뒤 payload를 decode한다.
- mutating command는 durable `ipc_request_ledger`를 사용한다. 같은 SID/command/key와 같은
  canonical payload는 기존 결과를 반환하고 다른 payload는 `IPC_IDEMPOTENCY_CONFLICT`다.
- long-running command는 IPC timeout 안에 `WorkItemId`를 반환하고 service가 실행을 소유한다.
- error는 stable code와 redacted detail만 포함하며 Rust debug/secret/resource existence를
  과다 노출하지 않는다.

### Destructive confirmation

- overwrite restore, retention delete, Connection/Destination archive, secret rotate, cutover,
  control-store restore, ACL 변경은 owner/admin SID와 confirmation을 요구한다.
- token은 SID, action, resource ID/fingerprint, canonical payload digest, expiry에 결합한다.
- raw token은 UI memory 밖에 장기 보존하지 않고 SQLite에는 BLAKE3 digest만 저장한다.
- ledger accept, confirmation `issued -> consumed` CAS, Run/Work Item 생성은 한 transaction이다.
- consumed/revoked/expired token은 terminal immutable이며 다른 target/SID/action/replay를
  거부한다.

### Secret Store

- secret payload의 유일한 권위는 service-owned Windows DPAPI 기반 Secret Store다.
- Operational Store, IPC response, Activity, support bundle에는 versioned Secret Ref와
  fingerprint만 저장한다.
- owner/service SID ACL을 분리하고 operation마다 필요한 secret role만 resolve한다.
- writer/reader/delete credential, DB auth, TLS/tunnel, notification auth를 역할별로 분리한다.
- transient plaintext는 최소 수명 buffer와 `zeroize`를 사용하며 child environment/argument/
  stderr로 누출되지 않게 한다.
- rotation은 새 Secret Ref version을 만들고 queued occurrence는 enqueue 당시 immutable
  revision의 version을 사용한다.

### Safe authority cutover

G1 evidence가 모두 같은 revision/build에서 통과한 경우에만 다음을 수행한다.

1. recovery bundle과 최종 shadow diff 확인
2. legacy scheduler freeze
3. service/queue health 확인
4. cutover marker와 minimum reader version 원자 기록
5. production claim enable과 old direct path guard

marker 뒤 첫 v2 data 전에는 rehearsal한 bundle rollback이 가능하다. 새 queue/run data 뒤에는
old binary downgrade를 금지하고 service authority를 유지한 채 forward-fix한다.

## Dependencies

- **Requires**: section-02 domain/schema/repository, ADR-001/005/006/007
- **Blocks**: section-05 storage integration, section-07 UI application client
- **Parallel**: section-04 runtime은 application interface 뒤에서 병렬 구현 가능하나 actual
  cutover는 양쪽 gate가 모두 끝난 후 한 번만 수행한다.

## Flow Diagram Nodes

### `connection-onboarding.mmd`

| Node | 구현 |
|---|---|
| `saveSecret` | secret payload를 저장하고 Secret Ref만 반환 |
| `recordFailure` | probe failure를 stable/redacted result와 Attention으로 기록 |

### `config-cutover.mmd`

| Node | 구현 |
|---|---|
| `freezeLegacy` | Task enabled 상태를 bundle에 보존한 뒤 새 trigger 차단 |
| `serviceHealthy` | instance/schema/queue/G1/build 일치 검사 |
| `unfreezeLegacy` | marker 전 실패에만 기존 enablement 복원 |
| `writeMarker` | authority와 minimum reader version 원자 기록 |
| `disableOldPath` | CLI/Tauri/Task direct-run guard |
| `newData` | 첫 v2 control/queue/run write 여부 판정 |
| `rollbackBundle` | new data 전 marker/scheduler 상태 원복 |
| `forwardFix` | new data 뒤 service authority 유지 |
| `complete` | 단일 execution authority 확인 |

`serviceHealthy=아니요`는 unfreeze 후 stopped다. marker 뒤 장애는 `newData` 경계를 기준으로
rollback 또는 forward-fix 중 하나만 선택한다.

## Reference Libraries

| Library/API | 기준 | 용도 |
|---|---|---|
| Windows Service/SCM APIs | 구현 시 Microsoft 공식 문서 확인 | lifecycle, preshutdown |
| Windows Named Pipe/ACL/Impersonation APIs | 구현 시 공식 문서 확인 | local transport와 SID auth |
| `windows` crate | lockfile에서 exact pin | Win32 bindings, token/ACL/Job 연계 |
| `tokio` | workspace pin | bounded IPC accept/read/write |
| `serde`, `serde_json` | workspace pin | versioned envelope |
| `zeroize`, DPAPI wrapper | workspace pin/공식 API 확인 | secret custody |
| `rusqlite =0.31.0` | 유지 | replay/confirmation transaction |

## Implementation Details

### 1. Application crate

`restoreplane-application`에 command/result DTO와 use case trait를 둔다. Connection, Destination,
Plan, Snapshot, Work Item, Restore, Settings 명령은 domain/repository를 호출한다. Tauri DTO,
rusqlite row, native process type을 공개 계약으로 내보내지 않는다.

### 2. Service host

`restoreplane-service`는 SCM entry, singleton authority lock, structured shutdown token, writer actor,
IPC server를 조립한다. start 시 schema checksum/cutover/minimum reader를 먼저 검사하고 mismatch면
read-only diagnostics만 노출한다. stop은 accept 중단→새 claim 중단→client drain→writer
checkpoint 순서다. G1 전 runtime executor는 feature flag가 아니라 gate result로 명시적 거부한다.

### 3. IPC codec/server/client

- length-prefixed bounded frame과 protocol v1 discriminated command를 사용한다.
- decode 전 version/time/length, dispatch 전 SID/authorization/idempotency를 검사한다.
- read command는 key가 없어도 되지만 mutation/enqueue/destructive는 필수다.
- subscription은 Activity sequence 이후 재개하며 disconnect가 Work Item cancel을 뜻하지 않는다.
- Tauri command와 CLI는 동일 client library를 사용한다.

### 4. Confirmation service

`prepare-confirmation`은 authoritative resource fingerprint와 canonical payload digest를 service가
다시 계산해 짧은 random token을 발급한다. confirm 시 token digest를 constant-time 비교하고
repository의 atomic consume operation만 사용한다. in-memory cache만으로 replay를 막지 않는다.

### 5. Secret custody

Secret Store는 create/resolve/rotate/revoke를 제공하고 public metadata와 secret buffer type을
분리한다. service account ACL test, canary redaction scan, crash dump/log exclusion을 둔다.
Process Supervisor에 필요한 secret은 section-04의 restricted environment builder로만 전달한다.

### 6. Client strangler and cutover

먼저 read/config/diagnostics를 Tauri/CLI client로 전환한다. production run은 legacy에 남긴다.
section-04 G1 뒤 migration command가 scheduler freeze, service health, marker, claim enable,
old guard를 실행한다. dual-write와 두 executor의 병행 운영은 허용하지 않는다.

## Test Scenarios

### Normal

| ID | 입력 | 기대 |
|---|---|---|
| S03-N01 | owner SID read/operate command | 인가되고 stable response |
| S03-N02 | 같은 mutation key/payload 재전송 | 기존 result, `deduplicated=true` |
| S03-N03 | destructive token 일치 | token 1회 consume와 Work Item 원자 생성 |
| S03-N04 | Tauri 종료 후 running service | 작업과 IPC state 유지 |
| S03-N05 | secret rotate 후 새 revision enqueue | 새 Ref 사용, 기존 occurrence는 이전 version |

### Error/Security

| ID | 입력 | 기대 |
|---|---|---|
| S03-E01 | unauthorized/cross-session SID | payload 처리·상태 변경 0 |
| S03-E02 | unknown version/oversize/expired frame | decode/dispatch 전 stable IPC 오류 |
| S03-E03 | 같은 key의 다른 payload | idempotency conflict, 첫 결과 불변 |
| S03-E04 | 다른 SID/target/action 또는 expired token | consume 0, 감사 event |
| S03-E05 | consumed token replay | trigger/CAS 거부 |
| S03-E06 | Secret Store ACL 위반 | secret 반환 0, redacted error |
| S03-E07 | G1 전 execute/cutover | rehearsal-only 오류, scheduler 불변 |

### Edge/Fault

| ID | 입력 | 기대 |
|---|---|---|
| S03-X01 | frame 중간 disconnect | partial command 실행 0, buffer zeroize |
| S03-X02 | service 재시작 후 request replay | durable ledger에서 기존 결과 |
| S03-X03 | 두 service instance 동시 start | 하나만 singleton, 다른 하나 종료 |
| S03-X04 | scheduler freeze 뒤 health 실패 | marker 전 unfreeze, legacy authority |
| S03-X05 | marker 뒤 첫 v2 data 전 crash | bundle rollback 가능 |
| S03-X06 | 첫 v2 data 뒤 downgrade | old binary guard, forward-fix |

## Implementation Strategy

### Red

unauthorized SID, envelope fuzz, replay/payload conflict, confirmation binding/replay, Secret canary,
service singleton, G1-before-cutover tests를 먼저 실패시킨다.

### Green

non-executing service shell, bounded named pipe, application client, durable ledger, digest-only
confirmation, DPAPI store를 최소 구현한다. legacy run path는 유지한다.

### Refactor

Win32 handle RAII, codec/application/auth 분리, common Tauri/CLI client, redaction/zeroize helper를
정리한다. IPC handler 안에 domain validation이나 SQL을 복제하지 않는다.

## Quality Gate

- [ ] section-02 schema/repository gate 통과
- [ ] 두 service instance 중 execution authority 하나
- [ ] unauthorized/replay/oversize/fuzz corpus 통과
- [ ] token binding과 terminal immutability 통과
- [ ] secret canary가 DB/IPC/log/support bundle에서 0건
- [ ] Tauri 종료가 service 작업을 취소하지 않음
- [ ] G1 전 production claim/freeze/marker 0건
- [ ] G1 뒤 fault matrix에서 dual-run 0건
- [ ] fmt/clippy/workspace/Windows integration 통과

## Risk and Rollback

| Risk | Mitigation | Rollback |
|---|---|---|
| SID/ACL 오구성 | deny-by-default와 실제 multi-user fixture | service 중지, legacy 유지 |
| token replay | durable ledger+atomic consume+terminal trigger | command 거부, token revoke |
| secret 유출 | role binding, zeroize, canary scan | rotate/revoke, affected build block |
| 조기 cutover | G1 build/revision all-required | marker 전 scheduler 원복 |
| marker 후 split brain | old path guard와 singleton | new data 전 bundle; 이후 forward-fix |
| protocol drift | versioned envelope/golden tests | v1 client 유지 또는 explicit version bump |

## Acceptance Criteria

- [ ] service shell이 GUI와 독립적으로 실행되고 G1 전 execute하지 않는다.
- [ ] pipe DACL과 token SID가 command 등급을 강제한다.
- [ ] mutating replay와 다른-payload 충돌이 재시작 후에도 결정적이다.
- [ ] destructive token이 바인딩되고 한 번만 소비된다.
- [ ] secret payload가 Operational Store와 응답에 없다.
- [ ] Tauri/CLI가 동일 application IPC client를 사용한다.
- [ ] cutover 어느 시점에도 legacy와 service가 동시에 production 실행하지 않는다.

## Files to Create/Modify

```text
crates/restoreplane-application/src/{lib,commands,results,use_cases}.rs
crates/restoreplane-service/src/{main,scm,host,shutdown}.rs
crates/restoreplane-service/src/ipc/{codec,server,auth,confirmation}.rs
crates/restoreplane-service/src/secrets/{mod,dpapi,acl}.rs
crates/restoreplane-service/tests/{ipc_security,service_singleton,secret_canary,cutover}.rs
crates/restoreplane-cli/src/client.rs
apps/console/src-tauri/src/{client,commands}.rs
crates/restoreplane-scheduler/src/xml.rs
installer/service scripts
Cargo.toml
Cargo.lock
```
