[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Mandatory = $true)] [string] $PackageRoot,
    [Parameter(Mandatory = $true)] [string] $ManifestPath,
    [string] $InstallRoot = "$env:ProgramFiles\DbBackup",
    [string] $DataRoot = "$env:ProgramData\DbBackup",
    [string] $ServiceName = "DbBackupService",
    [string] $ExpectedSignerThumbprint,
    [string] $ExpectedNativeToolAuditSha256,
    [string] $ExpectedPackageManifestSha256,
    [ValidateRange(10, 3600)] [int] $DrainTimeoutSeconds = 300,
    [ValidateSet(
        "none",
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
    )] [string] $InjectFailure = "none",
    [switch] $AllowUnsignedDevelopmentPackage,
    [switch] $ValidateOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Assert-DbBackupDeploymentAuthenticode {
    param(
        [Parameter(Mandatory = $true)] [string[]] $Paths,
        [Parameter(Mandatory = $true)] [string] $SignerThumbprint
    )

    if ([string]::IsNullOrWhiteSpace($SignerThumbprint)) {
        throw "DEPLOYMENT_SCRIPT_SIGNER_REQUIRED"
    }
    $normalizedSigner = $SignerThumbprint.Replace(" ", "")
    if ($normalizedSigner -notmatch "^[0-9a-fA-F]{40}$") {
        throw "DEPLOYMENT_SCRIPT_SIGNER_REQUIRED"
    }
    $expected = $normalizedSigner.ToUpperInvariant()
    foreach ($path in $Paths) {
        $metadata = Get-Item -LiteralPath $path -Force
        $signature = Get-AuthenticodeSignature -LiteralPath $metadata.FullName
        if ($metadata.PSIsContainer -or
            $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint -or
            $signature.Status.ToString() -ne "Valid" -or
            $null -eq $signature.SignerCertificate -or
            $signature.SignerCertificate.Thumbprint.Replace(" ", "").ToUpperInvariant() -ne
                $expected) {
            throw "DEPLOYMENT_SCRIPT_AUTHENTICODE_INVALID"
        }
    }
}

$installerModulePath = Join-Path (Split-Path -Parent $PSScriptRoot) `
    "installer\DbBackup.Install.Common.psm1"
if (-not $ValidateOnly -or -not $AllowUnsignedDevelopmentPackage) {
    Assert-DbBackupDeploymentAuthenticode `
        -Paths @($PSCommandPath, $installerModulePath) `
        -SignerThumbprint $ExpectedSignerThumbprint
}
Import-Module $installerModulePath -Force

$stages = @(
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
$recoveryByStage = [ordered] @{
    download = "preserve_current_authority"
    verify_downloaded_artifact = "preserve_current_authority"
    stop_claims = "resume_current_build"
    drain = "resume_current_build"
    control_backup = "resume_current_build"
    compatibility_check = "resume_current_build"
    stop_service = "restore_previous_binary"
    verify_staged_artifact = "restore_previous_binary"
    replace_binary = "restore_previous_binary"
    migrate_schema = "forward_fix_claims_stopped"
    start_service = "forward_fix_claims_stopped"
    reconcile = "forward_fix_claims_stopped"
    commit_minimum_reader = "forward_fix_claims_stopped"
    resume_claims = "forward_fix_claims_stopped"
}

$package = Resolve-DbBackupAbsolutePath -Path $PackageRoot
$manifestFile = Resolve-DbBackupAbsolutePath -Path $ManifestPath
$install = Resolve-DbBackupAbsolutePath -Path $InstallRoot -AllowMissing
$data = Resolve-DbBackupAbsolutePath -Path $DataRoot -AllowMissing
Assert-DbBackupSeparatedRoots -InstallRoot $install -DataRoot $data
$canonicalManifest = Assert-DbBackupChildPath -Root $package -Candidate (
    Join-Path $package "package-manifest.json"
)
if (-not $manifestFile.Equals(
        $canonicalManifest,
        [StringComparison]::OrdinalIgnoreCase
    )) {
    throw "PACKAGE_MANIFEST_PATH_INVALID"
}
$trustedManifest = Read-DbBackupTrustedPackageManifest `
    -ManifestPath $manifestFile `
    -ExpectedSha256 $ExpectedPackageManifestSha256 `
    -AllowMissingExpectedDevelopmentFixture:(
        $ValidateOnly -and $AllowUnsignedDevelopmentPackage
    )
