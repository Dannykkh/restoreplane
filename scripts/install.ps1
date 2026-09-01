[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [Parameter(Mandatory = $true)] [string] $PackageRoot,
    [Parameter(Mandatory = $true)] [string] $ManifestPath,
    [string] $InstallRoot = "$env:ProgramFiles\RestorePlane",
    [string] $DataRoot = "$env:ProgramData\RestorePlane",
    [string] $ServiceName = "RestorePlaneService",
    [string] $ExpectedSignerThumbprint,
    [string] $ExpectedNativeToolAuditSha256,
    [string] $ExpectedPackageManifestSha256,
    [string] $OwnerSid,
    [switch] $AllowUnsignedDevelopmentPackage,
    [switch] $ValidateOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

function Assert-RestorePlaneDeploymentAuthenticode {
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
    "installer\RestorePlane.Install.Common.psm1"
if (-not $ValidateOnly -or -not $AllowUnsignedDevelopmentPackage) {
    Assert-RestorePlaneDeploymentAuthenticode `
        -Paths @($PSCommandPath, $installerModulePath) `
        -SignerThumbprint $ExpectedSignerThumbprint
}
Import-Module $installerModulePath -Force

$package = Resolve-RestorePlaneAbsolutePath -Path $PackageRoot
$manifestFile = Resolve-RestorePlaneAbsolutePath -Path $ManifestPath
$install = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot -AllowMissing
$data = Resolve-RestorePlaneAbsolutePath -Path $DataRoot -AllowMissing
Assert-RestorePlaneSeparatedRoots -InstallRoot $install -DataRoot $data
$canonicalManifest = Assert-RestorePlaneChildPath -Root $package -Candidate (
    Join-Path $package "package-manifest.json"
)
if (-not $manifestFile.Equals(
        $canonicalManifest,
        [StringComparison]::OrdinalIgnoreCase
    )) {
    throw "PACKAGE_MANIFEST_PATH_INVALID"
}
$trustedManifest = Read-RestorePlaneTrustedPackageManifest `
    -ManifestPath $manifestFile `
    -ExpectedSha256 $ExpectedPackageManifestSha256 `
    -AllowMissingExpectedDevelopmentFixture:(
        $ValidateOnly -and $AllowUnsignedDevelopmentPackage
    )
$manifest = $trustedManifest.manifest
$manifestSha256 = $trustedManifest.sha256
$manifestBytes = $trustedManifest.bytes
Test-RestorePlanePackage -PackageRoot $package -Manifest $manifest `
    -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
    -ExpectedNativeToolAuditSha256 $ExpectedNativeToolAuditSha256 `
    -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage

$contract = [ordered] @{
    schemaVersion = 1
    operation = "clean_install"
    serviceName = $ServiceName
    version = $manifest.version
    installRoot = $install
    dataRoot = $data
    serviceArguments = @("--data-root", $data)
    authorityBootstrap = "pristine_standalone_only"
    nativeToolsIncluded = $null -ne $manifest.PSObject.Properties["nativeTools"]
    nativeToolAuditSha256 = if ($null -ne $manifest.PSObject.Properties["nativeTools"]) {
        [string] $manifest.nativeTools.auditManifestSha256
    } else { $null }
    preserves = @("backup_artifacts", "control_backups", "operational_store")
    verifies = @(
        "sha256",
        "authenticode",
        "service_sid",
        "protected_acl",
        "service_health"
    )
    deploymentScriptAuthenticodeRequired = $true
    exactInstallTreeRequired = $true
    unmanifestedFileDisposition = "quarantine_before_start"
}
if ($ValidateOnly) {
    $contract | ConvertTo-Json -Depth 8
    return
}

Assert-RestorePlaneAdministrator
$existing = & sc.exe query $ServiceName 2>$null
if ($LASTEXITCODE -eq 0) {
    throw "INSTALL_SERVICE_ALREADY_EXISTS_USE_REPAIR"
}
if (-not $PSCmdlet.ShouldProcess($install, "Install RestorePlane Windows service")) {
    return
}
Assert-RestorePlaneSupportedRootLayout `
    -InstallRoot $install `
    -DataRoot $data

$dataRootExisted = Test-Path -LiteralPath $data -PathType Container
if ([string]::IsNullOrWhiteSpace($OwnerSid)) {
    $currentInstallerSid = (
        [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    )
    if ($currentInstallerSid -eq "S-1-5-18") {
        throw "INSTALL_OWNER_SID_REQUIRED"
    }
    if ($dataRootExisted) {
        throw "INSTALL_EXISTING_DATA_ROOT_OWNER_SID_REQUIRED"
    }
}
$OwnerSid = Resolve-RestorePlaneOwnerSid `
    -DataRoot $data `
    -RequestedOwnerSid $OwnerSid
$serviceSid = Get-RestorePlaneServiceSid -ServiceName $ServiceName
$dataRootTransition = Initialize-RestorePlaneDataRoot `
    -DataRoot $data `
    -OwnerSid $OwnerSid `
    -ServiceSid $serviceSid `
    -InstallRoot $install `
    -AllowInstallRootReplacement
$installRootTransition = Initialize-RestorePlaneInstallRoot `
    -InstallRoot $install `
    -DataRoot $data `
    -OwnerSid $OwnerSid `
    -ServiceSid $serviceSid
Set-RestorePlaneProtectedAcl -Path $install -OwnerSid $OwnerSid -Recurse
Assert-RestorePlaneProtectedAcl -Path $package -OwnerSid $OwnerSid -Recurse
$packageTreeLock = Enter-RestorePlaneInstallTreeLock -InstallRoot $package
try {
    Test-RestorePlanePackage -PackageRoot $package -Manifest $manifest `
        -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
        -ExpectedNativeToolAuditSha256 $ExpectedNativeToolAuditSha256 `
        -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage
    $installTreeQuarantine = Move-RestorePlaneUnmanifestedInstallFiles `
        -InstallRoot $install `
        -DataRoot $data `
        -Manifest $manifest
    Copy-RestorePlanePackageFiles `
        -PackageRoot $package `
        -InstallRoot $install `
        -Manifest $manifest
    Copy-RestorePlanePackageManifest -TrustedManifestBytes $manifestBytes `
        -InstallRoot $install `
        -ExpectedSha256 $manifestSha256
    Test-RestorePlanePackage -PackageRoot $install -Manifest $manifest `
        -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
        -ExpectedNativeToolAuditSha256 $ExpectedNativeToolAuditSha256 `
        -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage
}
finally {
    $packageTreeLock.Dispose()
}
$serviceExe = Assert-RestorePlaneChildPath -Root $install -Candidate (
    Join-Path $install "restoreplane-service.exe"
)
if (-not (Test-Path -LiteralPath $serviceExe -PathType Leaf)) {
    throw "INSTALL_SERVICE_BINARY_MISSING"
}

Invoke-RestorePlaneNative -FilePath "sc.exe" -Arguments @(
    "create",
    $ServiceName,
    "binPath=",
    "`"$serviceExe`" --data-root `"$data`"",
    "start=",
    "auto",
    "obj=",
    "NT SERVICE\$ServiceName",
    "DisplayName=",
    "RestorePlane Service"
) | Out-Null
Invoke-RestorePlaneNative -FilePath "sc.exe" -Arguments @(
    "sidtype",
    $ServiceName,
    "unrestricted"
) | Out-Null
Invoke-RestorePlaneNative -FilePath "sc.exe" -Arguments @(
    "failure",
    $ServiceName,
    "reset=",
    "86400",
    "actions=",
    "restart/5000/restart/15000/none/0"
) | Out-Null
$configuredServiceSid = Get-RestorePlaneServiceSid -ServiceName $ServiceName
if ($configuredServiceSid -ne $serviceSid) {
    throw "INSTALL_SERVICE_SID_CHANGED"
}

Set-RestorePlaneProtectedAcl -Path $install -ServiceSid $serviceSid `
    -OwnerSid $OwnerSid -ServiceAccess "read-execute" -Recurse
Set-RestorePlaneProtectedAcl -Path $data -ServiceSid $serviceSid -OwnerSid $OwnerSid
$identityPath = Join-Path $data "service-identity.json"
if (-not (Test-Path -LiteralPath $identityPath)) {
    $serviceManifestEntries = @($manifest.files | Where-Object {
            ([string] $_.path).Replace("\", "/") -ieq "restoreplane-service.exe"
        })
    if ($serviceManifestEntries.Count -ne 1) {
        throw "INSTALL_SERVICE_MANIFEST_ENTRY_INVALID"
    }
    Write-RestorePlaneJsonAtomic -Path $identityPath -Value ([ordered] @{
        version = 1
        ownerSid = $OwnerSid
        approvedAdministratorSids = @("S-1-5-32-544")
        standaloneBootstrap = [ordered] @{
            installationId = [Guid]::NewGuid().ToString()
            sourceRevision = ([string] $manifest.revision).ToLowerInvariant()
            packageManifestDigestSha256 = $manifestSha256.ToLowerInvariant()
            buildId = ([string] $manifest.buildId).ToLowerInvariant()
            serviceBinarySha256 = (
                [string] $serviceManifestEntries[0].sha256
            ).ToLowerInvariant()
        }
        trustedSigningKeys = @()
    })
}
Set-RestorePlaneProtectedAcl -Path $identityPath -ServiceSid $serviceSid `
    -OwnerSid $OwnerSid
Write-RestorePlaneInstallRootProvenance `
    -DataRoot $data `
    -InstallRoot $install `
    -OwnerSid $OwnerSid `
    -ServiceSid $serviceSid `
    -AllowInitialMint:$dataRootTransition.created
$installTreeLock = $null
$identityLock = $null
try {
    $installTreeLock = Enter-RestorePlaneInstallTreeLock -InstallRoot $install
    $identityLock = Enter-RestorePlanePathMutationLock -Path $identityPath
    Assert-RestorePlanePackageManifest -ManifestPath $manifestFile -InstallRoot $install `
        -ExpectedSha256 $manifestSha256
    Test-RestorePlanePackage -PackageRoot $install -Manifest $manifest `
        -ExpectedSignerThumbprint $ExpectedSignerThumbprint `
        -ExpectedNativeToolAuditSha256 $ExpectedNativeToolAuditSha256 `
        -AllowUnsignedDevelopmentPackage:$AllowUnsignedDevelopmentPackage
    Assert-RestorePlaneProtectedAcl -Path $install -ServiceSid $serviceSid `
        -OwnerSid $OwnerSid -ServiceAccess "read-execute" -Recurse
    Assert-RestorePlaneProtectedAcl -Path $data -ServiceSid $serviceSid `
        -OwnerSid $OwnerSid
    Assert-RestorePlaneProtectedAcl -Path $identityPath -ServiceSid $serviceSid `
        -OwnerSid $OwnerSid
    Assert-RestorePlaneInstallRootProvenance `
        -DataRoot $data `
        -InstallRoot $install `
        -OwnerSid $OwnerSid `
        -ServiceSid $serviceSid
    Start-Service -Name $ServiceName
    $service = Get-Service -Name $ServiceName
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

([ordered] @{
    schemaVersion = 1
    operation = "clean_install"
    status = "completed"
    serviceName = $ServiceName
    serviceSid = $serviceSid
    installRoot = $install
    dataRoot = $data
    preserved = $contract.preserves
    quarantinedInstallFileCount = $installTreeQuarantine.quarantinedCount
    installTreeQuarantinePath = $installTreeQuarantine.quarantinePath
    replacedInstallRootQuarantinePath = $installRootTransition.quarantinePath
} | ConvertTo-Json -Depth 8)
