# Local IPC and Tauri Command Specification

> HTTP API: initial v2에는 없음
> Transport: Windows local named pipe between Tauri/CLI and `restoreplane-service`
> Tauri command는 이 계약의 thin wrapper이며 native tool을 직접 실행하지 않는다.

## 1. Security Envelope

### Request

```json
{
  "protocolVersion": 1,
  "requestId": "uuid",
  "idempotencyKey": "optional-client-key",
  "issuedAtMs": 1785072000000,
  "expiresAtMs": 1785072030000,
  "command": "connection.probe",
  "payload": {}
}
```

### Response

```json
{
  "protocolVersion": 1,
  "requestId": "uuid",
  "ok": true,
  "result": {}
}
```

```json
{
  "protocolVersion": 1,
  "requestId": "uuid",
  "ok": false,
  "error": {
    "code": "CONNECTION_AUTH_FAILED",
    "category": "authentication",
    "retryable": false,
    "message": "데이터베이스 인증에 실패했습니다.",
    "details": {
      "phase": "probe"
    }
  }
}
```

규칙:

- pipe DACL과 client token SID 확인은 ADR-007을 따른다.
- unknown version, duplicate request, expired request, oversized body는 payload 처리 전에 거부한다.
- `idempotencyKey`는 enqueue와 destructive command에 필수다.
- mutating command는 `client SID + command + idempotencyKey + canonical payload digest`를
  `ipc_request_ledger`에 기록한다. 같은 key와 같은 payload는 기존 결과/Work Item을 반환하고,
  같은 key의 다른 payload는 `IPC_IDEMPOTENCY_CONFLICT`로 거부한다.
- destructive confirmation은 SID, action, resource ID/fingerprint, canonical payload digest,
  expiry에 바인딩한다. Operational Store에는 raw token이 아니라 digest만 저장하고, Run과
  Work Item 생성 transaction에서 한 번만 `consumed`로 전이한다.
- secret은 응답, error detail, Activity Event에 echo하지 않는다.
- application error는 stable code를 사용하고 Rust debug 문자열을 frontend 계약으로 노출하지 않는다.

## 2. Authorization Classes

| Class | Commands | Requirement |
|-------|----------|-------------|
| read | list/get/status/preview | 설치 owner SID 또는 승인된 administrator |
| operate | probe/discover/enqueue/cancel/retry/rebuild/test notification | owner SID, valid request |
| destructive/admin | overwrite restore, retention delete, secret rotate, migration/cutover, ACL change | owner/admin SID + bound confirmation token |

초기 v2는 설치 owner 한 명을 지원한다. 조직 RBAC는 이 IPC 계약에 넣지 않는다.

## 3. Shared Types

### Cursor Page

```json
{
  "items": [],
  "nextCursor": "opaque-or-null",
  "hasMore": false
}
```

### Resource Reference

```json
{
  "id": "uuid",
  "revision": 3
}
```

### Long-running Command Result

```json
{
  "workItemId": "uuid",
  "state": "queued",
  "deduplicated": false
}
```

장시간 작업은 완료 결과를 기다리지 않고 Work Item을 반환한다. UI는 `work-item.get`,
`activity.list` 또는 선택적 subscription으로 상태를 갱신한다.

Backup/Restore/Verification/Catalog Rebuild는 전용 Run을 Work Item에 연결한다. probe,
discovery, Replica repair, retention, tool rescan, notification test/delivery, migration,
control backup, support bundle 같은 나머지 장기 작업은 `Operation Run`을 통해 같은 durable
queue, Attempt, lease/fencing, cancellation 계약을 사용한다.

## 4. Command Catalog

### Service and Diagnostics

| Command | Auth | Request | Response | Frontend Caller |
|---------|------|---------|----------|-----------------|
| `service.status` | read | 없음 | version, instance ID, boot ID, schema, cutover, queue health | global shell |
| `service.diagnostics` | read | include counters | queue depth, leases, permits, spool, WAL, tool health | Overview/Settings |
| `service.prepare-confirmation` | operate | action, resource fingerprint | bound token, expiry | destructive workspace |
| `control.backup` | admin | destination path, includeSecrets=false | Work Item | Settings |
| `control.restore-prepare` | admin | bundle path | diff, schema, integrity, confirmation fingerprint | Settings |
| `support-bundle.create` | operate | time range, selected objects | Work Item | Activity/Settings |

### Connections and Database Targets