$manifest = $trustedManifest.manifest
$manifestSha256 = $trustedManifest.sha256
$manifestBytes = $trustedManifest.bytes
Test-DbBackupPackage -PackageRoot $package -Manifest $manifest `
    -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
    -ExpectedNativeToolAuditSha256 $ExpectedNativeToolAuditSha256 `
    -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage

if ($ValidateOnly) {
    [ordered] @{
        schemaVersion = 1
        operation = "update"
        serviceName = $ServiceName
        version = $manifest.version
        installRoot = $install
        dataRoot = $data
        serviceArguments = @("--data-root", $data)
        nativeToolsIncluded = $null -ne $manifest.PSObject.Properties["nativeTools"]
        nativeToolAuditSha256 = if ($null -ne $manifest.PSObject.Properties["nativeTools"]) {
            [string] $manifest.nativeTools.auditManifestSha256
        } else { $null }
        stages = $stages
        recoveryByStage = $recoveryByStage
        doubleSignatureVerification = $true
        updatePendingClaimGuard = $true
        minimumReaderCommitBeforeResume = $true
        deploymentScriptAuthenticodeRequired = $true
        exactInstallTreeRequired = $true
        unmanifestedFileDisposition = "quarantine_before_start"
    } | ConvertTo-Json -Depth 12
    return
}

Assert-DbBackupAdministrator
& sc.exe query $ServiceName 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "UPDATE_SERVICE_NOT_FOUND"
}
$cli = Assert-DbBackupChildPath -Root $install -Candidate (
    Join-Path $install "dbbackup.exe"
)
if (-not (Test-Path -LiteralPath $cli -PathType Leaf)) {
    throw "UPDATE_CLI_MISSING"
}
if (-not $PSCmdlet.ShouldProcess($install, "Update DbBackup to $($manifest.version)")) {
    return
}
Assert-DbBackupSupportedRootLayout `
    -InstallRoot $install `
    -DataRoot $data

$ownerSid = Resolve-DbBackupOwnerSid `
    -DataRoot $data `
    -RequirePersisted
$serviceSid = Get-DbBackupServiceSid -ServiceName $ServiceName
Assert-DbBackupInstallRootProvenance `
    -DataRoot $data `
    -InstallRoot $install `
    -OwnerSid $ownerSid `
    -ServiceSid $serviceSid
$null = Initialize-DbBackupDataRoot `
    -DataRoot $data `
    -OwnerSid $ownerSid `
    -ServiceSid $serviceSid `
    -InstallRoot $install
$identityPath = Assert-DbBackupChildPath -Root $data -Candidate (
    Join-Path $data "service-identity.json"
)
$serviceExe = Assert-DbBackupChildPath -Root $install -Candidate (
    Join-Path $install "dbbackup-service.exe"
)
if (-not (Test-Path -LiteralPath $serviceExe -PathType Leaf)) {
    throw "UPDATE_SERVICE_BINARY_MISSING"
}
Assert-DbBackupProtectedAcl -Path $install -ServiceSid $serviceSid `
    -OwnerSid $ownerSid -ServiceAccess "read-execute" -Recurse
Assert-DbBackupProtectedAcl -Path $data -ServiceSid $serviceSid `
    -OwnerSid $ownerSid
