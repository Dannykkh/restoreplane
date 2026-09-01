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
    [string] $OwnerSid,
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
        operation = "repair"
        serviceName = $ServiceName
        version = $manifest.version
        installRoot = $install
        dataRoot = $data
        serviceArguments = @("--data-root", $data)
        nativeToolsIncluded = $null -ne $manifest.PSObject.Properties["nativeTools"]
        nativeToolAuditSha256 = if ($null -ne $manifest.PSObject.Properties["nativeTools"]) {
            [string] $manifest.nativeTools.auditManifestSha256
        } else { $null }
        preserves = @("backup_artifacts", "control_backups", "operational_store")
        repairs = @("binary_hash", "service_definition", "service_sid", "acl")
        deploymentScriptAuthenticodeRequired = $true
        exactInstallTreeRequired = $true
        unmanifestedFileDisposition = "quarantine_before_start"
    } | ConvertTo-Json -Depth 8
    return
}

Assert-DbBackupAdministrator
& sc.exe query $ServiceName 2>$null | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "REPAIR_SERVICE_NOT_FOUND_USE_INSTALL"
}
if (-not $PSCmdlet.ShouldProcess($install, "Repair DbBackup installation")) {
    return
}
Assert-DbBackupSupportedRootLayout `
    -InstallRoot $install `
    -DataRoot $data

$OwnerSid = Resolve-DbBackupOwnerSid `
    -DataRoot $data `
    -RequestedOwnerSid $OwnerSid
$serviceSid = Get-DbBackupServiceSid -ServiceName $ServiceName
Assert-DbBackupInstallRootProvenance `
    -DataRoot $data `
    -InstallRoot $install `
    -OwnerSid $OwnerSid `
    -ServiceSid $serviceSid `
    -AllowInstallRootReplacement
$service = Get-Service -Name $ServiceName
if ($service.Status -ne "Stopped") {
    Stop-Service -Name $ServiceName -NoWait
    $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(180))
}
$null = Initialize-DbBackupDataRoot `
    -DataRoot $data `
    -OwnerSid $OwnerSid `
    -ServiceSid $serviceSid `
    -InstallRoot $install `
    -AllowInstallRootReplacement
$installRootTransition = Initialize-DbBackupInstallRoot `
    -InstallRoot $install `
    -DataRoot $data `
    -OwnerSid $OwnerSid `
    -ServiceSid $serviceSid
Set-DbBackupProtectedAcl -Path $install -OwnerSid $OwnerSid -Recurse
Assert-DbBackupProtectedAcl -Path $package -OwnerSid $OwnerSid -Recurse
$packageTreeLock = Enter-DbBackupInstallTreeLock -InstallRoot $package
try {
    Test-DbBackupPackage -PackageRoot $package -Manifest $manifest `
        -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
        -ExpectedNativeToolAuditSha256 $ExpectedNativeToolAuditSha256 `
        -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage
    $installTreeQuarantine = Move-DbBackupUnmanifestedInstallFiles `
        -InstallRoot $install `
        -DataRoot $data `
        -Manifest $manifest
    Copy-DbBackupPackageFiles `
        -PackageRoot $package `
        -InstallRoot $install `
        -Manifest $manifest
    Copy-DbBackupPackageManifest -TrustedManifestBytes $manifestBytes `
        -InstallRoot $install `
        -ExpectedSha256 $manifestSha256
    Test-DbBackupPackage -PackageRoot $install -Manifest $manifest `
        -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
        -ExpectedNativeToolAuditSha256 $ExpectedNativeToolAuditSha256 `
        -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage
}
finally {
    $packageTreeLock.Dispose()
}
$serviceExe = Assert-DbBackupChildPath -Root $install -Candidate (
    Join-Path $install "dbbackup-service.exe"
)
Invoke-DbBackupNative -FilePath "sc.exe" -Arguments @(
    "config",
    $ServiceName,
    "binPath=",
    "`"$serviceExe`" --data-root `"$data`"",
    "start=",
    "auto",
    "obj=",
    "NT SERVICE\$ServiceName"
) | Out-Null
Invoke-DbBackupNative -FilePath "sc.exe" -Arguments @(
    "sidtype",
    $ServiceName,
    "unrestricted"
) | Out-Null
$configuredServiceSid = Get-DbBackupServiceSid -ServiceName $ServiceName
if ($configuredServiceSid -ne $serviceSid) {
    throw "INSTALL_SERVICE_SID_CHANGED"
}
Set-DbBackupProtectedAcl -Path $install -ServiceSid $serviceSid `
    -OwnerSid $OwnerSid -ServiceAccess "read-execute" -Recurse
Set-DbBackupProtectedAcl -Path $data -ServiceSid $serviceSid -OwnerSid $OwnerSid
$identityPath = Join-Path $data "service-identity.json"
if (-not (Test-Path -LiteralPath $identityPath -PathType Leaf)) {
    Write-DbBackupJsonAtomic -Path $identityPath -Value ([ordered] @{
        version = 1
        ownerSid = $OwnerSid
        approvedAdministratorSids = @("S-1-5-32-544")
        trustedSigningKeys = @()
    })
}
Set-DbBackupProtectedAcl -Path $identityPath -ServiceSid $serviceSid `
    -OwnerSid $OwnerSid
Write-DbBackupInstallRootProvenance `
    -DataRoot $data `
    -InstallRoot $install `
    -OwnerSid $OwnerSid `
    -ServiceSid $serviceSid
$installTreeLock = $null
$identityLock = $null
try {
    $installTreeLock = Enter-DbBackupInstallTreeLock -InstallRoot $install
    $identityLock = Enter-DbBackupPathMutationLock -Path $identityPath
    Assert-DbBackupPackageManifest -ManifestPath $manifestFile -InstallRoot $install `
        -ExpectedSha256 $manifestSha256
    Test-DbBackupPackage -PackageRoot $install -Manifest $manifest `
        -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
        -ExpectedNativeToolAuditSha256 $ExpectedNativeToolAuditSha256 `
        -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage
    Assert-DbBackupProtectedAcl -Path $install -ServiceSid $serviceSid `
        -OwnerSid $OwnerSid -ServiceAccess "read-execute" -Recurse
    Assert-DbBackupProtectedAcl -Path $data -ServiceSid $serviceSid `
        -OwnerSid $OwnerSid
    Assert-DbBackupProtectedAcl -Path $identityPath -ServiceSid $serviceSid `
        -OwnerSid $OwnerSid
    Assert-DbBackupInstallRootProvenance `
        -DataRoot $data `
        -InstallRoot $install `
        -OwnerSid $OwnerSid `
        -ServiceSid $serviceSid
    Start-Service -Name $ServiceName
    $service.WaitForStatus("Running", [TimeSpan]::FromSeconds(30))
}
finally {
    if ($null -ne $identityLock) {
        $identityLock.Dispose()
    }
    if ($null -ne $installTreeLock) {
        $installTreeLock.Dispose()
    }
}

[ordered] @{
    schemaVersion = 1
    operation = "repair"
    status = "completed"
    serviceName = $ServiceName
    serviceSid = $serviceSid
    preserved = @("backup_artifacts", "control_backups", "operational_store")
    quarantinedInstallFileCount = $installTreeQuarantine.quarantinedCount
    installTreeQuarantinePath = $installTreeQuarantine.quarantinePath
    replacedInstallRootQuarantinePath = $installRootTransition.quarantinePath
} | ConvertTo-Json -Depth 8
