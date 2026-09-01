[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "dbbackup-install-contract-" + [Guid]::NewGuid().ToString("N")
)

function Write-FixtureManifest {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $PackageRoot,
        [ValidateSet("development", "production")] [string] $Channel = "development",
        [bool] $AuthenticodeRequired = $false
    )

    $files = @("dbbackup-service.exe", "dbbackup.exe") | ForEach-Object {
        [ordered] @{
            path = $_
            sha256 = (
                Get-FileHash -LiteralPath (Join-Path $PackageRoot $_) -Algorithm SHA256
            ).Hash.ToLowerInvariant()
            sizeBytes = [long] (
                Get-Item -LiteralPath (Join-Path $PackageRoot $_) -Force
            ).Length
            authenticodeRequired = $AuthenticodeRequired
            signerRole = if ($AuthenticodeRequired) { "release" } else { "hash_only" }
            signerThumbprint = ""
            publisher = "DbBackup"
        }
    }
    $manifest = [ordered] @{
        schemaVersion = 1
        channel = $Channel
        version = "0.1.0"
        minimumReaderVersion = "0.1.0"
        buildId = ("a" * 64)
        revision = ("b" * 40)
        sourceTreeDigest = ("c" * 64)
        files = @($files)
    }
    [IO.File]::WriteAllText(
        $Path,
        ($manifest | ConvertTo-Json -Depth 16),
        [Text.UTF8Encoding]::new($false)
    )
}

