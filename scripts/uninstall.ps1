[CmdletBinding(SupportsShouldProcess = $true)]
param(
    [string] $InstallRoot = "$env:ProgramFiles\RestorePlane",
    [string] $DataRoot = "$env:ProgramData\RestorePlane",
    [string] $ServiceName = "RestorePlaneService",
    [string] $ExpectedSignerThumbprint,
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
if (-not $ValidateOnly) {
    Assert-RestorePlaneDeploymentAuthenticode `
        -Paths @($PSCommandPath, $installerModulePath) `
        -SignerThumbprint $ExpectedSignerThumbprint
}
Import-Module $installerModulePath -Force

$install = Resolve-RestorePlaneAbsolutePath -Path $InstallRoot -AllowMissing
$data = Resolve-RestorePlaneAbsolutePath -Path $DataRoot -AllowMissing
Assert-RestorePlaneSeparatedRoots -InstallRoot $install -DataRoot $data
$preserved = @(
    (Join-Path $data "operational-v2.db"),
    (Join-Path $data "control-backups"),
    (Join-Path $data "support-bundles"),
    (Join-Path $data "telemetry-archives"),
    (Join-Path $data "spool")
)
$recovery = [ordered] @{
    afterSuccessfulUninstall = (
        "Run install.ps1 with the preserved DataRoot `"$data`"; " +
        "the service entry no longer exists after a normal uninstall."
    )
    serviceEntryStillRegistered = (
        "Run repair.ps1 with DataRoot `"$data`" only when the service entry " +
        "still exists and installed binaries or ACLs are damaged."
    )
}
if ($ValidateOnly) {
    [ordered] @{
        schemaVersion = 1
        operation = "uninstall"
        serviceName = $ServiceName
        action = "unregister_service_only"
        installRootPreserved = $install
        dataPathsPreserved = $preserved
        destructivePurgeSupported = $false
        recovery = $recovery
        deploymentScriptAuthenticodeRequired = $true
    } | ConvertTo-Json -Depth 8
    return
}

Assert-RestorePlaneAdministrator
if (-not $PSCmdlet.ShouldProcess($ServiceName, "Stop and unregister RestorePlane service")) {
    return
}
$service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($null -ne $service) {
    if ($service.Status -ne "Stopped") {
        Stop-Service -Name $ServiceName -NoWait
        $service.WaitForStatus("Stopped", [TimeSpan]::FromSeconds(180))
    }
    Invoke-RestorePlaneNative -FilePath "sc.exe" -Arguments @(
        "delete",
        $ServiceName
    ) | Out-Null
}

[ordered] @{
    schemaVersion = 1
    operation = "uninstall"
    status = "service_unregistered"
    installRootPreserved = $install
    dataPathsPreserved = $preserved
    recovery = $recovery
} | ConvertTo-Json -Depth 8
