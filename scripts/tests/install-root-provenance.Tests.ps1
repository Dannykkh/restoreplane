[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$modulePath = Join-Path $repoRoot "installer\RestorePlane.Install.Common.psm1"
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "restoreplane-root-provenance-" + [Guid]::NewGuid().ToString("N")
)
$ownerSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$serviceSid = "S-1-5-80-1-2-3-4-5"

function Assert-FailsWith {
    param(
        [Parameter(Mandatory = $true)] [scriptblock] $Action,
        [Parameter(Mandatory = $true)] [string] $Pattern,
        [Parameter(Mandatory = $true)] [string] $Message
    )

    $blocked = $false
    try {
        & $Action
    }
    catch {
        $blocked = $_.Exception.Message -match $Pattern
    }
    if (-not $blocked) {
        throw $Message
    }
}

try {
    Import-Module $modulePath -Force
    $commonModule = Get-Module "RestorePlane.Install.Common"
    New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
    Set-RestorePlaneProtectedAcl -Path $fixtureRoot -OwnerSid $ownerSid
    $installParent = Join-Path $fixtureRoot "install-parent"
    $dataParent = Join-Path $fixtureRoot "data-parent"
    New-Item -ItemType Directory -Path $installParent -Force | Out-Null
    New-Item -ItemType Directory -Path $dataParent -Force | Out-Null
    Set-RestorePlaneProtectedAcl -Path $installParent -OwnerSid $ownerSid
    Set-RestorePlaneProtectedAcl -Path $dataParent -OwnerSid $ownerSid

    $directVolumeCandidate = Join-Path (
        [IO.Path]::GetPathRoot($fixtureRoot)
    ) ("restoreplane-direct-root-" + [Guid]::NewGuid().ToString("N"))
    & $commonModule {
        param($candidate)
        Assert-RestorePlaneSupportedRootParent `
            -Path $candidate `
            -DefaultParent ([Environment]::GetFolderPath(
                [Environment+SpecialFolder]::ProgramFiles
            ))
    } $directVolumeCandidate

    $dataRoot = Join-Path $dataParent "data"
    $dataTransition = Initialize-RestorePlaneDataRoot `
        -DataRoot $dataRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -TrustedAncestorRoot $fixtureRoot
    if ($dataTransition.created -ne $true) {
        throw "fresh DataRoot creation was not reported"
    }

    $installRoot = Join-Path $installParent "install"
    $null = Initialize-RestorePlaneInstallRoot `
        -InstallRoot $installRoot `
        -DataRoot $dataRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -TrustedAncestorRoot $fixtureRoot
    Set-RestorePlaneProtectedAcl `
        -Path $installRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -ServiceAccess "read-execute" `
        -Recurse

    Assert-FailsWith `
        -Action {
            Assert-RestorePlaneInstallRootProvenance `
                -DataRoot $dataRoot `
                -InstallRoot $installRoot `
                -OwnerSid $ownerSid `
                -ServiceSid $serviceSid
        } `
        -Pattern "INSTALL_ROOT_PROVENANCE_INVALID" `
        -Message "missing install-root provenance was accepted"

    Write-RestorePlaneInstallRootProvenance `
        -DataRoot $dataRoot `
        -InstallRoot $installRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -AllowInitialMint
    Assert-RestorePlaneInstallRootProvenance `
        -DataRoot $dataRoot `
        -InstallRoot $installRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid

    $secretsRoot = Join-Path $dataRoot "secrets"
    $everyone = [Security.Principal.SecurityIdentifier]::new("S-1-1-0")
    $secretAcl = Get-Acl -LiteralPath $secretsRoot
    $secretAcl.AddAccessRule(
        [Security.AccessControl.FileSystemAccessRule]::new(
            $everyone,
            [Security.AccessControl.FileSystemRights]::Read,
            [Security.AccessControl.AccessControlType]::Allow
        )
    )
    Set-Acl -LiteralPath $secretsRoot -AclObject $secretAcl
    Assert-FailsWith `
        -Action {
            Assert-RestorePlaneInstallRootProvenance `
                -DataRoot $dataRoot `
                -InstallRoot $installRoot `
                -OwnerSid $ownerSid `
                -ServiceSid $serviceSid
        } `
        -Pattern "INSTALL_ROOT_PROVENANCE_INVALID" `
        -Message "an unknown read ACE on the secrets root was accepted"
    Set-RestorePlaneProtectedAcl `
        -Path $secretsRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -Recurse
    Assert-RestorePlaneInstallRootProvenance `
        -DataRoot $dataRoot `
        -InstallRoot $installRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid

    $oldInstall = Join-Path $installParent "old-install"
    Move-RestorePlaneDirectoryByHandleAtomically `
        -SourcePath $installRoot `
        -DestinationPath $oldInstall
    New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
    Set-RestorePlaneProtectedAcl -Path $installRoot -OwnerSid $ownerSid
    Assert-FailsWith `
        -Action {
            Assert-RestorePlaneInstallRootProvenance `
                -DataRoot $dataRoot `
                -InstallRoot $installRoot `
                -OwnerSid $ownerSid `
                -ServiceSid $serviceSid
        } `
        -Pattern "INSTALL_ROOT_PROVENANCE_INVALID" `
        -Message "a replacement InstallRoot object reused old provenance"
    Assert-RestorePlaneInstallRootProvenance `
        -DataRoot $dataRoot `
        -InstallRoot $installRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -AllowInstallRootReplacement

    Remove-Item -LiteralPath $installRoot -Force
    Move-RestorePlaneDirectoryByHandleAtomically `
        -SourcePath $oldInstall `
        -DestinationPath $installRoot
    Assert-RestorePlaneInstallRootProvenance `
        -DataRoot $dataRoot `
        -InstallRoot $installRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid

    $spool = Join-Path $dataRoot "spool"
    $oldSpool = Join-Path $dataRoot "old-spool"
    Move-RestorePlaneDirectoryByHandleAtomically `
        -SourcePath $spool `
        -DestinationPath $oldSpool
    New-Item -ItemType Directory -Path $spool -Force | Out-Null
    Set-RestorePlaneProtectedAcl `
        -Path $spool `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid
    Assert-FailsWith `
        -Action {
            Assert-RestorePlaneInstallRootProvenance `
                -DataRoot $dataRoot `
                -InstallRoot $installRoot `
                -OwnerSid $ownerSid `
                -ServiceSid $serviceSid
        } `
        -Pattern "INSTALL_ROOT_PROVENANCE_INVALID" `
        -Message "a replacement DataRoot control directory reused old provenance"

    $broadAncestor = Join-Path $fixtureRoot "broad-ancestor"
    $protectedParent = Join-Path $broadAncestor "protected-parent"
    New-Item -ItemType Directory -Path $protectedParent -Force | Out-Null
    Set-RestorePlaneProtectedAcl -Path $protectedParent -OwnerSid $ownerSid
    $ancestorAcl = Get-Acl -LiteralPath $broadAncestor
    $ancestorAcl.AddAccessRule(
        [Security.AccessControl.FileSystemAccessRule]::new(
            $everyone,
            [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles,
            [Security.AccessControl.AccessControlType]::Allow
        )
    )
    Set-Acl -LiteralPath $broadAncestor -AclObject $ancestorAcl
    Assert-FailsWith `
        -Action {
            $null = Initialize-RestorePlaneInstallRoot `
                -InstallRoot (Join-Path $protectedParent "install") `
                -DataRoot $dataRoot `
                -OwnerSid $ownerSid `
                -ServiceSid $serviceSid `
                -TrustedAncestorRoot $fixtureRoot
        } `
        -Pattern "INSTALL_ROOT_REPLACEMENT_FAILED" `
        -Message "a mutation-capable custom-root ancestor was trusted"

    $unsupportedNestedParent = Join-Path $fixtureRoot "unsupported\nested"
    New-Item -ItemType Directory -Path $unsupportedNestedParent -Force |
        Out-Null
    Set-RestorePlaneProtectedAcl `
        -Path (Join-Path $fixtureRoot "unsupported") `
        -OwnerSid $ownerSid `
        -Recurse
    Assert-FailsWith `
        -Action {
            $null = Initialize-RestorePlaneInstallRoot `
                -InstallRoot (Join-Path $unsupportedNestedParent "install") `
                -DataRoot $dataRoot `
                -OwnerSid $ownerSid `
                -ServiceSid $serviceSid
        } `
        -Pattern "INSTALL_CUSTOM_ROOT_PARENT_UNTRUSTED" `
        -Message "a pre-existing nested custom root was accepted without provenance"

    Write-Output "install root provenance contract tests passed"
}
finally {
    if (Test-Path -LiteralPath $fixtureRoot) {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force
    }
}