try {
    $packageRoot = Join-Path $fixtureRoot "package"
    $installRoot = Join-Path $fixtureRoot "install"
    $dataRoot = Join-Path $fixtureRoot "data"
    New-Item -ItemType Directory -Path $packageRoot -Force | Out-Null
    [IO.File]::WriteAllText(
        (Join-Path $packageRoot "dbbackup-service.exe"),
        "service-fixture",
        [Text.UTF8Encoding]::new($false)
    )
    [IO.File]::WriteAllText(
        (Join-Path $packageRoot "dbbackup.exe"),
        "cli-fixture",
        [Text.UTF8Encoding]::new($false)
    )
    $manifestPath = Join-Path $packageRoot "package-manifest.json"
    Write-FixtureManifest -Path $manifestPath -PackageRoot $packageRoot
    $manifestSha256 = (
        Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256
    ).Hash

    $common = @{
        PackageRoot = $packageRoot
        ManifestPath = $manifestPath
        InstallRoot = $installRoot
        DataRoot = $dataRoot
        ExpectedPackageManifestSha256 = $manifestSha256
        AllowUnsignedDevelopmentPackage = $true
        ValidateOnly = $true
    }
    $developmentFixtureWithoutDigest = $common.Clone()
    $null = $developmentFixtureWithoutDigest.Remove(
        "ExpectedPackageManifestSha256"
    )
    $developmentFixtureContract = (& (
            Join-Path $repoRoot "scripts\install.ps1"
        ) @developmentFixtureWithoutDigest) | ConvertFrom-Json
    if ($developmentFixtureContract.operation -ne "clean_install") {
        throw "ValidateOnly development fixture digest exception is invalid"
    }
    $install = (& (Join-Path $repoRoot "scripts\install.ps1") @common) |
        ConvertFrom-Json
    $repair = (& (Join-Path $repoRoot "scripts\repair.ps1") @common) |
        ConvertFrom-Json
    $update = (& (Join-Path $repoRoot "scripts\update.ps1") @common) |
        ConvertFrom-Json
    $uninstall = (& (Join-Path $repoRoot "scripts\uninstall.ps1") `
        -InstallRoot $installRoot -DataRoot $dataRoot -ValidateOnly) |
        ConvertFrom-Json

    $overlapRoot = Join-Path $fixtureRoot "overlap"
    foreach ($scriptName in @("install.ps1", "repair.ps1", "update.ps1")) {
        $overlapBlocked = $false
        try {
            & (Join-Path $repoRoot "scripts\$scriptName") `
                -PackageRoot $packageRoot `
                -ManifestPath $manifestPath `
                -InstallRoot $overlapRoot `
                -DataRoot $overlapRoot `
                -AllowUnsignedDevelopmentPackage `
                -ValidateOnly | Out-Null
        }
        catch {
            $overlapBlocked = $_.Exception.Message -match "INSTALL_DATA_ROOT_OVERLAP"
        }
        if (-not $overlapBlocked) {
            throw "$scriptName did not reject overlapping install/data roots"
        }
    }
    $uninstallOverlapBlocked = $false
    try {
        & (Join-Path $repoRoot "scripts\uninstall.ps1") `
            -InstallRoot $overlapRoot `
            -DataRoot $overlapRoot `
            -ValidateOnly | Out-Null
    }
    catch {
        $uninstallOverlapBlocked = $_.Exception.Message -match (
            "INSTALL_DATA_ROOT_OVERLAP"
        )
    }
    if (-not $uninstallOverlapBlocked) {
        throw "uninstall.ps1 did not reject overlapping install/data roots"
    }

    if ($install.operation -ne "clean_install" -or
        $install.authorityBootstrap -ne "pristine_standalone_only" -or
        $repair.operation -ne "repair" -or
        $update.operation -ne "update" -or
        $uninstall.operation -ne "uninstall") {
        throw "installer lifecycle contract identity is invalid"
    }
    foreach ($contract in @($install, $repair, $update, $uninstall)) {
        if ($contract.serviceName -ne "DbBackupService") {
            throw "installer lifecycle service name is inconsistent"
        }
    }
    $unsignedDeploymentBlocked = $false
    try {
        & (Join-Path $repoRoot "scripts\uninstall.ps1") `
            -InstallRoot $installRoot `
            -DataRoot $dataRoot `
            -ExpectedSignerThumbprint ("a" * 40) | Out-Null
    }
    catch {
        $unsignedDeploymentBlocked = $_.Exception.Message -match (
            "DEPLOYMENT_SCRIPT_AUTHENTICODE_INVALID"
        )
    }
    if (-not $unsignedDeploymentBlocked) {
        throw "unsigned deployment entry point was not blocked before mutation"
    }
    foreach ($contract in @($install, $repair, $update)) {
        if ($contract.installRoot -ne $installRoot -or
            $contract.dataRoot -ne $dataRoot -or
            (@($contract.serviceArguments) -join "|") -ne (
                @("--data-root", $dataRoot) -join "|"
            )) {
            throw "service lifecycle must preserve distinct install/data roots"
        }
    }
    $expectedStages = @(
        "download",
        "verify_downloaded_artifact",
        "stop_claims",
        "drain",
        "control_backup",
        "compatibility_check",
        "stop_service",
        "verify_staged_artifact",
        "replace_binary",
        "migrate_schema",
        "start_service",
        "reconcile",
        "commit_minimum_reader",
        "resume_claims"
    )
    if ((@($update.stages) -join ",") -ne ($expectedStages -join ",") -or
        $update.doubleSignatureVerification -ne $true -or
        $update.updatePendingClaimGuard -ne $true -or
        $update.minimumReaderCommitBeforeResume -ne $true) {
        throw "update lifecycle ordering or fail-closed contract is invalid"
    }
    if ($update.recoveryByStage.replace_binary -ne "restore_previous_binary" -or
        $update.recoveryByStage.migrate_schema -ne "forward_fix_claims_stopped") {
        throw "update rollback/forward-fix boundary is invalid"
    }
    foreach ($contract in @($install, $repair, $update)) {
        if ($contract.deploymentScriptAuthenticodeRequired -ne $true -or
            $contract.exactInstallTreeRequired -ne $true -or
            $contract.unmanifestedFileDisposition -ne "quarantine_before_start") {
            throw "installer lifecycle must authenticate scripts and enforce an exact install tree"
        }
    }
    $updateText = Get-Content -LiteralPath (
        Join-Path $repoRoot "scripts\update.ps1"
    ) -Raw -Encoding UTF8
    $stopStageIndex = $updateText.IndexOf(
        'Invoke-UpdateStage -Name "stop_service"',
        [StringComparison]::Ordinal
    )
    $stopWaitIndex = if ($stopStageIndex -ge 0) {
        $updateText.IndexOf(
            'Wait-DbBackupService -ExpectedStatus "Stopped"',
            $stopStageIndex,
            [StringComparison]::Ordinal
        )
    }
    else {
        -1
    }
    $serviceConfigIndex = $updateText.IndexOf(
        '    "config",',
        [StringComparison]::Ordinal
    )
    if ($stopStageIndex -lt 0 -or $stopWaitIndex -lt $stopStageIndex -or
        $serviceConfigIndex -lt $stopWaitIndex) {
        throw "update must stop the running service before changing its ImagePath"
    }
    $verifiedStartDefinition = $updateText.IndexOf(
        "function Start-DbBackupVerifiedService",
        [StringComparison]::Ordinal
    )
    $verifiedStartCall = $updateText.IndexOf(
        "Start-DbBackupVerifiedService",
        $verifiedStartDefinition + 1,
        [StringComparison]::Ordinal
    )
    $rawStartCount = [regex]::Matches(
        $updateText,
        "(?m)^\s*Start-Service\b"
    ).Count
    $verifiedStartCallCount = [regex]::Matches(
        $updateText,
        "(?m)^\s*Start-DbBackupVerifiedService\b"
    ).Count
    if ($verifiedStartDefinition -lt 0 -or
        $verifiedStartCall -le $verifiedStartDefinition -or
        $rawStartCount -ne 1 -or
        $verifiedStartCallCount -ne 5 -or
        -not $updateText.Contains("Assert-DbBackupPackageManifest") -or
        -not $updateText.Contains("Assert-DbBackupProtectedAcl") -or
        -not $updateText.Contains("Copy-DbBackupPackageManifest") -or
        -not $updateText.Contains("rollbackManifestPath")) {
        throw "every update start and rollback must pass the ACL/package manifest gate"
    }
    if ($uninstall.destructivePurgeSupported -ne $false -or
        @($uninstall.dataPathsPreserved).Count -lt 5) {
        throw "uninstall must preserve operator data and artifacts"
    }
    if ($uninstall.recovery.afterSuccessfulUninstall -notmatch "install\.ps1" -or
        $uninstall.recovery.afterSuccessfulUninstall -notmatch [regex]::Escape($dataRoot) -or
        $uninstall.recovery.serviceEntryStillRegistered -notmatch "repair\.ps1") {
        throw "uninstall recovery guidance must distinguish install from repair"
    }

    Add-Content -LiteralPath (Join-Path $packageRoot "dbbackup.exe") -Value "tampered"
    $tamperBlocked = $false
    try {
        & (Join-Path $repoRoot "scripts\install.ps1") @common | Out-Null
    }
    catch {
        $tamperBlocked = $_.Exception.Message -match "PACKAGE_(?:SIZE|HASH)_MISMATCH"
    }
    if (-not $tamperBlocked) {
        throw "package hash tampering was not blocked"
    }

    [IO.File]::WriteAllText(
        (Join-Path $packageRoot "dbbackup.exe"),
        "cli-fixture",
        [Text.UTF8Encoding]::new($false)
    )
    Write-FixtureManifest -Path $manifestPath -PackageRoot $packageRoot `
        -Channel "production" -AuthenticodeRequired $false
    $productionManifestSha256 = (
        Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256
    ).Hash
    $missingDigestContract = $common.Clone()
    $null = $missingDigestContract.Remove("ExpectedPackageManifestSha256")
    if ($missingDigestContract.ContainsKey("ExpectedPackageManifestSha256")) {
        throw "missing manifest digest fixture retained the digest parameter"
    }
    $mismatchedDigestContract = $common.Clone()
    $mismatchedDigestContract.ExpectedPackageManifestSha256 = "f" * 64
    foreach ($scriptName in @("install.ps1", "repair.ps1", "update.ps1")) {
        $missingManifestDigestBlocked = $false
        try {
            & (Join-Path $repoRoot "scripts\$scriptName") `
                @missingDigestContract | Out-Null
        }
        catch {
            $missingManifestDigestBlocked = $_.Exception.Message -match (
                "PACKAGE_MANIFEST_DIGEST_REQUIRED"
            )
        }
        if (-not $missingManifestDigestBlocked) {
            throw "$scriptName accepted a missing external manifest digest"
        }
        $mismatchedManifestDigestBlocked = $false
        try {
            & (Join-Path $repoRoot "scripts\$scriptName") `
                @mismatchedDigestContract | Out-Null
        }
        catch {
            $mismatchedManifestDigestBlocked = $_.Exception.Message -match (
                "PACKAGE_MANIFEST_DIGEST_MISMATCH"
            )
        }
        if (-not $mismatchedManifestDigestBlocked) {
            throw "$scriptName accepted a mismatched external manifest digest"
        }
    }
    $common.ExpectedPackageManifestSha256 = $productionManifestSha256
    $unsignedProductionBlocked = $false
    try {
        & (Join-Path $repoRoot "scripts\install.ps1") @common | Out-Null
    }
    catch {
        $unsignedProductionBlocked = $_.Exception.Message -match (
            "PACKAGE_SIGNATURE_POLICY_INVALID"
        )
    }
    if (-not $unsignedProductionBlocked) {
        throw "unsigned production executables were not blocked"
    }

    $moduleText = Get-Content -LiteralPath (
        Join-Path $repoRoot "installer\DbBackup.Install.Common.psm1"
    ) -Raw -Encoding UTF8
    foreach ($required in @(
            "Get-AuthenticodeSignature",
            "Get-FileHash",
            "sc.exe",
            "SetAccessRuleProtection",
            "ReparsePoint",
            "[IO.File]::Replace",
            "Flush(`$true)"
        )) {
        if (-not $moduleText.Contains($required)) {
            throw "installer common module is missing $required"
        }
    }

    Import-Module (
        Join-Path $repoRoot "installer\DbBackup.Install.Common.psm1"
    ) -Force

    $lockFixtureRoot = Join-Path $fixtureRoot "install-tree-lock"
    $lockFixtureNested = Join-Path $lockFixtureRoot "nested"
    New-Item -ItemType Directory -Path $lockFixtureNested -Force | Out-Null
    $overwriteTarget = Join-Path $lockFixtureNested "overwrite.bin"
    $deleteTarget = Join-Path $lockFixtureNested "delete.bin"
    $replaceTarget = Join-Path $lockFixtureNested "replace.bin"
    foreach ($path in @($overwriteTarget, $deleteTarget, $replaceTarget)) {
        [IO.File]::WriteAllText(
            $path,
            "trusted fixture",
            [Text.UTF8Encoding]::new($false)
        )
    }

    $preopenedWriter = [IO.File]::Open(
        $overwriteTarget,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
    )
    try {
        $preopenedWriterBlocked = $false
        try {
            $unexpectedLock = Enter-DbBackupInstallTreeLock `
                -InstallRoot $lockFixtureRoot
            $unexpectedLock.Dispose()
        }
        catch {
            $preopenedWriterBlocked = $_.Exception.Message -match (
                "INSTALL_TREE_LOCK_FAILED"
            )
        }
        if (-not $preopenedWriterBlocked) {
            throw "install tree lock accepted a pre-open write handle"
        }
    }
    finally {
        $preopenedWriter.Dispose()
    }

    $replaceSource = Join-Path $fixtureRoot "replacement-source.bin"
    $replaceBackup = Join-Path $fixtureRoot "replacement-backup.bin"
    [IO.File]::WriteAllText(
        $replaceSource,
        "replacement fixture",
        [Text.UTF8Encoding]::new($false)
    )
    $treeLock = Enter-DbBackupInstallTreeLock -InstallRoot $lockFixtureRoot
    try {
        if ($treeLock.LockedPathCount -lt 5) {
            throw "install tree lock did not retain the full tree"
        }
        if ([string]::IsNullOrWhiteSpace(
                (Get-FileHash -LiteralPath $overwriteTarget -Algorithm SHA256).Hash
            )) {
            throw "install tree lock blocked trusted read verification"
        }
        $overwriteBlocked = $false
        try {
            [IO.File]::WriteAllText($overwriteTarget, "tampered")
        }
        catch {
            $overwriteBlocked = $true
        }
        $deleteBlocked = $false
        try {
            Remove-Item -LiteralPath $deleteTarget -Force
        }
        catch {
            $deleteBlocked = $true
        }
        $replaceBlocked = $false
        try {
            [IO.File]::Replace(
                $replaceSource,
                $replaceTarget,
                $replaceBackup,
                $true
            )
        }
        catch {
            $replaceBlocked = $true
        }
        if (-not $overwriteBlocked -or -not $deleteBlocked -or
            -not $replaceBlocked) {
            throw "install tree lock allowed overwrite, delete, or replacement"
        }
    }
    finally {
        $treeLock.Dispose()
    }
    [IO.File]::WriteAllText($overwriteTarget, "after dispose")
    Remove-Item -LiteralPath $deleteTarget -Force
    if (-not (Test-Path -LiteralPath $replaceSource -PathType Leaf)) {
        [IO.File]::WriteAllText(
            $replaceSource,
            "replacement fixture",
            [Text.UTF8Encoding]::new($false)
        )
    }
    [IO.File]::Replace(
        $replaceSource,
        $replaceTarget,
        $replaceBackup,
        $true
    )
    if ([IO.File]::ReadAllText($overwriteTarget) -ne "after dispose" -or
        (Test-Path -LiteralPath $deleteTarget) -or
        [IO.File]::ReadAllText($replaceTarget) -ne "replacement fixture") {
        throw "disposed install tree lock retained mutation restrictions"
    }

    $pathLockTarget = Join-Path $fixtureRoot "path-mutation-lock"
    $renamedPathLockTarget = Join-Path $fixtureRoot "path-mutation-lock-renamed"
    New-Item -ItemType Directory -Path $pathLockTarget -Force | Out-Null
    $pathLock = Enter-DbBackupPathMutationLock -Path $pathLockTarget
    try {
        $pathRenameBlocked = $false
        try {
            [IO.Directory]::Move($pathLockTarget, $renamedPathLockTarget)
        }
        catch {
            $pathRenameBlocked = $true
        }
        if (-not $pathRenameBlocked) {
            throw "path mutation lock allowed target replacement"
        }
    }
    finally {
        $pathLock.Dispose()
    }
    [IO.Directory]::Move($pathLockTarget, $renamedPathLockTarget)

    $lockReparseTarget = Join-Path $fixtureRoot "lock-reparse-target"
    $lockReparseTree = Join-Path $fixtureRoot "lock-reparse-tree"
    New-Item -ItemType Directory -Path $lockReparseTarget -Force | Out-Null
    New-Item -ItemType Directory -Path $lockReparseTree -Force | Out-Null
    New-Item -ItemType Junction `
        -Path (Join-Path $lockReparseTree "junction") `
        -Target $lockReparseTarget | Out-Null
    $lockReparseBlocked = $false
    try {
        $unexpectedLock = Enter-DbBackupInstallTreeLock `
            -InstallRoot $lockReparseTree
        $unexpectedLock.Dispose()
    }
    catch {
        $lockReparseBlocked = $_.Exception.Message -match "INSTALL_TREE_LOCK_FAILED"
    }
    if (-not $lockReparseBlocked) {
        throw "install tree lock accepted a reparse point"
    }

    $lockRealAncestor = Join-Path $fixtureRoot "lock-real-ancestor"
    $lockAncestorJunction = Join-Path $fixtureRoot "lock-ancestor-junction"
    New-Item -ItemType Directory `
        -Path (Join-Path $lockRealAncestor "install") `
        -Force | Out-Null
    New-Item -ItemType Junction `
        -Path $lockAncestorJunction `
        -Target $lockRealAncestor | Out-Null
    $lockAncestorRedirectionBlocked = $false
    try {
        $unexpectedLock = Enter-DbBackupInstallTreeLock `
            -InstallRoot (Join-Path $lockAncestorJunction "install")
        $unexpectedLock.Dispose()
    }
    catch {
        $lockAncestorRedirectionBlocked = $_.Exception.Message -match (
            "INSTALL_TREE_LOCK_FAILED"
        )
    }
    if (-not $lockAncestorRedirectionBlocked) {
        throw "install tree lock accepted an ancestor path redirection"
    }

    $partialLockRoot = Join-Path $fixtureRoot "partial-lock-cleanup"
    $partialLockRenamed = Join-Path $fixtureRoot "partial-lock-cleanup-renamed"
    New-Item -ItemType Directory -Path $partialLockRoot -Force | Out-Null
    $partialBlockedFile = Join-Path $partialLockRoot "blocked.bin"
    [IO.File]::WriteAllText($partialBlockedFile, "blocked fixture")
    $partialWriter = [IO.File]::Open(
        $partialBlockedFile,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
    )
    $partialAcquisitionBlocked = $false
    try {
        try {
            $unexpectedLock = Enter-DbBackupInstallTreeLock `
                -InstallRoot $partialLockRoot
            $unexpectedLock.Dispose()
            throw "partial tree lock acquisition unexpectedly succeeded"
        }
        catch {
            if ($_.Exception.Message -notmatch "INSTALL_TREE_LOCK_FAILED") {
                throw
            }
            $partialAcquisitionBlocked = $true
        }
    }
    finally {
        $partialWriter.Dispose()
    }
    if (-not $partialAcquisitionBlocked) {
        throw "partial tree lock acquisition did not fail closed"
    }
    [IO.Directory]::Move($partialLockRoot, $partialLockRenamed)
    [IO.Directory]::Move($partialLockRenamed, $partialLockRoot)

    $aclFixtureRoot = Join-Path $fixtureRoot "protected-acl"
    $aclFixtureChild = Join-Path $aclFixtureRoot "nested\payload.dll"
    New-Item -ItemType Directory -Path (Split-Path -Parent $aclFixtureChild) `
        -Force | Out-Null
    [IO.File]::WriteAllText(
        $aclFixtureChild,
        "dependency fixture",
        [Text.UTF8Encoding]::new($false)
    )
    $ownerSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $serviceSid = "S-1-5-80-111-222-333-444-555"
    $everyoneSid = [Security.Principal.SecurityIdentifier]::new("S-1-1-0")
    $seedAcl = Get-Acl -LiteralPath $aclFixtureRoot
    $seedAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $everyoneSid,
            [Security.AccessControl.FileSystemRights]::Modify,
            [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
                [Security.AccessControl.InheritanceFlags]::ObjectInherit,
            [Security.AccessControl.PropagationFlags]::None,
            [Security.AccessControl.AccessControlType]::Allow
        ))
    $seedAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new("S-1-5-32-545"),
            [Security.AccessControl.FileSystemRights]::CreateFiles,
            [Security.AccessControl.InheritanceFlags]::ContainerInherit,
            [Security.AccessControl.PropagationFlags]::None,
            [Security.AccessControl.AccessControlType]::Allow
        ))
    Set-Acl -LiteralPath $aclFixtureRoot -AclObject $seedAcl

    $aclPreopenedWriter = [IO.File]::Open(
        $aclFixtureChild,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete
    )
    try {
        $aclPreopenedWriterBlocked = $false
        try {
            Set-DbBackupProtectedAcl `
                -Path $aclFixtureRoot `
                -ServiceSid $serviceSid `
                -OwnerSid $ownerSid `
                -ServiceAccess "read-execute" `
                -Recurse
        }
        catch {
            $aclPreopenedWriterBlocked = $_.Exception.Message -match (
                "INSTALL_TREE_LOCK_FAILED"
            )
        }
        if (-not $aclPreopenedWriterBlocked) {
            throw "ACL transition accepted a pre-open child write handle"
        }
    }
    finally {
        $aclPreopenedWriter.Dispose()
    }

    Set-DbBackupProtectedAcl `
        -Path $aclFixtureRoot `
        -ServiceSid $serviceSid `
        -OwnerSid $ownerSid `
        -ServiceAccess "read-execute"
    Assert-DbBackupProtectedAcl `
        -Path $aclFixtureRoot `
        -ServiceSid $serviceSid `
        -OwnerSid $ownerSid `
        -ServiceAccess "read-execute"
    foreach ($aclPath in @($aclFixtureRoot, $aclFixtureChild)) {
        $broadRules = @((Get-Acl -LiteralPath $aclPath).GetAccessRules(
                $true,
                $true,
                [Security.Principal.SecurityIdentifier]
            ) | Where-Object {
                $_.IdentityReference.Value -in @(
                    "S-1-1-0",
                    "S-1-5-11",
                    "S-1-5-32-545"
                ) -and
                $_.AccessControlType -eq
                    [Security.AccessControl.AccessControlType]::Allow -and
                ($_.FileSystemRights -band
                    [Security.AccessControl.FileSystemRights]::Modify)
            })
        if ($broadRules.Count -ne 0) {
            throw "protected ACL retained a broad write-capable ACE"
        }
    }

    $tamperedAcl = Get-Acl -LiteralPath $aclFixtureRoot
    $tamperedAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $everyoneSid,
            [Security.AccessControl.FileSystemRights]::CreateFiles,
            [Security.AccessControl.InheritanceFlags]::ContainerInherit,
            [Security.AccessControl.PropagationFlags]::None,
            [Security.AccessControl.AccessControlType]::Allow
        ))
    Set-Acl -LiteralPath $aclFixtureRoot -AclObject $tamperedAcl
    $broadAclBlocked = $false
    try {
        Assert-DbBackupProtectedAcl `
            -Path $aclFixtureRoot `
            -ServiceSid $serviceSid `
            -OwnerSid $ownerSid `
            -ServiceAccess "read-execute"
    }
    catch {
        $broadAclBlocked = $_.Exception.Message -match "INSTALL_ACL_INVALID"
    }
    if (-not $broadAclBlocked) {
        throw "protected ACL verifier accepted a broad write-capable ACE"
    }
    Set-DbBackupProtectedAcl `
        -Path $aclFixtureRoot `
        -ServiceSid $serviceSid `
        -OwnerSid $ownerSid `
        -ServiceAccess "read-execute" `
        -Recurse
    $nestedAcl = Get-Acl -LiteralPath $aclFixtureChild
    $nestedAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            [Security.Principal.SecurityIdentifier]::new("S-1-5-32-544"),
            [Security.AccessControl.FileSystemRights]::FullControl,
            [Security.AccessControl.AccessControlType]::Allow
        ))
    Set-Acl -LiteralPath $aclFixtureChild -AclObject $nestedAcl
    $nestedAclDriftBlocked = $false
    try {
        Assert-DbBackupProtectedAcl `
            -Path $aclFixtureRoot `
            -ServiceSid $serviceSid `
            -OwnerSid $ownerSid `
            -ServiceAccess "read-execute" `
            -Recurse
    }
    catch {
        $nestedAclDriftBlocked = $_.Exception.Message -match "INSTALL_ACL_INVALID"
    }
    if (-not $nestedAclDriftBlocked) {
        throw "protected ACL verifier accepted a nested arbitrary full-control ACE"
    }
    Set-DbBackupProtectedAcl `
        -Path $aclFixtureRoot `
        -ServiceSid $serviceSid `
        -OwnerSid $ownerSid `
        -ServiceAccess "full" `
        -Recurse
    $excessServiceAccessBlocked = $false
    try {
        Assert-DbBackupProtectedAcl `
            -Path $aclFixtureRoot `
            -ServiceSid $serviceSid `
            -OwnerSid $ownerSid `
            -ServiceAccess "read-execute" `
            -Recurse
    }
    catch {
        $excessServiceAccessBlocked = $_.Exception.Message -match (
            "INSTALL_ACL_INVALID"
        )
    }
    if (-not $excessServiceAccessBlocked) {
        throw "protected ACL verifier accepted service full control on InstallRoot"
    }
    Set-DbBackupProtectedAcl `
        -Path $aclFixtureRoot `
        -ServiceSid $serviceSid `
        -OwnerSid $ownerSid `
        -ServiceAccess "read-execute" `
        -Recurse
    $aclReparseTarget = Join-Path $fixtureRoot "acl-reparse-target"
    $aclReparsePath = Join-Path $aclFixtureRoot "nested\junction"
    New-Item -ItemType Directory -Path $aclReparseTarget -Force | Out-Null
    New-Item -ItemType Junction -Path $aclReparsePath -Target $aclReparseTarget |
        Out-Null
    $aclReparseBlocked = $false
    try {
        Set-DbBackupProtectedAcl `
            -Path $aclFixtureRoot `
            -ServiceSid $serviceSid `
            -OwnerSid $ownerSid `
            -ServiceAccess "read-execute" `
            -Recurse
    }
    catch {
        $aclReparseBlocked = $_.Exception.Message -match (
            "INSTALL_TREE_REPARSE_POINT_BLOCKED"
        )
    }
    if (-not $aclReparseBlocked) {
        throw "protected ACL traversal accepted a reparse point"
    }
    Remove-Item -LiteralPath $aclReparsePath -Force

    $ownerFixtureData = Join-Path $fixtureRoot "persisted-owner"
    New-Item -ItemType Directory -Path $ownerFixtureData -Force | Out-Null
    $ownerFixtureIdentity = Join-Path $ownerFixtureData "service-identity.json"
    [IO.File]::WriteAllText(
        $ownerFixtureIdentity,
        (@{
                version = 1
                ownerSid = $ownerSid
                approvedAdministratorSids = @("S-1-5-32-544")
                trustedSigningKeys = @()
            } | ConvertTo-Json -Depth 8),
        [Text.UTF8Encoding]::new($false)
    )
    $virtualServiceSid = "S-1-5-80-1-2-3-4-5"
    Set-DbBackupProtectedAcl `
        -Path $ownerFixtureIdentity `
        -ServiceSid $virtualServiceSid `
        -OwnerSid $ownerSid
    $resolvedPersistedOwner = Resolve-DbBackupOwnerSid `
        -DataRoot $ownerFixtureData
    if ($resolvedPersistedOwner -ne $ownerSid) {
        throw "installer did not preserve the persisted installation owner"
    }
    $ownerMigrationBlocked = $false
    try {
        Resolve-DbBackupOwnerSid `
            -DataRoot $ownerFixtureData `
            -RequestedOwnerSid "S-1-5-19" | Out-Null
    }
    catch {
        $ownerMigrationBlocked = $_.Exception.Message -match (
            "INSTALL_OWNER_SID_MISMATCH"
        )
    }
    if (-not $ownerMigrationBlocked) {
        throw "installer silently changed the persisted installation owner"
    }
    $identityAcl = Get-Acl -LiteralPath $ownerFixtureIdentity
    $identityAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $everyoneSid,
            [Security.AccessControl.FileSystemRights]::Modify,
            [Security.AccessControl.AccessControlType]::Allow
        ))
    Set-Acl -LiteralPath $ownerFixtureIdentity -AclObject $identityAcl
    $broadIdentityAclBlocked = $false
    try {
        Resolve-DbBackupOwnerSid -DataRoot $ownerFixtureData | Out-Null
    }
    catch {
        $broadIdentityAclBlocked = $_.Exception.Message -match (
            "INSTALL_IDENTITY_INVALID"
        )
    }
    if (-not $broadIdentityAclBlocked) {
        throw "installer trusted a persisted owner document with a broad ACL"
    }
    $newOwnerContextBlocked = $false
    try {
        Resolve-DbBackupOwnerSid `
            -DataRoot (Join-Path $fixtureRoot "new-owner-context") `
            -RequestedOwnerSid "S-1-5-19" | Out-Null
    }
    catch {
        $newOwnerContextBlocked = $_.Exception.Message -match (
            "INSTALL_OWNER_CONTEXT_REQUIRED"
        )
    }
    if (-not $newOwnerContextBlocked) {
        throw "installer accepted a new owner outside the executing security context"
    }

    Write-FixtureManifest -Path $manifestPath -PackageRoot $packageRoot
    $installTreeRoot = Join-Path $fixtureRoot "managed-install-tree"
    $installTreeData = Join-Path $fixtureRoot "managed-install-data"
    New-Item -ItemType Directory -Path (Join-Path $installTreeRoot "tools\old") `
        -Force | Out-Null
    [IO.File]::WriteAllText(
        (Join-Path $installTreeRoot "stale.exe"),
        "stale executable",
        [Text.UTF8Encoding]::new($false)
    )
    [IO.File]::WriteAllText(
        (Join-Path $installTreeRoot "tools\old\stale.dll"),
        "stale dependency",
        [Text.UTF8Encoding]::new($false)
    )
    Copy-Item `
        -LiteralPath $manifestPath `
        -Destination (Join-Path $installTreeRoot "package-manifest.json")
    $loadedFixtureManifest = Read-DbBackupPackageManifest -ManifestPath $manifestPath
    $quarantine = Move-DbBackupUnmanifestedInstallFiles `
        -InstallRoot $installTreeRoot `
        -DataRoot $installTreeData `
        -Manifest $loadedFixtureManifest
    if ($quarantine.quarantinedCount -ne 2 -or
        [string]::IsNullOrWhiteSpace([string] $quarantine.quarantinePath) -or
        (Test-Path -LiteralPath (Join-Path $installTreeRoot "stale.exe")) -or
        (Test-Path -LiteralPath (Join-Path $installTreeRoot "tools\old\stale.dll")) -or
        -not (Test-Path -LiteralPath (
                Join-Path $installTreeRoot "package-manifest.json"
            ) -PathType Leaf) -or
        -not (Test-Path -LiteralPath (
                Join-Path $quarantine.quarantinePath "stale.exe"
            ) -PathType Leaf) -or
        -not (Test-Path -LiteralPath (
                Join-Path $quarantine.quarantinePath "tools\old\stale.dll"
            ) -PathType Leaf)) {
        throw "unmanifested install files were not moved to recoverable quarantine"
    }

    Copy-DbBackupPackageFiles `
        -PackageRoot $packageRoot `
        -InstallRoot $installTreeRoot `
        -Manifest $loadedFixtureManifest
    $fixtureManifestSha256 = (
        Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256
    ).Hash
    Copy-DbBackupPackageManifest `
        -ManifestPath $manifestPath `
        -InstallRoot $installTreeRoot `
        -ExpectedSha256 $fixtureManifestSha256
    Assert-DbBackupPackageManifest `
        -ManifestPath $manifestPath `
        -InstallRoot $installTreeRoot `
        -ExpectedSha256 $fixtureManifestSha256
    Test-DbBackupPackage `
        -PackageRoot $installTreeRoot `
        -Manifest $loadedFixtureManifest `
        -AllowUnsignedDevelopmentPackage
    $installedManifestPath = Join-Path $installTreeRoot "package-manifest.json"
    Add-Content -LiteralPath $installedManifestPath -Value "tampered"
    $installedManifestTamperBlocked = $false
    try {
        Assert-DbBackupPackageManifest `
            -ManifestPath $manifestPath `
            -InstallRoot $installTreeRoot `
            -ExpectedSha256 $fixtureManifestSha256
    }
    catch {
        $installedManifestTamperBlocked = $_.Exception.Message -match (
            "PACKAGE_INSTALLED_MANIFEST_INVALID"
        )
    }
    if (-not $installedManifestTamperBlocked) {
        throw "installed package manifest tampering was not blocked"
    }
    $trustedFixtureManifest = Read-DbBackupTrustedPackageManifest `
        -ManifestPath $manifestPath `
        -ExpectedSha256 $fixtureManifestSha256
    Add-Content -LiteralPath $manifestPath -Value "source tamper"
    Copy-DbBackupPackageManifest `
        -TrustedManifestBytes $trustedFixtureManifest.bytes `
        -InstallRoot $installTreeRoot `
        -ExpectedSha256 $fixtureManifestSha256
    $installedSnapshotHash = (
        Get-FileHash -LiteralPath $installedManifestPath -Algorithm SHA256
    ).Hash
    if ($installedSnapshotHash -cne $fixtureManifestSha256) {
        throw "installed manifest did not use the authenticated byte snapshot"
    }
    Write-FixtureManifest -Path $manifestPath -PackageRoot $packageRoot

    foreach ($scriptName in @("install.ps1", "repair.ps1", "update.ps1", "uninstall.ps1")) {
        $deploymentScriptText = Get-Content -LiteralPath (
            Join-Path $repoRoot "scripts\$scriptName"
        ) -Raw -Encoding UTF8
        if (-not $deploymentScriptText.Contains(
                "Assert-DbBackupDeploymentAuthenticode"
            ) -or
            -not $deploymentScriptText.Contains(
                "DEPLOYMENT_SCRIPT_AUTHENTICODE_INVALID"
            )) {
            throw "$scriptName does not authenticate itself and the installer module"
        }
    }

    $installText = Get-Content -LiteralPath (
        Join-Path $repoRoot "scripts\install.ps1"
    ) -Raw -Encoding UTF8
    $repairText = Get-Content -LiteralPath (
        Join-Path $repoRoot "scripts\repair.ps1"
    ) -Raw -Encoding UTF8
    if (-not $installText.Contains('-TrustedManifestBytes $manifestBytes') -or
        -not $repairText.Contains('-TrustedManifestBytes $manifestBytes') -or
        -not $updateText.Contains(
            '-TrustedManifestBytes $script:manifestBytes'
        )) {
        throw "lifecycle scripts do not install the authenticated manifest snapshot"
    }
    if (-not $installText.Contains(
            "INSTALL_EXISTING_DATA_ROOT_OWNER_SID_REQUIRED"
        )) {
        throw "clean install does not externally pin an existing DataRoot owner"
    }
    foreach ($lifecycleText in @($installText, $repairText)) {
        $canonicalManifestIndex = $lifecycleText.IndexOf(
            '$canonicalManifest = Assert-DbBackupChildPath',
            [StringComparison]::Ordinal
        )
        $rootTransitionIndex = $lifecycleText.IndexOf(
            'Initialize-DbBackupDataRoot',
            [StringComparison]::Ordinal
        )
        $rootLayoutIndex = $lifecycleText.IndexOf(
            'Assert-DbBackupSupportedRootLayout',
            [StringComparison]::Ordinal
        )
        $provenanceWriteIndex = $lifecycleText.IndexOf(
            'Write-DbBackupInstallRootProvenance',
            [StringComparison]::Ordinal
        )
        $stagingAclIndex = $lifecycleText.IndexOf(
            'Assert-DbBackupProtectedAcl -Path $package',
            [StringComparison]::Ordinal
        )
        $stagingLockIndex = $lifecycleText.IndexOf(
            'Enter-DbBackupInstallTreeLock -InstallRoot $package',
            [StringComparison]::Ordinal
        )
        $payloadCopyIndex = $lifecycleText.IndexOf(
            'Copy-DbBackupPackageFiles',
            [StringComparison]::Ordinal
        )
        $finalTreeLockIndex = $lifecycleText.LastIndexOf(
            'Enter-DbBackupInstallTreeLock -InstallRoot $install',
            [StringComparison]::Ordinal
        )
        $startIndex = $lifecycleText.LastIndexOf(
            'Start-Service -Name $ServiceName',
            [StringComparison]::Ordinal
        )
        $runningWaitIndex = $lifecycleText.LastIndexOf(
            'WaitForStatus("Running"',
            [StringComparison]::Ordinal
        )
        if ($stagingAclIndex -lt 0 -or
            $canonicalManifestIndex -lt 0 -or
            $rootLayoutIndex -lt 0 -or
            $rootLayoutIndex -ge $rootTransitionIndex -or
            $canonicalManifestIndex -ge $rootTransitionIndex -or
            $stagingLockIndex -le $stagingAclIndex -or
            $payloadCopyIndex -le $stagingLockIndex -or
            $finalTreeLockIndex -le $payloadCopyIndex -or
            $startIndex -le $finalTreeLockIndex -or
            $provenanceWriteIndex -le $payloadCopyIndex -or
            $provenanceWriteIndex -ge $finalTreeLockIndex -or
            $runningWaitIndex -le $startIndex) {
            throw "package staging or service-start locks are not held in order"
        }
        $preCopyAclIndex = $lifecycleText.IndexOf(
            "Set-DbBackupProtectedAcl",
            [StringComparison]::Ordinal
        )
        $quarantineIndex = $lifecycleText.IndexOf(
            "Move-DbBackupUnmanifestedInstallFiles",
            [StringComparison]::Ordinal
        )
        $copyIndex = $lifecycleText.IndexOf(
            "Copy-DbBackupPackageFiles",
            [StringComparison]::Ordinal
        )
        $postCopyVerifyIndex = $lifecycleText.LastIndexOf(
            "Test-DbBackupPackage",
            [StringComparison]::Ordinal
        )
        $finalAclVerifyIndex = $lifecycleText.LastIndexOf(
            "Assert-DbBackupProtectedAcl",
            [StringComparison]::Ordinal
        )
        $startIndex = $lifecycleText.IndexOf(
            "Start-Service",
            [StringComparison]::Ordinal
        )
        if ($preCopyAclIndex -lt 0 -or
            $copyIndex -lt 0 -or
            $preCopyAclIndex -gt $copyIndex -or
            $quarantineIndex -lt 0 -or
            $postCopyVerifyIndex -lt $quarantineIndex -or
            $finalAclVerifyIndex -lt $postCopyVerifyIndex -or
            $startIndex -lt $finalAclVerifyIndex) {
            throw "install and repair must ACL-gate copy and re-verify before service start"
        }
    }
    $repairProvenanceIndex = $repairText.IndexOf(
        'Assert-DbBackupInstallRootProvenance',
        [StringComparison]::Ordinal
    )
    $repairStopIndex = $repairText.IndexOf(
        'Stop-Service -Name $ServiceName',
        [StringComparison]::Ordinal
    )
    if ($repairProvenanceIndex -lt 0 -or
        $repairStopIndex -le $repairProvenanceIndex -or
        $repairText.IndexOf(
            'Assert-DbBackupSupportedRootLayout',
            [StringComparison]::Ordinal
        ) -ge $repairStopIndex) {
        throw "repair stops the service before validating root provenance"
    }
    $updateCanonicalIndex = $updateText.IndexOf(
        '$canonicalManifest = Assert-DbBackupChildPath',
        [StringComparison]::Ordinal
    )
    $updateInitializeIndex = $updateText.IndexOf(
        'Initialize-DbBackupDataRoot',
        [StringComparison]::Ordinal
    )
    $updateProvenanceIndex = $updateText.IndexOf(
        'Assert-DbBackupInstallRootProvenance',
        [StringComparison]::Ordinal
    )
    $updateStartIndex = $updateText.IndexOf(
        'Start-Service -Name $script:ServiceName',
        [StringComparison]::Ordinal
    )
    if ($updateCanonicalIndex -lt 0 -or
        $updateCanonicalIndex -ge $updateInitializeIndex -or
        $updateText.IndexOf(
            'Assert-DbBackupSupportedRootLayout',
            [StringComparison]::Ordinal
        ) -ge $updateInitializeIndex -or
        $updateProvenanceIndex -lt 0 -or
        $updateProvenanceIndex -ge $updateStartIndex) {
        throw "update does not validate canonical source/provenance before mutation/start"
    }
    if (-not $updateText.Contains(
            'Assert-DbBackupProtectedAcl -Path $package'
        ) -or
        -not $updateText.Contains(
            '$packageTreeLock = Enter-DbBackupInstallTreeLock'
        ) -or
        -not $updateText.Contains(
            '$installTreeLock = Enter-DbBackupInstallTreeLock'
        ) -or
        -not $updateText.Contains('Wait-DbBackupService -ExpectedStatus "Running"')) {
        throw "update staging/start paths do not retain the trusted tree locks"
    }
    foreach ($invalidRoots in @(
            @($installRoot, (Join-Path $installRoot "nested-data")),
            @((Join-Path $dataRoot "nested-install"), $dataRoot)
        )) {
        $ancestorBlocked = $false
        try {
            Assert-DbBackupSeparatedRoots `
                -InstallRoot $invalidRoots[0] `
                -DataRoot $invalidRoots[1]
        }
        catch {
            $ancestorBlocked = $_.Exception.Message -match "INSTALL_DATA_ROOT_OVERLAP"
        }
        if (-not $ancestorBlocked) {
            throw "ancestor install/data roots were not rejected"
        }
    }
    $atomicRoot = Join-Path $fixtureRoot "atomic-replace"
    New-Item -ItemType Directory -Path $atomicRoot -Force | Out-Null
    $atomicDestination = Join-Path $atomicRoot "destination.bin"
    $atomicTemporary = Join-Path $atomicRoot "temporary.bin"
    [IO.File]::WriteAllText($atomicDestination, "old")
    [IO.File]::WriteAllText($atomicTemporary, "new")
    Set-DbBackupFileAtomically `
        -TemporaryPath $atomicTemporary `
        -DestinationPath $atomicDestination
    if ((Get-Content -LiteralPath $atomicDestination -Raw) -ne "new" -or
        (Test-Path -LiteralPath $atomicTemporary)) {
        throw "atomic replacement did not commit the staged file"
    }
    $atomicNewDestination = Join-Path $atomicRoot "new-destination.bin"
    $atomicNewTemporary = Join-Path $atomicRoot "new-temporary.bin"
    [IO.File]::WriteAllText($atomicNewTemporary, "first")
    Set-DbBackupFileAtomically `
        -TemporaryPath $atomicNewTemporary `
        -DestinationPath $atomicNewDestination
    if ((Get-Content -LiteralPath $atomicNewDestination -Raw) -ne "first") {
        throw "atomic first placement did not commit the staged file"
    }

    $uninstallText = Get-Content -LiteralPath (
        Join-Path $repoRoot "scripts\uninstall.ps1"
    ) -Raw -Encoding UTF8
    if ($uninstallText -match "Remove-Item\s+.*(?:DataRoot|data)") {
        throw "uninstall script must never delete the data root"
    }
}
finally {
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $resolvedFixture = [IO.Path]::GetFullPath($fixtureRoot)
    if ($resolvedFixture.StartsWith(
            $tempRoot,
            [StringComparison]::OrdinalIgnoreCase
        ) -and
        $resolvedFixture -ne $tempRoot -and
        (Test-Path -LiteralPath $resolvedFixture)) {
        Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
    }
}

Write-Output "install lifecycle contract tests passed"