Assert-DbBackupProtectedAcl -Path $identityPath -ServiceSid $serviceSid `
    -OwnerSid $ownerSid
$updateId = [Guid]::NewGuid().ToString()
$updatesRoot = Join-Path $data "updates"
$rollbackRoot = Join-Path $updatesRoot ("rollback-" + $updateId)
$rollbackManifestPath = Join-Path $rollbackRoot "package-manifest.json"
$installedManifestPath = Join-Path $install "package-manifest.json"
$pendingPath = Join-Path $updatesRoot "update-pending.json"
$journalPath = Join-Path $updatesRoot ("update-" + $updateId + ".json")
New-Item -ItemType Directory -Path $updatesRoot -Force | Out-Null
Set-DbBackupProtectedAcl -Path $updatesRoot -ServiceSid $serviceSid `
    -OwnerSid $ownerSid
New-Item -ItemType Directory -Path $rollbackRoot -Force | Out-Null
Set-DbBackupProtectedAcl -Path $rollbackRoot -ServiceSid $serviceSid `
    -OwnerSid $ownerSid
$currentManifest = Read-DbBackupPackageManifest `
    -ManifestPath $installedManifestPath
$currentManifestSha256 = (
    Get-FileHash -LiteralPath $installedManifestPath -Algorithm SHA256
).Hash
$currentNativeAuditSha256 = if (
    $null -ne $currentManifest.PSObject.Properties["nativeTools"]
) {
    [string] $currentManifest.nativeTools.auditManifestSha256
}
else {
    $null
}
Test-DbBackupPackage -PackageRoot $install -Manifest $currentManifest `
    -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
    -ExpectedNativeToolAuditSha256 $currentNativeAuditSha256 `
    -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage
Copy-Item -LiteralPath $installedManifestPath -Destination $rollbackManifestPath
Assert-DbBackupPackageManifest `
    -ManifestPath $rollbackManifestPath `
    -InstallRoot $install `
    -ExpectedSha256 $currentManifestSha256
foreach ($entry in @($currentManifest.files)) {
    $source = Assert-DbBackupChildPath -Root $install -Candidate (
        Join-Path $install ([string] $entry.path)
    )
    $backup = Assert-DbBackupChildPath -Root $rollbackRoot -Candidate (
        Join-Path $rollbackRoot ([string] $entry.path)
    )
    New-Item -ItemType Directory -Path (Split-Path -Parent $backup) -Force |
        Out-Null
    Copy-Item -LiteralPath $source -Destination $backup
}
Set-DbBackupProtectedAcl -Path $rollbackRoot -ServiceSid $serviceSid `
    -OwnerSid $ownerSid -Recurse
Test-DbBackupPackage `
    -PackageRoot $rollbackRoot `
    -Manifest $currentManifest `
    -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
    -ExpectedNativeToolAuditSha256 $currentNativeAuditSha256 `
    -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage
$journal = [ordered] @{
    schemaVersion = 1
    updateId = $updateId
    targetVersion = $manifest.version
    startedAt = [DateTimeOffset]::Now.ToString("o")
    status = "running"
    currentStage = $null
    recoveryDisposition = $null
    recoveryCompleted = $false
    stages = [Collections.Generic.List[object]]::new()
    quarantinedInstallFileCount = 0
    installTreeQuarantinePath = $null
}
$script:installTreeTrustedForRestart = $true

function Write-UpdateJournal {
    Write-DbBackupJsonAtomic -Path $journalPath -Value $journal
}

function Invoke-InstalledCli {
    param([Parameter(Mandatory = $true)] [string[]] $Arguments)

    $lines = @(& $script:cli @Arguments)
    if ($LASTEXITCODE -ne 0) {
        throw "UPDATE_SERVICE_COMMAND_FAILED"
    }
    $json = $lines -join "`n"
    return $json | ConvertFrom-Json
}

function Wait-DbBackupService {
    param([Parameter(Mandatory = $true)] [string] $ExpectedStatus)

    $service = Get-Service -Name $script:ServiceName
    $service.WaitForStatus($ExpectedStatus, [TimeSpan]::FromSeconds(60))
}

