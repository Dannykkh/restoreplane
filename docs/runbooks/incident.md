# Production incident runbook

이 절차의 우선순위는 새 작업을 더 만드는 것이 아니라 현재 authority와 복구 가능성을
보존하는 것이다. 다음 safety invariant는 허용치가 항상 0이다.

- 설명되지 않은 missed occurrence
- occurrence/target당 duplicate Recovery Point
- stale fence commit
- support bundle secret canary match
- evidence 없는 capability promotion

하나라도 관찰되면 rollout을 중단하고 새 설치·cutover·provider 승격을 금지한다.

## 1. Authority 보존과 초기 triage

```powershell
$cli = "$env:ProgramFiles\DbBackup\dbbackup.exe"
$status = & $cli service status | ConvertFrom-Json
$diagnostics = & $cli service diagnostics | ConvertFrom-Json
```

status가 응답하고 데이터 손상을 확대할 가능성이 있으면 새 claim을 닫는다.

```powershell
& $cli service drain
```

service가 응답하지 않는다고 legacy scheduler를 임의 enable하지 않는다. cutover marker와
Operational Store authority를 먼저 확인한다. 동시에 두 execution path를 켜는 것은 금지다.

## 2. 최소 증거 수집

다음을 incident ID와 release revision/build ID에 연결한다.

- `service status`
- `service diagnostics`
- 관련 update journal과 control backup manifest
- G0~G5 evidence locator와 checksum
- signer thumbprint, 설치 package build ID
- 영향을 받은 Connection/Plan/Occurrence/Run/Work/Recovery Point ID

지원 번들은 raw stdout/stderr, environment, connection string, backup content를 포함하지
않는 allowlist 자료다.

```powershell
& $cli service support-bundle
```

비밀값을 별도 텍스트 파일에 복사하지 않는다. support bundle 생성이 canary 검출로
거부되면 archive를 우회 생성하지 말고 해당 secret을 rotate한 뒤 redaction 결함을
수정한다.

## 3. 증상별 판단

| 증상 | 확인 | 안전한 조치 |
|---|---|---|
| queue/heartbeat freshness 저하 | queue state, active/stale lease, worker health | drain 유지, stale fence reconcile 후 재개 |
| spool 부족 또는 quarantine 증가 | reservation, reported free bytes, quarantine | 새 claim 중단, 원 artifact 보존, 용량 확보 |
| tool trust/signature 오류 | native tool inventory와 signature status | 해당 provider disable, 검증된 tool만 재등록 |
| destination probe/Replica 실패 | Destination 상태, required Replica compliance | affected destination 신규 작업 중단, reader/repair 검증 |
| notification backlog | outbox pending/delivering/stale | backup authority 유지, delivery worker만 복구 |
| telemetry archive 정체 | activity archive count/last time, low-priority queue | claims 유지, archive root/용량을 복구하고 bounded maintenance 재실행 |
| update pending | marker, journal current stage, minimum reader | `update.md` disposition에 따라 restore 또는 forward-fix |
| migration drift | schema version/checksum, diagnostics-only code | write 금지, compatible build와 control backup 준비 |
| duplicate/stale commit | Occurrence, Attempt, fence, process ledger | cohort 중단, immutable evidence 보존, 수동 SQL 수정 금지 |
| expired `committing` claim이 Attention | persisted Recovery Point/Manifest key, required Destination reader, signing key, payload metadata | claim을 수동 complete/requeue하지 말고 아래 commit recovery 경계를 확인 |

### `committing` startup recovery 확인

정상 worker는 fenced claim을 `committing`으로 옮길 때 Recovery Point ID와 signed Manifest
object key를 Operational Store에 먼저 기록한다. payload 뒤에 게시되는 signed Set Manifest가
마지막 logical commit marker다. startup reconciliation은 새 ID를 만들거나 dump를 즉시
재실행하지 않고 이 persisted intent를 사용한다.

다음 조건이 모두 맞을 때만 정상 commit과 같은 validator가 Recovery Point를 정산한다.

- 모든 required Destination이 동일한 canonical signed Manifest bytes를 반환한다.
- trusted signature와 occurrence, Run, Scope, engine, Recovery Point ID, Manifest key가 일치한다.
- Manifest의 Destination 집합이 현재 immutable Plan Revision binding과 일치한다.
- 각 payload의 metadata가 object 존재와 signed ciphertext byte 수를 확인한다.

payload metadata 확인은 database-sized object를 읽거나 다시 hash하지 않는다. startup은 최대
1,000개 expired claim을 스캔하고 commit evidence 확인에는 batch 전체가 공유하는 20초 예산을
쓴다. 누락, mismatch, reader 불가, timeout은 complete의 근거가 아니며 Attention으로 남는다.
이 경우 required Destination의 signed Manifest와 object를 수정·덮어쓰기·삭제하지 말고 reader
연결, signing-key trust, persisted IDs, current binding을 support bundle과 함께 조사한다. startup
metadata 확인을 L1 checksum verification 통과로 해석해서는 안 된다.

### Periodic recovery and native-process evidence

Reconciliation runs once before service readiness and every five seconds afterward. A pass scans
at most 1,000 expired claims and all Destination commit-evidence checks in that pass share one
20-second budget. The next pass receives a fresh budget. Do not interpret an `Attention` result as
a failed retry: a live unowned native child is deliberately held until its PID, creation time,
absolute image path, and SHA-256 evidence show that it exited, disappeared, or was reused.

If a mutation reports `APPLICATION_OUTCOME_UNKNOWN` or `IPC_REQUEST_INCOMPLETE`, do not submit a
new mutation immediately. Refresh the affected inventory and Activity first. The service has
intentionally refused to re-run the handler because the domain write may already have committed.

If a backup worker reports `AwaitingReconciliation` or remains `committing` after a publication
deadline, do not delete spool/destination objects and do not enqueue a replacement dump. The
external outcome is unknown after the commit boundary. Preserve the recovery intent and wait for
lease expiry plus the next five-second reconciler pass; missing or mismatched required evidence is
expected to remain Attention for operator investigation.

## 4. 복구 방식 선택

- cutover marker 뒤 첫 v2 write 전 clean epoch: `rollback.md`의 offline clean rollback
- update의 replace 이전: 검증된 previous binary 자동 복구
- schema migration 또는 새 v2 write 이후: claims를 닫은 compatible forward-fix
- Operational Store 손상: 검증된 online control backup의 독립 rehearsal 결과를 확인한 뒤
  승인된 복구 절차
- provider 결함: provider claim을 강등하고 기존 service authority는 유지

수동 DB UPDATE, marker 삭제, fence 재사용, artifact overwrite는 audit와 idempotency를
깨므로 금지한다.

## 5. 재개 조건

- 원인과 영향 범위가 식별되었다.
- orphan process, stale lease/allocation, duplicate commit이 0이다.
- 필요한 restore drill과 reader 검증이 통과했다.
- current revision/tree의 G5 자동 행렬이 통과했다.
- safety invariant가 다시 모두 0이다.
- rollout stop metric을 넘은 경우 owner가 재개를 명시 승인했다.

재개 전 incident timeline, disposition, 수정 build, 검증 evidence, 남은 risk를 기록한다.