| Command | Auth | Request | Response | Frontend Caller |
|---------|------|---------|----------|-----------------|
| `connection.list` | read | cursor, filters | ConnectionProfile page | Connections |
| `connection.get` | read | connection ID | detail, latest Probe Result, Capability | inspector |
| `connection.create` | operate | engine, endpoint, TLS, tunnel, credential input | ConnectionProfile | wizard |
| `connection.update` | operate | ID, expected revision, patch | new revision | inspector |
| `connection.probe` | operate | connection ID | Work Item | wizard/inspector |
| `connection.discover` | operate | connection ID | Work Item | wizard/inspector |
| `connection.delete-impact` | read | connection ID | related Target/Plan/Run counts | delete dialog |
| `connection.archive` | admin | ID, confirmation token | archived profile | delete dialog |
| `target.list` | read | cursor, connection/engine/protection/drift filters | Database Target page | Databases |
| `target.select` | operate | discovery run ID, discovered keys | created/existing Targets | discovery step |
| `target.reconcile-drift` | operate | target ID, resolution | updated Target | inspector |

`connection.create/update`의 credential input은 service가 Secret Store에 저장한 뒤 응답에서
`SecretRef` metadata만 반환한다.

### Destinations

| Command | Auth | Request | Response | Frontend Caller |
|---------|------|---------|----------|-----------------|
| `destination.list` | read | cursor, kind/status filters | Destination page | Destinations |
| `destination.get` | read | ID | detail, latest Probe Result, Capability | inspector |
| `destination.create` | operate | kind, public config, writer/reader secret input | Destination | wizard |
| `destination.update` | operate | ID, expected revision, patch | updated Destination | inspector |
| `destination.probe` | operate | ID, probe mode | Work Item | wizard/inspector |
| `destination.delete-impact` | read | ID | Plan/Replica/retention impact | delete dialog |
| `destination.archive` | admin | ID, confirmation token | archived Destination | delete dialog |

### Backup Plans

| Command | Auth | Request | Response | Frontend Caller |
|---------|------|---------|----------|-----------------|
| `plan.list` | read | cursor, state/target filters | Backup Plan page | Backup Plans |
| `plan.get` | read | ID | current revision, Targets, Destinations, compliance | inspector |
| `plan.preview` | read | draft Plan Revision | capability, next runs, RPO, retention, expected limits | wizard |
| `plan.create` | operate | immutable draft | Plan + revision 1 | wizard |
| `plan.revise` | operate | plan ID, expected revision, immutable draft | new Plan Revision | inspector |
| `plan.pause` | operate | plan ID, expected revision | paused Plan | list/inspector |
| `plan.archive` | admin | plan ID, impact acknowledgment | archived Plan | delete dialog |
| `plan.run-now` | operate | plan ID, revision, manual idempotency key | Plan Occurrence + Work Items | list/inspector |

Plan Revision은 Target, Destination role, schedule/timezone, RPO, retention, verification,
notification, durability policy의 immutable snapshot이다.

### Work Items and Activity

| Command | Auth | Request | Response | Frontend Caller |
|---------|------|---------|----------|-----------------|
| `work-item.list` | read | cursor, state/kind/plan/target filters | Work Item page | Activity |
| `work-item.get` | read | ID | Attempt, lease, phase, wait reason, related objects | Activity inspector |
| `work-item.cancel` | operate | ID, expected state, idempotency key | cancel-requested state | inspector |
| `work-item.retry` | operate | ID, idempotency key | new Attempt or rejection reason | inspector |
| `activity.list` | read | cursor, time/severity/category/resource filters | Activity Event page | Activity |
| `activity.subscribe` | read | after sequence, filters | ordered event stream | global tray/Activity |

Subscription disconnect는 작업을 취소하지 않는다. 재연결은 마지막 sequence 이후부터 재개한다.

### Snapshots and Repair

| Command | Auth | Request | Response | Frontend Caller |
|---------|------|---------|----------|-----------------|
| `snapshot.list` | read | cursor, target/time/level/compliance filters | Snapshot page | Snapshots |
| `snapshot.get` | read | ID | Recovery Point, Parts, Replicas, verification, eligibility | inspector |
| `snapshot.rebuild-preview` | operate | Destination ID, time/prefix | candidate/rejected summary | rebuild wizard |
| `snapshot.rebuild` | operate | preview ID, idempotency key | Work Item | rebuild wizard |
| `replica.repair` | operate | Recovery Point ID, source Replica, Destination ID | Work Item | inspector |
| `orphan.list` | read | cursor, Destination/age filters | Orphan Artifact page | diagnostics |

