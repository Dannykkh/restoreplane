# Signed service update runbook

DbBackup update는 실행 중 binary를 바로 덮어쓰지 않는다. claim을 닫고 drain한 뒤
Operational Store online backup을 rehearsal하고, 새 package를 두 번 검증한 후 교체한다.
minimum reader commit과 최종 restart가 끝나기 전에는 새 claim을 열지 않는다.

## 사전 조건

- 현재 service와 `service diagnostics`가 응답한다.
- 유효한 production package와 out-of-band signer thumbprint가 있다.
- out-of-band로 확인한 package manifest SHA-256이 있다.
- out-of-band로 확인한 native tool audit SHA-256이 있다.
- `%ProgramData%\DbBackup\updates`와 control backup volume에 여유 공간이 있다.
- 동일 release revision/tree에 연결된 G0~G5 evidence를 확인했다.
- safety invariant가 0이 아니면 update보다 incident 대응을 먼저 한다.

세 expected 값은 package와 함께 온 unsigned `signing-metadata.json`에서 그대로 신뢰하지 않고,
서명된 release announcement 또는 owner-approved deployment configuration에서 가져온다.

```powershell
$cli = "$env:ProgramFiles\DbBackup\dbbackup.exe"
& $cli service status
& $cli service diagnostics
```

## 정상 update

Treat the release staging root as a prerequisite trust boundary. It must be
created before extraction with a protected exact DACL granting FullControl only
to SYSTEM and the approved installer owner; normalize every extracted child to
that allowlist and reject every reparse point. Verify Authenticode and the
out-of-band signer thumbprint with built-in PowerShell, then invoke `update.ps1`
through `powershell.exe -NoProfile -NonInteractive -ExecutionPolicy AllSigned
-File`. Do not run an update from a staging tree writable by another local
identity.

Update is in-place only for an installation carrying valid protected root
provenance. `DataRoot\install-root-provenance.json` must bind the current
DataRoot, all six control directories, and InstallRoot by volume serial and
stable file ID. A markerless legacy install or a nested custom root is not
updated automatically; migrate it offline first. Every service start path,
including rollback and forward-fix recovery, rechecks provenance together with
the exact package and ACL gate.

```powershell
$releaseRoot = 'C:\staging\DbBackup-release'
$packageRoot = Join-Path $releaseRoot 'package'
$manifestPath = Join-Path $packageRoot 'package-manifest.json'
$expectedSigner = '<out-of-band signer thumbprint>'
$expectedPackageManifest = '<out-of-band package manifest SHA-256>'
$expectedNativeToolAudit = '<out-of-band native tool audit SHA-256>'

# Verify release code with built-in PowerShell before executing artifact code.
$deploymentFiles = @(
    Get-ChildItem (Join-Path $releaseRoot 'scripts') -Filter *.ps1 -File
) + @(
    Get-Item (Join-Path $releaseRoot 'installer\DbBackup.Install.Common.psm1')
)
foreach ($deploymentFile in $deploymentFiles) {
    $signature = Get-AuthenticodeSignature -LiteralPath $deploymentFile.FullName
    if ($signature.Status -ne 'Valid' -or
        $null -eq $signature.SignerCertificate -or
        $signature.SignerCertificate.Thumbprint.Replace(' ', '').ToUpperInvariant() -ne
            $expectedSigner.Replace(' ', '').ToUpperInvariant()) {
        throw 'DEPLOYMENT_SCRIPT_AUTHENTICODE_INVALID'
    }
}

& powershell.exe -NoProfile -NonInteractive -ExecutionPolicy AllSigned `
    -File (Join-Path $releaseRoot 'scripts\update.ps1') `
    -PackageRoot $packageRoot `
    -ManifestPath $manifestPath `
    -ExpectedPackageManifestSha256 $expectedPackageManifest `
    -ExpectedSignerThumbprint $expectedSigner `
    -ExpectedNativeToolAuditSha256 $expectedNativeToolAudit
```

스크립트는 다음 14단계를 journal에 기록한다.

