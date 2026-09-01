# Windows install, repair, and uninstall runbook

이 문서는 서명된 DbBackup Windows release를 설치하고 검증하는 절차다. 개발 channel
manifest는 개발·fixture에서만 사용한다. production 설치는 package hash와 유효한
Authenticode 서명, 예상 signer thumbprint가 모두 일치해야 한다.

## 준비물과 사전 조건

- 관리자 권한 PowerShell
- release의 `package/`, `scripts/`, `installer/` 디렉터리
- release와 별도 경로로 전달받아 확인한 signer certificate thumbprint
- release와 별도 경로로 전달받아 확인한 `package-manifest.json` SHA-256
- release와 별도 신뢰 경로로 전달받아 확인한 `native-tools-audit.json` SHA-256
- `InstallRoot`와 `DataRoot`는 root directory 자체가 아닌 절대 경로여야 한다. 두 경로는
  서로 달라야 하고 어느 한쪽도 다른 쪽의 하위 경로여서는 안 된다.
- 기존 `DbBackupService` 서비스가 없어야 한다. 있으면 `repair.ps1`을 사용한다.
- 기존 `DataRoot\service-identity.json`을 재사용하는 reinstall/repair는 그 파일에 기록된
  persisted owner 계정 또는 SYSTEM 문맥에서 실행한다. 다른 관리자 계정으로 owner를 바꾸는
  작업은 별도 승인된 identity migration이며 installer가 암묵적으로 수행하지 않는다.
- SYSTEM 문맥의 최초 install은 SYSTEM과 구분되는 승인된 설치 owner SID를 `-OwnerSid`로
  명시한다. owner와 SYSTEM을 같은 SID로 축약한 ACL은 허용하지 않는다.

release의 `signing-metadata.json`과 `checksums.sha256`은 동일 artifact인지 확인하는 자료다.
둘 다 package와 함께 전달되는 unsigned metadata이므로 signer thumbprint, package manifest
SHA-256, native tool audit SHA-256의 trust source가 아니다. 세 expected 값은 서명된 release announcement 또는
owner-approved deployment configuration 같은 별도 신뢰 채널에서 취득하고, package metadata는
그 값과 비교하는 대상으로만 사용한다. 설치 스크립트는 그 외부 기준으로 package manifest의
SHA-256, native tool audit digest, Windows trust chain을 확인한다.

### Production release assembly prerequisite

관리형 Windows release runner에는 운영자가 별도로 취득·감사한 native tool tree를 읽기 전용
경로로 준비하고, production-signing environment 변수 `AUDITED_NATIVE_TOOL_ROOT`와
`AUDITED_NATIVE_TOOL_AUDIT_SHA256`을 설정해야 한다. tree는
`release/native-tool-audit.schema.json` 계약에 따라 PG18, MySQL8, MariaDB11의 필수 실행 파일,
runtime dependency, license를 모두 열거한다. release workflow는 vendor URL을 추측하거나 최신
버전을 다운로드하지 않으며 이 입력이 없거나 일부만 있으면 package 생성 전에 실패한다.
운영자는 PE dependency closure와 재배포 license를 audit 전에 확인해야 한다. workflow는
audit-pinned 서명/hash를 검증한 뒤 필수 client의 `--version`을 실행해 실제 major와 dependency
load 가능성을 다시 확인하지만, 누락된 dependency를 인터넷에서 찾아 보완하지 않는다.

## Clean install

The release staging root is an input trust boundary, not something an already
running package script can secure retroactively. Create the staging directory
with a protected exact DACL before extraction: only SYSTEM and the approved
installer owner receive FullControl, every extracted child is normalized to the
same protected allowlist, and reparse points are forbidden. Keep the built-in
Authenticode/thumbprint check below, then invoke each deployment script in a new
`powershell.exe -NoProfile -NonInteractive -ExecutionPolicy AllSigned -File`
process. A staging root writable by any other local identity is not supported.

`InstallRoot` and `DataRoot` must each be a direct child of their default
Program Files/ProgramData parent, or a direct child of a volume root. Nested
custom parents are not accepted because a current ACL cannot prove that no
ordinary user retained an older WRITE_DAC/WRITE_OWNER handle. Reusing an
existing DataRoot requires a valid protected `install-root-provenance.json` and
an externally approved owner SID passed as `-OwnerSid`. Markerless legacy roots
require an offline migration; the installer does not mint provenance
retroactively.

release root에서 다음을 실행한다.