### Restore and Verification

| Command | Auth | Request | Response | Frontend Caller |
|---------|------|---------|----------|-----------------|
| `restore.prepare` | operate | Snapshot ID, Replica ID, target Connection, new DB name, policy | eligibility, object inventory, target fingerprint | Recovery wizard |
| `restore.confirm` | admin when destructive | prepare ID, confirmation token, idempotency key | Restore Run + Work Item | Recovery wizard |
| `restore.get` | read | Restore Run ID | state, validation, evidence | Recovery |
| `verification.enqueue` | operate | Snapshot ID, level, purpose, idempotency key | Verification Run + Work Item | Snapshot inspector |
| `verification.get` | read | Verification Run ID | level, result, evidence, expiry | Snapshot/Recovery |

`restore.confirm`은 `restore.prepare` 뒤 target fingerprint, Snapshot, conflict policy가 같을 때만
성공한다. 새 DB가 기본이며 overwrite는 destructive/admin command다.

### Settings, Tools, and Notifications

| Command | Auth | Request | Response | Frontend Caller |
|---------|------|---------|----------|-----------------|
| `settings.runtime-get` | read | 없음 | permit, spool, retry settings | Settings |
| `settings.runtime-update` | admin | expected revision, patch | updated settings | Settings |
| `tool.list` | read | engine filter | tool identity and certification | Settings |
| `tool.rescan` | operate | engine/all | Work Item | Settings |
| `notification-channel.list` | read | 없음 | channels without secrets | Settings |
| `notification-channel.upsert` | admin | public config, Secret input | channel metadata | Settings |
| `notification-channel.test` | operate | channel ID | Work Item | Settings |
| `notification-rule.upsert` | operate | events, filters, quiet hours, channel IDs | rule | Settings/Plan |
| `migration.preview` | admin | source bundle | diff and blockers | migration wizard |
| `migration.cutover` | admin | preview ID, confirmation token | Work Item | migration wizard |

## 5. Stable Error Codes

| Prefix | Area | Examples |
|--------|------|----------|
| `IPC_` | envelope/auth | `IPC_UNAUTHORIZED_SID`, `IPC_REPLAYED_REQUEST` |
| `IPC_` | idempotency/confirmation | `IPC_IDEMPOTENCY_CONFLICT`, `IPC_CONFIRMATION_MISMATCH`, `IPC_CONFIRMATION_EXPIRED` |
| `CONNECTION_` | probe/discovery | `CONNECTION_TLS_FAILED`, `CONNECTION_TOOL_MISSING` |
| `DESTINATION_` | storage | `DESTINATION_OBJECT_LOCK_MISSING`, `DESTINATION_NO_SPACE` |
| `PLAN_` | validation | `PLAN_CAPABILITY_MISMATCH`, `PLAN_STALE_REVISION` |
| `QUEUE_` | Work Item | `QUEUE_STALE_FENCING_TOKEN`, `QUEUE_RETRY_EXHAUSTED` |
| `PROCESS_` | native tool | `PROCESS_TIMEOUT`, `PROCESS_ORPHANED` |
| `RECOVERY_POINT_` | commit | `RECOVERY_POINT_INCOMPLETE`, `RECOVERY_POINT_RECONCILE_REQUIRED` |
| `VERIFY_` | L1/L2/L3 | `VERIFY_SIGNATURE_FAILED`, `VERIFY_CONTENT_MISMATCH` |
| `RESTORE_` | Recovery | `RESTORE_TARGET_CHANGED`, `RESTORE_OBJECT_BLOCKED` |
| `MIGRATION_` | cutover | `MIGRATION_OLD_BINARY_GUARD`, `MIGRATION_ROLLBACK_REQUIRED` |

## 6. Contract Change Rules

- 새 command를 구현하면 이 파일에 command, auth, request, response, Frontend Caller를 추가한다.
- 같은 데이터를 다른 이름으로 반환하는 중복 command를 만들지 않는다.
- payload field 제거/의미 변경은 protocol version 변경 또는 호환 migration이 필요하다.
- Tauri wrapper와 CLI는 service command의 validation을 재구현하지 않는다.
- public HTTP/Agent protocol은 이 local v1 계약을 그대로 외부에 노출하지 않고 G5 ADR에서
  별도 위협 모델과 versioning을 정의한다.