1. `download`
2. `verify_downloaded_artifact`
3. `stop_claims`
4. `drain`
5. `control_backup`
6. `compatibility_check`
7. `stop_service`
8. `verify_staged_artifact`
9. `replace_binary`
10. `migrate_schema`
11. `start_service`
12. `reconcile`
13. `commit_minimum_reader`
14. `resume_claims`

journal은 `%ProgramData%\DbBackup\updates\update-<id>.json`에 있고, rollback binary는 같은
디렉터리의 `rollback-<id>` 아래에 이전 `package-manifest.json`과 함께 보존된다.
rollback package는 이전 manifest의 exact file set/hash/signature로 검증된 뒤에만 사용한다.
target과 rollback manifest는 각각 최초 검증 시 고정한 SHA-256과 설치 직전·시작 직전 다시
비교하므로 검증 뒤 source 변경도 fail-closed한다.
`update-pending.json`이 있는 동안 새 service는
draining으로 시작하고 claims를 열지 않는다.
`replace_binary`는 target manifest에 없는 InstallRoot 파일을 분리된 DataRoot의
`updates\install-tree-quarantine`으로 먼저 옮긴다. target 파일 집합, hash, Authenticode,
native-tool audit와 전체 InstallRoot exact ACL이 다시 통과해야 service를 시작한다. 정상 시작,
current-build 재시작, rollback, schema migration, 최종 resume의 모든 `Start-Service`는 같은
ACL/package gate를 거친다. service 중지 뒤 검증 완료 전에 실패하면
불완전한 rollback tree를 자동 시작하지 않고 pending marker를 남긴 채 수동 복구로 전환한다.

## 완료 확인

```powershell
$status = & $cli service status | ConvertFrom-Json
$diagnostics = & $cli service diagnostics | ConvertFrom-Json

if ($status.success -ne $true -or
    $status.result.updatePending -ne $false -or
    $status.result.claimsEnabled -ne $true -or
    $diagnostics.result.leases.stale -ne 0) {
    throw 'UPDATE_POSTCHECK_FAILED'
}
```

다음도 확인한다.

- 새 version과 minimum reader가 의도한 값이다.
- update journal의 `status=completed`이고 14단계가 모두 `passed`다.
- control backup의 integrity/rehearsal 결과가 `passed`다.
- orphan process, stale allocation, duplicate Recovery Point가 0이다.
- 기존 backup artifact와 spool quarantine이 임의 삭제되지 않았다.

## 단계별 실패 처리

| 실패 단계 | 자동 disposition | 운영자 행동 |
|---|---|---|
| download, 첫 verify | `preserve_current_authority` | package를 격리하고 현재 service 유지 |
| stop claims부터 compatibility까지 | `resume_current_build` | current build 재시작 후 원인 수정 |
| stop service부터 replace까지 | `restore_previous_binary` | 자동 복구 결과와 서명을 확인 |
| schema migration부터 resume까지 | `forward_fix_claims_stopped` | claims를 닫은 채 compatible 새 build로 forward-fix |

schema migration 뒤에는 구 binary를 강제 실행하지 않는다. minimum reader보다 낮은 binary는
write mode를 열지 않고 diagnostics-only로 시작해야 한다. `update-pending.json`,
minimum-reader row, cutover marker를 수동 편집하거나 삭제하지 않는다.

## Fault rehearsal

비운영 fixture에서만 `-InjectFailure <stage>`를 사용한다. 각 stage와 recovery disposition은
`crates/dbbackup-service/tests/update_lifecycle.rs`와
`scripts/tests/install-lifecycle.Tests.ps1`의 전체 행렬로 검증한다. 운영 서버에서 fault
injection을 사용하지 않는다.

## 중단 또는 escalation 조건

- signature/hash 불일치
- control backup integrity/rehearsal 실패
- drain timeout 또는 stale lease
- schema migration 실패
- minimum reader mismatch
- duplicate Recovery Point, stale fence commit, secret canary 검출

이 경우 현재 cohort를 중단하고 `incident.md` 절차로 전환한다. cutover clean epoch일 때만
`rollback.md`의 clean rollback을 고려하고, 그 이후 write가 있으면 forward-fix한다.