```powershell
$releaseRoot = 'C:\staging\DbBackup-release'
$packageRoot = Join-Path $releaseRoot 'package'
$manifestPath = Join-Path $packageRoot 'package-manifest.json'
$expectedSigner = '<out-of-band signer thumbprint>'
$expectedPackageManifest = '<out-of-band package manifest SHA-256>'
$expectedNativeToolAudit = '<out-of-band native tool audit SHA-256>'
$ownerSid = '<approved non-SYSTEM installer owner SID>'
$installRoot = "$env:ProgramFiles\DbBackup"
$dataRoot = "$env:ProgramData\DbBackup"

# Do not execute release code until this built-in verification passes.
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
    -File (Join-Path $releaseRoot 'scripts\install.ps1') `
    -PackageRoot $packageRoot `
    -ManifestPath $manifestPath `
    -InstallRoot $installRoot `
    -DataRoot $dataRoot `
    -OwnerSid $ownerSid `
    -ExpectedPackageManifestSha256 $expectedPackageManifest `
    -ExpectedSignerThumbprint $expectedSigner `
    -ExpectedNativeToolAuditSha256 $expectedNativeToolAudit
```

설치기는 다음 경계를 검사하고 구성한다.

- manifest와 모든 설치 파일의 SHA-256
- production executable의 Authenticode status, signer thumbprint, publisher
- `tools/pg/pg18`, `tools/mysql/mysql8`, `tools/mysql/mariadb11` 전체 tree가 외부에서
  고정한 audit digest 및 파일별 SHA-256과 일치하는지 확인
- vendor entry-point EXE의 audit-pinned Authenticode signer/publisher 확인. OSS dependency DLL은
  audit가 서명을 요구하면 Authenticode도 확인하고, 그렇지 않으면 외부 audit digest와 파일별
  SHA-256을 신뢰 기준으로 사용
- 모든 관리자 실행 PS1과 `DbBackup.Install.Common.psm1`을 artifact 외부에서 받은 signer
  thumbprint로 검증한 뒤에만 실행
- manifest에 없는 InstallRoot 파일은 service 시작 전에
  `DataRoot\updates\install-tree-quarantine\<timestamp-guid>`로 이동하고, 복사 후 전체 설치
  파일 집합과 hash/AuthentiCode/native-tool audit를 다시 검증. reparse point는 이동하지 않고 차단
- 최초 package 검증 때 고정한 SHA-256으로 `package-manifest.json` source를 다시 확인하고
  InstallRoot에 atomic copy한 뒤 destination도 같은 digest인지 비교. 검증 후 source 변경,
  installed manifest missing/tamper는 service 시작 전에 차단
- `NT SERVICE\DbBackupService` virtual account와 unrestricted service SID
- `%ProgramFiles%\DbBackup` 전체 tree의 protected exact allowlist ACL: SYSTEM/owner FullControl,
  service SID ReadAndExecute. 추가 allow/deny/inherited ACE는 허용하지 않음
- `%ProgramData%\DbBackup`의 service/owner 전용 ACL
- Operational Store, spool, control backup, support bundle, telemetry archive, update 디렉터리
- 자동 시작과 service failure recovery
- 새 Store에서만 사용할 서명 package/binary digest 기반 standalone authority bootstrap 증거

두 root의 책임은 다음과 같다.

| Root | 내용 | lifecycle |
|---|---|---|
| `InstallRoot` | `dbbackup-service.exe`, CLI, bundled native tools | signed install/update/repair가 배치 |
| `DataRoot` | Operational Store, service identity, secrets, spool, control backup, support bundle, telemetry archive, update journal | repair/uninstall/reinstall에서 보존 |

service의 `binPath`는 `InstallRoot\dbbackup-service.exe --data-root "<DataRoot>"` 형태다.
service는 자신의 executable parent를 `InstallRoot`로 사용하며 root가 같거나 중첩되거나
`--data-root`가 모호하면 시작하지 않는다. custom root를 사용한 설치는 이후 repair, update,
uninstall에도 같은 두 값을 명시해야 한다.

Install and repair never reuse an existing InstallRoot object, even if its ACL
currently looks exact. The old object is renamed by its verified handle to a
random same-volume sibling quarantine path, and a fresh protected InstallRoot
is created before package copy. InstallRoot and DataRoot may therefore reside on
different volumes.

