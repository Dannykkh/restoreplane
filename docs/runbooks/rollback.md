# Clean cutover rollback runbook

이 절차는 cutover marker 뒤 첫 v2 control/queue/run write가 생기기 전의 clean epoch에만
사용한다. `CONFIG_CUTOVER_FORWARD_FIX_REQUIRED`가 나오면 legacy rollback을 중단하고 compatible
service build로 forward-fix한다.

## Preconditions

- elevated PowerShell을 설치 owner 계정으로 실행한다.
- recovery bundle, freeze receipt, G1 evidence가 protected
  `%ProgramData%\DbBackup\<cutoverVerifier.evidenceDirectory>` 안에 보존되어 있다.
- `cutoverMarker`, `rollbackId`, `rolledBackAtMs`를 작업 기록에 저장한다. 재시도 때 새 값을 만들지
  않는다.

## Procedure

1. 현재 marker와 recovery disposition을 보존한다.

   ```powershell
   $status = dbbackup service status | ConvertFrom-Json
   $cutoverMarker = $status.result.cutoverMarker
   if ($status.result.recoveryDisposition -ne 'rollback_eligible') {
       throw "Clean rollback is not permitted; use forward-fix."
   }
   ```

2. 서비스를 정지하고 실제 `Stopped` 상태를 확인한다.

   ```powershell
   Stop-Service -Name DbBackupService
   (Get-Service -Name DbBackupService).Status
   ```

3. 설치된 service account SID를 확인한다. LocalSystem이면 `S-1-5-18`이다. 다른 계정이면 SCM의
   `StartName`을 SID로 변환한다.

   ```powershell
   $startName = (Get-CimInstance Win32_Service -Filter "Name='DbBackupService'").StartName
   $serviceSid = if ($startName -eq 'LocalSystem') {
       'S-1-5-18'
   } else {
       ([System.Security.Principal.NTAccount] $startName).Translate(
           [System.Security.Principal.SecurityIdentifier]
       ).Value
   }
   ```

4. 최초 시도에서만 replay identity와 timestamp를 만들고 안전한 작업 기록에 보존한다.

   ```powershell
   $rollbackId = [Guid]::NewGuid().ToString()
   $rolledBackAtMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
   ```

5. marker를 동일하게 두 번 제공해 offline recovery를 실행한다.

   ```powershell
   dbbackup service recover-cutover `
       --service-sid $serviceSid `
       --cutover-marker $cutoverMarker `
       --confirm-cutover-marker $cutoverMarker `
       --recovery-bundle-manifest bundle-manifest.json `
       --rollback-id $rollbackId `
       --rolled-back-at-ms $rolledBackAtMs `
       --reason post-cutover-health-failed
   ```

6. 성공 JSON의 `status=completed`, `taskCount`, `rollbackCreated`를 보존한다. CLI 직접 실행이 다시
   허용되는지 확인하고 service는 운영자가 새 cutover를 결정할 때까지 rehearsal 상태로 둔다.

## Failure handling

| 오류 코드 | 상태 | 조치 |
|---|---|---|
| `SERVICE_SINGLETON_ALREADY_RUNNING` | service 또는 다른 recovery가 실행 중 | service를 완전히 정지하고 단일 operator만 남긴 뒤 재시도 |
| `CONFIG_CUTOVER_RECOVERY_UNAUTHORIZED` | owner/elevation 불일치 | identity DACL을 변경하지 말고 설치 owner의 elevated shell 사용 |
| `CONFIG_CUTOVER_RECOVERY_EVIDENCE_INVALID` | path escape, 누락, digest 또는 Task 상태 불일치 | bundle을 수정하지 말고 원본 protected evidence 복원 |
| `CONFIG_CUTOVER_FORWARD_FIX_REQUIRED` | 첫 v2 write가 이미 발생 | legacy Task를 enable하지 말고 service authority 유지 후 forward-fix |
| `CONFIG_CUTOVER_SCHEDULER_RESTORE_INCOMPLETE` | DB는 legacy로 commit됐고 일부 Task 복원 실패 | 동일한 rollback ID, timestamp, marker, reason으로 같은 명령 재실행 |

`SCHEDULER_RESTORE_INCOMPLETE` 뒤 service가 우연히 시작되어도 persisted authority가 legacy이므로
service claims는 비활성이다. 수동 SQL 변경이나 별도 `schtasks /ENABLE`은 audit와 재개 검증을
우회하므로 사용하지 않는다.

## Update rollback and forward-fix boundary

이 절차는 위의 config cutover clean rollback과 별개다. signed update journal의 현재 단계에
따라 다음 disposition만 허용한다.

| update 단계 | disposition | 이유 |
|---|---|---|
| download, 첫 package verify | 현재 authority 보존 | 실행 binary와 schema를 건드리지 않음 |
| stop claims, drain, control backup, compatibility | current build 재개 | binary 교체 전 |
| stop service, staged verify, replace | previous signed binary 복구 | schema migration 전 |
| migrate schema, start, reconcile, minimum-reader commit, resume | claims를 닫고 forward-fix | 구 reader가 새 schema/write를 해석하지 못할 수 있음 |

`%ProgramData%\DbBackup\updates\update-pending.json`이 남아 있으면 service는
diagnostics/draining 상태로 시작해야 한다. marker를 수동 삭제해 claims를 여는 대신 update
journal과 control backup을 보존하고 compatible build를 사용한다.

minimum reader가 올라간 뒤 old binary를 강제 실행하거나 DB의 minimum-reader row를 낮추는
것은 금지다. old binary는 `SERVICE_MINIMUM_READER_VERSION_UNMET`로 write를 거부하면서
diagnostics만 제공하는 것이 정상이다.
