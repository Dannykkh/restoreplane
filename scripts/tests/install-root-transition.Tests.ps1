[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$modulePath = Join-Path $repoRoot "installer\RestorePlane.Install.Common.psm1"
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "restoreplane-root-transition-" + [Guid]::NewGuid().ToString("N")
)
$ownerSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$serviceSid = "S-1-5-80-1-2-3-4-5"

try {
    Import-Module $modulePath -Force
    New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
    Set-RestorePlaneProtectedAcl -Path $fixtureRoot -OwnerSid $ownerSid
    $installParent = Join-Path $fixtureRoot "install-parent"
    $dataParent = Join-Path $fixtureRoot "data-parent"
    New-Item -ItemType Directory -Path $installParent -Force | Out-Null
    New-Item -ItemType Directory -Path $dataParent -Force | Out-Null
    Set-RestorePlaneProtectedAcl -Path $installParent -OwnerSid $ownerSid
    Set-RestorePlaneProtectedAcl -Path $dataParent -OwnerSid $ownerSid

    $moveSource = Join-Path $installParent "move-source"
    $moveDestination = Join-Path $installParent "move-destination"
    New-Item -ItemType Directory `
        -Path (Join-Path $moveSource "nested") `
        -Force | Out-Null
    [IO.File]::WriteAllText(
        (Join-Path $moveSource "nested\payload.dll"),
        "whole root fixture",
        [Text.UTF8Encoding]::new($false)
    )
    $moveIdentityBefore = Get-RestorePlanePathIdentity -Path $moveSource
    if ($moveIdentityBefore -notmatch (
            '^v1:[0-9a-f]{16}:[0-9a-f]{32}$'
        )) {
        throw "path identity is not in the canonical v1 format"
    }
    if ($moveIdentityBefore -eq (
            Get-RestorePlanePathIdentity -Path $installParent
        )) {
        throw "distinct directories returned the same path identity"
    }
    Move-RestorePlaneDirectoryByHandleAtomically `
        -SourcePath $moveSource `
        -DestinationPath $moveDestination
    if ((Test-Path -LiteralPath $moveSource) -or
        -not (Test-Path -LiteralPath (
                Join-Path $moveDestination "nested\payload.dll"
            ) -PathType Leaf)) {
        throw "handle-based whole-directory rename did not preserve the source object"
    }
    if ($moveIdentityBefore -ne (
            Get-RestorePlanePathIdentity -Path $moveDestination
        )) {
        throw "handle-based rename changed the directory object identity"
    }

    $existingMoveSource = Join-Path $installParent "existing-move-source"
    $existingMoveDestination = Join-Path (
        $installParent
    ) "existing-move-destination"
    New-Item -ItemType Directory -Path $existingMoveSource -Force | Out-Null
    New-Item `
        -ItemType Directory `
        -Path $existingMoveDestination `
        -Force | Out-Null
    $existingDestinationIdentity = Get-RestorePlanePathIdentity `
        -Path $existingMoveDestination
    $existingDestinationBlocked = $false
    try {
        Move-RestorePlaneDirectoryByHandleAtomically `
            -SourcePath $existingMoveSource `
            -DestinationPath $existingMoveDestination
    }
    catch {
        $existingDestinationBlocked = $_.Exception.Message -match (
            "INSTALL_DIRECTORY_MOVE_FAILED"
        )
    }
    if (-not $existingDestinationBlocked -or
        -not (Test-Path -LiteralPath $existingMoveSource -PathType Container) -or
        $existingDestinationIdentity -ne (
            Get-RestorePlanePathIdentity -Path $existingMoveDestination
        )) {
        throw "handle-based rename replaced an existing destination"
    }

    $blockedMoveSource = Join-Path $installParent "blocked-move-source"
    $blockedMoveDestination = Join-Path $installParent "blocked-move-destination"
    New-Item -ItemType Directory -Path $blockedMoveSource -Force | Out-Null
    $sourceDeleteBlocker = Enter-RestorePlanePathMutationLock -Path $blockedMoveSource
    try {
        $nonDeleteShareBlocked = $false
        try {
            Move-RestorePlaneDirectoryByHandleAtomically `
                -SourcePath $blockedMoveSource `
                -DestinationPath $blockedMoveDestination
        }
        catch {
            $nonDeleteShareBlocked = $_.Exception.Message -match (
                "INSTALL_DIRECTORY_MOVE_FAILED"
            )
        }
        if (-not $nonDeleteShareBlocked) {
            throw "directory rename accepted a pre-open non-delete-share handle"
        }
    }
    finally {
        $sourceDeleteBlocker.Dispose()
    }
    if (-not (Test-Path -LiteralPath $blockedMoveSource -PathType Container) -or
        (Test-Path -LiteralPath $blockedMoveDestination)) {
        throw "failed handle-based rename changed the directory name"
    }

    $renameReparseTarget = Join-Path $installParent "rename-reparse-target"
    $renameReparseSource = Join-Path $installParent "rename-reparse-source"
    New-Item -ItemType Directory -Path $renameReparseTarget -Force | Out-Null
    New-Item -ItemType Junction `
        -Path $renameReparseSource `
        -Target $renameReparseTarget | Out-Null
    $renameLeafReparseBlocked = $false
    try {
        Move-RestorePlaneDirectoryByHandleAtomically `
            -SourcePath $renameReparseSource `
            -DestinationPath (Join-Path $installParent "rename-reparse-destination")
    }
    catch {
        $renameLeafReparseBlocked = $_.Exception.Message -match (
            "INSTALL_DIRECTORY_MOVE_FAILED"
        )
    }
    if (-not $renameLeafReparseBlocked) {
        throw "handle-based rename accepted a source directory reparse point"
    }
    $identityLeafReparseBlocked = $false
    try {
        Get-RestorePlanePathIdentity -Path $renameReparseSource | Out-Null
    }
    catch {
        $identityLeafReparseBlocked = $_.Exception.Message -match (
            "INSTALL_PATH_IDENTITY_FAILED"
        )
    }
    if (-not $identityLeafReparseBlocked) {
        throw "path identity accepted a leaf reparse point"
    }

    $renameRealAncestor = Join-Path $installParent "rename-real-ancestor"
    $renameAncestorJunction = Join-Path $installParent "rename-ancestor-junction"
    New-Item -ItemType Directory `
        -Path (Join-Path $renameRealAncestor "source") `
        -Force | Out-Null
    New-Item -ItemType Junction `
        -Path $renameAncestorJunction `
        -Target $renameRealAncestor | Out-Null
    $renameAncestorReparseBlocked = $false
    try {
        Move-RestorePlaneDirectoryByHandleAtomically `
            -SourcePath (Join-Path $renameAncestorJunction "source") `
            -DestinationPath (Join-Path $installParent "ancestor-destination")
    }
    catch {
        $renameAncestorReparseBlocked = $_.Exception.Message -match (
            "INSTALL_DIRECTORY_MOVE_FAILED"
        )
    }
    if (-not $renameAncestorReparseBlocked) {
        throw "handle-based rename accepted an ancestor path redirection"
    }
    $identityAncestorReparseBlocked = $false
    try {
        Get-RestorePlanePathIdentity `
            -Path (Join-Path $renameAncestorJunction "source") | Out-Null
    }
    catch {
        $identityAncestorReparseBlocked = $_.Exception.Message -match (
            "INSTALL_PATH_IDENTITY_FAILED"
        )
    }
    if (-not $identityAncestorReparseBlocked) {
        throw "path identity accepted an ancestor path redirection"
    }

    $renameDestinationTarget = Join-Path (
        $installParent
    ) "rename-destination-target"
    $renameDestinationJunction = Join-Path (
        $installParent
    ) "rename-destination-junction"
    $renameDestinationSource = Join-Path (
        $installParent
    ) "rename-destination-source"
    New-Item -ItemType Directory -Path $renameDestinationTarget -Force |
        Out-Null
    New-Item -ItemType Directory -Path $renameDestinationSource -Force |
        Out-Null
    New-Item -ItemType Junction `
        -Path $renameDestinationJunction `
        -Target $renameDestinationTarget | Out-Null
    $renameDestinationReparseBlocked = $false
    try {
        Move-RestorePlaneDirectoryByHandleAtomically `
            -SourcePath $renameDestinationSource `
            -DestinationPath (
                Join-Path $renameDestinationJunction "destination"
            )
    }
    catch {
        $renameDestinationReparseBlocked = $_.Exception.Message -match (
            "INSTALL_DIRECTORY_MOVE_FAILED"
        )
    }
    if (-not $renameDestinationReparseBlocked -or
        -not (Test-Path `
            -LiteralPath $renameDestinationSource `
            -PathType Container)) {
        throw "handle-based rename accepted a destination parent redirection"
    }

    $dataRoot = Join-Path $dataParent "data"
    $null = Initialize-RestorePlaneDataRoot `
        -DataRoot $dataRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -TrustedAncestorRoot $fixtureRoot
    Assert-RestorePlaneProtectedAcl `
        -Path $dataRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid
    foreach ($name in @(
            "secrets",
            "spool",
            "control-backups",
            "support-bundles",
            "telemetry-archives",
            "updates"
        )) {
        Assert-RestorePlaneProtectedAcl `
            -Path (Join-Path $dataRoot $name) `
            -OwnerSid $ownerSid `
            -ServiceSid $serviceSid `
            -Recurse
    }

    $installRoot = Join-Path $installParent "install"
    New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
    [IO.File]::WriteAllText(
        (Join-Path $installRoot "attacker.dll"),
        "untrusted",
        [Text.UTF8Encoding]::new($false)
    )
    $everyone = [Security.Principal.SecurityIdentifier]::new("S-1-1-0")
    $broadAcl = Get-Acl -LiteralPath $installRoot
    $broadAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $everyone,
            [Security.AccessControl.FileSystemRights]::Modify,
            [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
                [Security.AccessControl.InheritanceFlags]::ObjectInherit,
            [Security.AccessControl.PropagationFlags]::None,
            [Security.AccessControl.AccessControlType]::Allow
        ))
    Set-Acl -LiteralPath $installRoot -AclObject $broadAcl

    $transition = Initialize-RestorePlaneInstallRoot `
        -InstallRoot $installRoot `
        -DataRoot $dataRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -TrustedAncestorRoot $fixtureRoot
    Assert-RestorePlaneProtectedAcl -Path $installRoot -OwnerSid $ownerSid -Recurse
    if ([string]::IsNullOrWhiteSpace([string] $transition.quarantinePath) -or
        -not (Test-Path -LiteralPath (
                Join-Path $transition.quarantinePath "attacker.dll"
            ) -PathType Leaf) -or
        (Test-Path -LiteralPath (Join-Path $installRoot "attacker.dll"))) {
        throw "untrusted InstallRoot was not replaced as a whole object"
    }

    $previousExactFile = Join-Path $installRoot "previous-exact.dll"
    [IO.File]::WriteAllText(
        $previousExactFile,
        "previous exact tree",
        [Text.UTF8Encoding]::new($false)
    )
    Set-RestorePlaneProtectedAcl `
        -Path $installRoot `
        -OwnerSid $ownerSid `
        -Recurse
    $exactTransition = Initialize-RestorePlaneInstallRoot `
        -InstallRoot $installRoot `
        -DataRoot $dataRoot `
        -OwnerSid $ownerSid `
        -ServiceSid $serviceSid `
        -TrustedAncestorRoot $fixtureRoot
    if ([string]::IsNullOrWhiteSpace(
            [string] $exactTransition.quarantinePath
        ) -or
        -not (Test-Path -LiteralPath (
                Join-Path $exactTransition.quarantinePath "previous-exact.dll"
            ) -PathType Leaf) -or
        (Test-Path -LiteralPath $previousExactFile)) {
        throw "install/repair reused an existing root with unknown handle history"
    }

    $unsafeControlDirectory = Join-Path $dataRoot "spool"
    $unsafeAcl = Get-Acl -LiteralPath $unsafeControlDirectory
    $unsafeAcl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $everyone,
            [Security.AccessControl.FileSystemRights]::Modify,
            [Security.AccessControl.AccessControlType]::Allow
        ))
    Set-Acl -LiteralPath $unsafeControlDirectory -AclObject $unsafeAcl
    $unsafeDataBlocked = $false
    try {
        Initialize-RestorePlaneDataRoot `
            -DataRoot $dataRoot `
            -OwnerSid $ownerSid `
            -ServiceSid $serviceSid `
            -TrustedAncestorRoot $fixtureRoot
    }
    catch {
        $unsafeDataBlocked = $_.Exception.Message -match (
            "INSTALL_DATA_ROOT_INVALID"
        )
    }
    if (-not $unsafeDataBlocked) {
        throw "unsafe existing DataRoot control directory was reused"
    }

    Write-Output "install root transition contract tests passed"
}
finally {
    if (Test-Path -LiteralPath $fixtureRoot) {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force
    }
}