Legacy `dbbackup run`, `schedule` 등의 direct execution guard는 `DbBackupService` 항목이
SCM에 존재하면 service 상태나 configured `DataRoot`와 관계없이 실행을 차단한다. 지원되는
uninstall 흐름으로 service 항목을 제거하고 SCM이 service 미설치(Windows error 1060)를 명확히
반환할 때만 기존 `%ProgramData%\DbBackup` legacy 경로를 허용한다. SCM 조회 실패도 두 execution
authority가 동시에 동작하지 않도록 fail-closed한다.

설치기는 Plan별 Windows Task Scheduler 항목을 만들지 않는다. `DbBackupService`는 UI가
닫힌 뒤에도 실행되는 singleton lifecycle host이며, 일정과 실행 cursor는 Operational Store에
있다. 설치 대상에 기존 Store나 legacy/shadow 흔적이 있으면 standalone bootstrap을 거부하고
기존 cutover 절차를 요구한다.

## 설치 후 검증

```powershell
sc.exe query DbBackupService
sc.exe qc DbBackupService
sc.exe showsid DbBackupService
icacls.exe $installRoot
icacls.exe $dataRoot

& (Join-Path $installRoot 'dbbackup.exe') service status
& (Join-Path $installRoot 'dbbackup.exe') service diagnostics
```

통과 조건은 service가 `Running`이고, status가 성공이며, update pending이 없고,
diagnostics가 read-only로 반환되는 것이다. clean install의 새 Store는
`phase=production_ready`, `executionAuthority=service`, `claimsEnabled=true`여야 한다.
기존 Store는 설치 성공만으로 legacy scheduler에서 service authority로 전환하지 않으며,
`release/gates/g0-claims.json`과 cutover evidence를 확인한다.

## Repair

binary hash/signature, service definition, service SID 또는 ACL drift가 의심되면 같은 버전
이상의 서명된 package로 repair한다. Operational Store와 artifact는 보존된다.

```powershell
& powershell.exe -NoProfile -NonInteractive -ExecutionPolicy AllSigned `
    -File (Join-Path $releaseRoot 'scripts\repair.ps1') `
    -PackageRoot $packageRoot `
    -ManifestPath $manifestPath `
    -InstallRoot $installRoot `
    -DataRoot $dataRoot `
    -OwnerSid $ownerSid `
    -ExpectedPackageManifestSha256 $expectedPackageManifest `
    -ExpectedSignerThumbprint $expectedSigner `
    -ExpectedNativeToolAuditSha256 $expectedNativeToolAudit
```

repair 뒤 `service status`, `service diagnostics`, `sc.exe showsid`, 두 root의 ACL을 다시
검사한다. ACL을 수동으로 넓히거나 LocalSystem으로 service identity를 바꾸지 않는다.
Everyone, Authenticated Users, BUILTIN\Users, Administrators 또는 별도 운영자 SID를
InstallRoot DACL에 추가하면 package bootstrap이 fail-closed한다. 유지보수가 필요하면 ACL을
수동 완화하지 말고 persisted owner 또는 SYSTEM으로 서명된 repair를 다시 실행한다.

### 프로그램 파일만 유실된 경우

`DataRoot`와 service 등록은 남아 있지만 `InstallRoot`의 binary나 bundled tool이 삭제됐다면
새 backup을 시작하지 말고 동일하거나 더 새 서명 package로 위 repair를 실행한다. repair는
service definition의 quoted `--data-root`, service SID, ACL을 복구하고 기존 Operational Store,
spool, artifact, control backup을 보존한다. `DataRoot` 자체가 유실된 경우는 repair가 아니라
검증된 control backup과 recovery material을 사용하는 별도 재해 복구 대상이다.

## Uninstall

```powershell
& (Join-Path $releaseRoot 'scripts\uninstall.ps1') `
    -InstallRoot $installRoot `
    -DataRoot $dataRoot `
    -ExpectedSignerThumbprint $expectedSigner
```

uninstall은 새 claim을 중단하고 최대 180초 동안 정상 drain을 기다린 뒤 service 등록만
제거한다. 제한 시간 안에 정지하지 않으면 삭제를 계속하지 않고 실패한다. 다음 데이터는
의도적으로 삭제하지 않는다.

- `InstallRoot`
- `DataRoot\operational-v2.db`
- `DataRoot` 아래 `secrets`, `control-backups`, `support-bundles`, `telemetry-archives`, `spool`
- backup artifact와 operator recovery 자료

완전 삭제는 이 release에서 자동화하지 않는다. 보존 기간과 복구 필요성을 별도로 확인한
뒤 승인된 데이터 폐기 절차를 사용한다.