function Assert-DbBackupInstalledStartGate {
    param(
        [Parameter(Mandatory = $true)] [string] $ManifestSource,
        [Parameter(Mandatory = $true)] [string] $ManifestSha256
    )

    Assert-DbBackupProtectedAcl `
        -Path $script:install `
        -ServiceSid $script:serviceSid `
        -OwnerSid $script:ownerSid `
        -ServiceAccess "read-execute" `
        -Recurse
    Assert-DbBackupProtectedAcl `
        -Path $script:data `
        -ServiceSid $script:serviceSid `
        -OwnerSid $script:ownerSid
    Assert-DbBackupProtectedAcl `
        -Path $script:identityPath `
        -ServiceSid $script:serviceSid `
        -OwnerSid $script:ownerSid
    Assert-DbBackupPackageManifest `
        -ManifestPath $ManifestSource `
        -InstallRoot $script:install `
        -ExpectedSha256 $ManifestSha256
    $installedManifest = Read-DbBackupPackageManifest `
        -ManifestPath $script:installedManifestPath
    $nativeAuditSha256 = if (
        $null -ne $installedManifest.PSObject.Properties["nativeTools"]
    ) {
        [string] $installedManifest.nativeTools.auditManifestSha256
    }
    else {
        $null
    }
    Test-DbBackupPackage `
        -PackageRoot $script:install `
        -Manifest $installedManifest `
        -ExpectedSignerThumbprint $script:ExpectedSignerThumbprint `
        -ExpectedNativeToolAuditSha256 $nativeAuditSha256 `
        -AllowUnsignedDevelopmentPackage:$script:AllowUnsignedDevelopmentPackage
}

function Start-DbBackupVerifiedService {
    param(
        [Parameter(Mandatory = $true)] [string] $ManifestSource,
        [Parameter(Mandatory = $true)] [string] $ManifestSha256
    )

    $installTreeLock = $null
    $identityLock = $null
    try {
        $installTreeLock = Enter-DbBackupInstallTreeLock `
            -InstallRoot $script:install
        $identityLock = Enter-DbBackupPathMutationLock `
            -Path $script:identityPath
        Assert-DbBackupInstalledStartGate `
            -ManifestSource $ManifestSource `
            -ManifestSha256 $ManifestSha256
        Assert-DbBackupInstallRootProvenance `
            -DataRoot $script:data `
            -InstallRoot $script:install `
            -OwnerSid $script:ownerSid `
            -ServiceSid $script:serviceSid
        Start-Service -Name $script:ServiceName
        Wait-DbBackupService -ExpectedStatus "Running"
    }
    finally {
        if ($null -ne $identityLock) {
            $identityLock.Dispose()
        }
        if ($null -ne $installTreeLock) {
            $installTreeLock.Dispose()
        }
    }
}

function Invoke-UpdateStage {
    param(
        [Parameter(Mandatory = $true)] [string] $Name,
        [Parameter(Mandatory = $true)] [scriptblock] $Action
    )

    $script:journal.currentStage = $Name
    $row = [ordered] @{
        stage = $Name
        startedAt = [DateTimeOffset]::Now.ToString("o")
        completedAt = $null
        status = "running"
    }
    $script:journal.stages.Add($row)
    Write-UpdateJournal
    if ($script:InjectFailure -eq $Name) {
        throw "UPDATE_FAULT_INJECTED_$($Name.ToUpperInvariant())"
    }
    & $Action
    $row.completedAt = [DateTimeOffset]::Now.ToString("o")
    $row.status = "passed"
    Write-UpdateJournal
}

function Remove-UpdatePendingMarker {
    if (Test-Path -LiteralPath $script:pendingPath -PathType Leaf) {
        Remove-Item -LiteralPath $script:pendingPath -Force
    }
}

function Restart-CurrentService {
    $service = Get-Service -Name $script:ServiceName
    if ($service.Status -ne "Stopped") {
        Stop-Service -Name $script:ServiceName
        Wait-DbBackupService -ExpectedStatus "Stopped"
    }
    Start-DbBackupVerifiedService `
        -ManifestSource $script:rollbackManifestPath `
        -ManifestSha256 $script:currentManifestSha256
    Wait-DbBackupService -ExpectedStatus "Running"
}