나중에 service를 다시 등록할 때는 `repair.ps1`이 아니라 동일하거나 더 새 서명 package의
`install.ps1`을 같은 `InstallRoot`와 기존 `DataRoot`로 실행한다. `repair.ps1`은 기존 service
entry가 있을 때만 허용된다. 기존 Store를 재사용하는 install은 pristine standalone authority를
새로 bootstrap하지 않으므로 시작 후 기존 execution authority와 claims 상태를 반드시 확인한다.

## 실패 코드

| 코드 | 의미 | 조치 |
|---|---|---|
| `PACKAGE_HASH_MISMATCH` | package file이 manifest와 다름 | 설치 중단, 원 release를 다시 취득 |
| `PACKAGE_AUTHENTICODE_INVALID` | 서명, trust chain 또는 signer 불일치 | 설치 중단, signing owner에게 이관 |
| `PACKAGE_SIGNER_REQUIRED` | production signer 기준 미제공 | out-of-band thumbprint를 확인 |
| `PACKAGE_MANIFEST_DIGEST_REQUIRED` | production manifest의 외부 SHA-256 기준 누락 | signed release announcement의 digest 확인 |
| `PACKAGE_MANIFEST_DIGEST_MISMATCH` | manifest가 외부 기준과 다르거나 검증 중 변경됨 | package 격리 후 신뢰 채널에서 다시 취득 |
| `PACKAGE_NATIVE_TOOL_AUDIT_DIGEST_REQUIRED` | native tool audit 기준 미제공 | out-of-band audit SHA-256을 확인 |
| `PACKAGE_NATIVE_TOOL_AUDIT_DIGEST_MISMATCH` | package의 native tool audit가 승인본과 다름 | 설치 중단, artifact 격리 |
| `INSTALL_SERVICE_ALREADY_EXISTS_USE_REPAIR` | 기존 service 존재 | clean install 대신 repair 사용 |
| `INSTALL_ADMINISTRATOR_REQUIRED` | 권한 부족 | 승인된 관리자 shell에서 재실행 |
| `INSTALL_PATH_ESCAPE` | package 또는 설치 경로 escape | 경로를 수정하지 말고 artifact 격리 |
| `DEPLOYMENT_SCRIPT_SIGNER_REQUIRED` | 관리자 실행 스크립트의 외부 signer 기준 누락 | release 밖 승인 채널에서 thumbprint 확인 |
| `DEPLOYMENT_SCRIPT_AUTHENTICODE_INVALID` | PS1/PSM1 서명, trust chain 또는 signer 불일치 | 실행 중단, release artifact 격리 |
| `INSTALL_TREE_REPARSE_POINT_BLOCKED` | InstallRoot 안 reparse point 발견 | service를 시작하지 말고 경로 조사 |
| `INSTALL_ACL_INVALID` | InstallRoot/DataRoot/identity ACL이 exact allowlist와 다름 | service를 시작하지 말고 signed repair로 ACL 복구 |
| `INSTALL_OWNER_CONTEXT_REQUIRED` | persisted owner/SYSTEM이 아닌 계정에서 유지보수 시도 | owner 또는 SYSTEM 문맥에서 재실행 |
| `INSTALL_OWNER_SID_REQUIRED` | SYSTEM 최초 설치에서 별도 owner SID 누락 | 승인된 비-SYSTEM owner SID를 명시 |
| `INSTALL_OWNER_SID_MISMATCH` | 요청 owner가 persisted owner와 다름 | owner migration 승인 없이는 변경하지 않음 |
| `INSTALL_EXISTING_DATA_ROOT_OWNER_SID_REQUIRED` | existing DataRoot에 외부 owner pin이 없음 | 승인된 owner SID를 별도 신뢰 채널에서 받아 명시 |
| `INSTALL_ROOT_PROVENANCE_INVALID` | root provenance가 없거나 다른 filesystem object를 가리킴 | 시작하지 말고 승인된 offline migration 또는 원 root 복구 |
| `INSTALL_CUSTOM_ROOT_PARENT_UNTRUSTED` | pre-existing nested custom parent 아래 root | 기본 parent 또는 volume root 직계 경로 사용 |
| `PACKAGE_INSTALLED_MANIFEST_INVALID` | 설치 manifest missing 또는 staged manifest와 hash 불일치 | service를 시작하지 말고 package 격리 후 repair |

## 증거 보존

install/repair/uninstall의 JSON 결과, service SID, ACL 출력, status/diagnostics 결과의 hash,
release revision/build ID를 incident 또는 rollout 기록에 연결한다. 비밀번호, connection
string, process environment, raw dump 내용은 증거에 넣지 않는다.