function Restore-PreviousBinary {
    $service = Get-Service -Name $script:ServiceName
    if ($service.Status -ne "Stopped") {
        Stop-Service -Name $script:ServiceName
        Wait-DbBackupService -ExpectedStatus "Stopped"
    }
    $previousManifest = Read-DbBackupPackageManifest `
        -ManifestPath $script:rollbackManifestPath
    $null = Move-DbBackupUnmanifestedInstallFiles `
        -InstallRoot $script:install `
        -DataRoot $script:data `
        -Manifest $previousManifest
    Copy-DbBackupPackageFiles `
        -PackageRoot $script:rollbackRoot `
        -InstallRoot $script:install `
        -Manifest $previousManifest
    Copy-DbBackupPackageManifest `
        -ManifestPath $script:rollbackManifestPath `
        -InstallRoot $script:install `
        -ExpectedSha256 $script:currentManifestSha256
    Set-DbBackupProtectedAcl `
        -Path $script:install `
        -ServiceSid $script:serviceSid `
        -OwnerSid $script:ownerSid `
        -ServiceAccess "read-execute" `
        -Recurse
    Assert-DbBackupInstalledStartGate `
        -ManifestSource $script:rollbackManifestPath `
        -ManifestSha256 $script:currentManifestSha256
    $script:installTreeTrustedForRestart = $true
    if (-not $script:installTreeTrustedForRestart) {
        throw "UPDATE_ROLLBACK_INSTALL_TREE_NOT_VERIFIED"
    }
    Remove-UpdatePendingMarker
    Start-DbBackupVerifiedService `
        -ManifestSource $script:rollbackManifestPath `
        -ManifestSha256 $script:currentManifestSha256
    Wait-DbBackupService -ExpectedStatus "Running"
}

Assert-DbBackupProtectedAcl -Path $package -OwnerSid $ownerSid -Recurse
$packageTreeLock = Enter-DbBackupInstallTreeLock -InstallRoot $package
try {
    Write-UpdateJournal
    Invoke-UpdateStage -Name "download" -Action {
        if (-not (Test-Path -LiteralPath $script:package -PathType Container)) {
            throw "UPDATE_PACKAGE_UNAVAILABLE"
        }
    }
    Invoke-UpdateStage -Name "verify_downloaded_artifact" -Action {
        Test-DbBackupPackage -PackageRoot $script:package -Manifest $script:manifest `
            -ExpectedSignerThumbprint $script:ExpectedSignerThumbprint `
            -ExpectedNativeToolAuditSha256 $script:ExpectedNativeToolAuditSha256 `
            -AllowUnsignedDevelopmentPackage:$script:AllowUnsignedDevelopmentPackage
    }
    Invoke-UpdateStage -Name "stop_claims" -Action {
        $outcome = Invoke-InstalledCli -Arguments @("service", "drain")
        if ($outcome.success -ne $true -or $outcome.result.state -ne "draining") {
            throw "UPDATE_DRAIN_REJECTED"
        }
    }
    Invoke-UpdateStage -Name "drain" -Action {
        $deadline = [DateTimeOffset]::Now.AddSeconds($script:DrainTimeoutSeconds)
        do {
            $outcome = Invoke-InstalledCli -Arguments @("service", "diagnostics")
            if ($outcome.success -eq $true -and $outcome.result.leases.active -eq 0) {
                return
            }
            Start-Sleep -Milliseconds 500
        } while ([DateTimeOffset]::Now -lt $deadline)
        throw "UPDATE_DRAIN_TIMEOUT"
    }
    Invoke-UpdateStage -Name "control_backup" -Action {
        $outcome = Invoke-InstalledCli -Arguments @("service", "control-backup")
        if ($outcome.success -ne $true -or
            $outcome.result.integrityResult -ne "passed" -or
            $outcome.result.rehearsalResult -ne "passed") {
            throw "UPDATE_CONTROL_BACKUP_FAILED"
        }
    }
    Invoke-UpdateStage -Name "compatibility_check" -Action {
        $target = [Version] $script:manifest.version
        $minimum = [Version] $script:manifest.minimumReaderVersion
        if ($minimum -gt $target) {
            throw "UPDATE_COMPATIBILITY_INVALID"
        }
        Write-DbBackupJsonAtomic -Path $script:pendingPath -Value ([ordered] @{
            schemaVersion = 1
            updateId = $script:updateId
            targetVersion = $script:manifest.version
            createdAtMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        })
    }
    Invoke-UpdateStage -Name "stop_service" -Action {
        $script:installTreeTrustedForRestart = $false
        Stop-Service -Name $script:ServiceName
        Wait-DbBackupService -ExpectedStatus "Stopped"
        Invoke-DbBackupNative -FilePath "sc.exe" -Arguments @(
            "config",
            $script:ServiceName,
            "binPath=",
            "`"$script:serviceExe`" --data-root `"$script:data`""
        ) | Out-Null
    }
    Invoke-UpdateStage -Name "verify_staged_artifact" -Action {
        Test-DbBackupPackage -PackageRoot $script:package -Manifest $script:manifest `
            -ExpectedSignerThumbprint $script:ExpectedSignerThumbprint `
            -ExpectedNativeToolAuditSha256 $script:ExpectedNativeToolAuditSha256 `
            -AllowUnsignedDevelopmentPackage:$script:AllowUnsignedDevelopmentPackage
    }
    Invoke-UpdateStage -Name "replace_binary" -Action {
        $quarantine = Move-DbBackupUnmanifestedInstallFiles `
            -InstallRoot $script:install `
            -DataRoot $script:data `
            -Manifest $script:manifest
        $script:journal.quarantinedInstallFileCount = $quarantine.quarantinedCount
        $script:journal.installTreeQuarantinePath = $quarantine.quarantinePath
        Write-UpdateJournal
        foreach ($entry in @($script:manifest.files)) {
            $source = Assert-DbBackupChildPath -Root $script:package -Candidate (
                Join-Path $script:package ([string] $entry.path)
            )
            $destination = Assert-DbBackupChildPath -Root $script:install -Candidate (
                Join-Path $script:install ([string] $entry.path)
            )
            New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force |
                Out-Null
            $temporary = "$destination.$($script:updateId).new"
            Copy-Item -LiteralPath $source -Destination $temporary -Force
            $actual = (Get-FileHash -LiteralPath $temporary -Algorithm SHA256).Hash
            if ($actual -ne ([string] $entry.sha256).ToUpperInvariant()) {
                throw "UPDATE_REPLACEMENT_HASH_MISMATCH"
            }
            Set-DbBackupFileAtomically `
                -TemporaryPath $temporary `
                -DestinationPath $destination
        }
        Copy-DbBackupPackageManifest `
            -TrustedManifestBytes $script:manifestBytes `
            -InstallRoot $script:install `
            -ExpectedSha256 $script:manifestSha256
        Set-DbBackupProtectedAcl `
            -Path $script:install `
            -ServiceSid $script:serviceSid `
            -OwnerSid $script:ownerSid `
            -ServiceAccess "read-execute" `
            -Recurse
        Assert-DbBackupPackageManifest `
            -ManifestPath $script:manifestFile `
            -InstallRoot $script:install `
            -ExpectedSha256 $script:manifestSha256
        Test-DbBackupPackage -PackageRoot $script:install -Manifest $script:manifest `
            -ExpectedSignerThumbprint $script:ExpectedSignerThumbprint `
            -ExpectedNativeToolAuditSha256 $script:ExpectedNativeToolAuditSha256 `
            -AllowUnsignedDevelopmentPackage:$script:AllowUnsignedDevelopmentPackage
        $script:installTreeTrustedForRestart = $true
        $script:cli = Join-Path $script:install "dbbackup.exe"
    }
    Invoke-UpdateStage -Name "migrate_schema" -Action {
        Start-DbBackupVerifiedService `
            -ManifestSource $script:manifestFile `
            -ManifestSha256 $script:manifestSha256
        Wait-DbBackupService -ExpectedStatus "Running"
        $outcome = Invoke-InstalledCli -Arguments @("service", "schema")
        if ($outcome.success -ne $true -or $outcome.result.userVersion -lt 2) {
            throw "UPDATE_SCHEMA_MIGRATION_FAILED"
        }
    }
    Invoke-UpdateStage -Name "start_service" -Action {
        if ((Get-Service -Name $script:ServiceName).Status -ne "Running") {
            throw "UPDATE_SERVICE_START_FAILED"
        }
    }
    Invoke-UpdateStage -Name "reconcile" -Action {
        $diagnostics = Invoke-InstalledCli -Arguments @("service", "diagnostics")
        $health = Invoke-InstalledCli -Arguments @("service", "status")
        if ($diagnostics.success -ne $true -or
            $diagnostics.result.leases.stale -ne 0 -or
            $health.success -ne $true -or
            $health.result.updatePending -ne $true -or
            $health.result.claimsEnabled -ne $false) {
            throw "UPDATE_RECONCILIATION_FAILED"
        }
    }
    Invoke-UpdateStage -Name "commit_minimum_reader" -Action {
        $outcome = Invoke-InstalledCli -Arguments @("service", "commit-update")
        if ($outcome.success -ne $true -or
            $outcome.result.state -ne "committed" -or
            $outcome.result.restartRequired -ne $true -or
            (Test-Path -LiteralPath $script:pendingPath)) {
            throw "UPDATE_MINIMUM_READER_COMMIT_FAILED"
        }
    }
    Invoke-UpdateStage -Name "resume_claims" -Action {
        Stop-Service -Name $script:ServiceName
        Wait-DbBackupService -ExpectedStatus "Stopped"
        Start-DbBackupVerifiedService `
            -ManifestSource $script:manifestFile `
            -ManifestSha256 $script:manifestSha256
        Wait-DbBackupService -ExpectedStatus "Running"
        $health = Invoke-InstalledCli -Arguments @("service", "status")
        if ($health.success -ne $true -or
            $health.result.updatePending -ne $false -or
            $health.result.phase -in @("diagnostics_only", "draining")) {
            throw "UPDATE_RESUME_FAILED"
        }
    }
    $journal.status = "completed"
    $journal.currentStage = $null
    $journal.completedAt = [DateTimeOffset]::Now.ToString("o")
    Write-UpdateJournal
}
catch {
    $failedStage = [string] $journal.currentStage
    $disposition = [string] $recoveryByStage[$failedStage]
    $journal.status = "failed"
    $journal.recoveryDisposition = $disposition
    $journal.errorCode = "SERVICE_UPDATE_STAGE_FAILED"
    try {
        switch ($disposition) {
            "preserve_current_authority" {
            }
            "resume_current_build" {
                Remove-UpdatePendingMarker
                Restart-CurrentService
            }
            "restore_previous_binary" {
                Restore-PreviousBinary
            }
            "forward_fix_claims_stopped" {
                $service = Get-Service -Name $ServiceName
                if ($service.Status -eq "Stopped") {
                    Start-DbBackupVerifiedService `
                        -ManifestSource $script:manifestFile `
                        -ManifestSha256 $script:manifestSha256
                    Wait-DbBackupService -ExpectedStatus "Running"
                }
            }
            default {
                throw "UPDATE_RECOVERY_DISPOSITION_INVALID"
            }
        }
        $journal.recoveryCompleted = $true
    }
    catch {
        $journal.recoveryCompleted = $false
        $journal.recoveryErrorCode = "SERVICE_UPDATE_RECOVERY_FAILED"
    }
    $journal.completedAt = [DateTimeOffset]::Now.ToString("o")
    Write-UpdateJournal
    throw
}
finally {
    $packageTreeLock.Dispose()
}

$journal | ConvertTo-Json -Depth 12
