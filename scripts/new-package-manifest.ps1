[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $PackageRoot,
    [Parameter(Mandatory = $true)] [string] $OutputPath,
    [Parameter(Mandatory = $true)] [string] $Version,
    [Parameter(Mandatory = $true)] [string] $MinimumReaderVersion,
    [Parameter(Mandatory = $true)] [string] $BuildId,
    [Parameter(Mandatory = $true)] [string] $Revision,
    [Parameter(Mandatory = $true)] [string] $SourceTreeDigest,
    [Parameter(Mandatory = $true)] [string] $Publisher,
    [ValidateSet("development", "production")] [string] $Channel = "production",
    [string[]] $Files = @(),
    [switch] $AllowUnsignedDevelopmentPackage
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
Import-Module (
    Join-Path (Split-Path -Parent $PSScriptRoot) "installer\RestorePlane.Install.Common.psm1"
) -Force

$root = Resolve-RestorePlaneAbsolutePath -Path $PackageRoot
$output = Assert-RestorePlaneChildPath -Root $root -Candidate (
    [IO.Path]::GetFullPath($OutputPath)
)
$versionValue = $null
$minimumValue = $null
if (-not [Version]::TryParse($Version, [ref] $versionValue) -or
    -not [Version]::TryParse($MinimumReaderVersion, [ref] $minimumValue) -or
    $versionValue.ToString(3) -ne $Version -or
    $minimumValue.ToString(3) -ne $MinimumReaderVersion -or
    $minimumValue -gt $versionValue -or
    $BuildId -notmatch "^[0-9a-f]{64}$" -or
    $Revision -notmatch "^[0-9a-f]{40,64}$" -or
    $SourceTreeDigest -notmatch "^[0-9a-f]{64}$" -or
    [string]::IsNullOrWhiteSpace($Publisher)) {
    throw "PACKAGE_MANIFEST_INPUT_INVALID"
}
if ($Channel -eq "development" -and -not $AllowUnsignedDevelopmentPackage) {
    throw "PACKAGE_DEVELOPMENT_CHANNEL_NOT_ALLOWED"
}

$items = @(Get-ChildItem -LiteralPath $root -Recurse -Force -ErrorAction Stop)
if (@($items | Where-Object {
            $_.Attributes -band [IO.FileAttributes]::ReparsePoint
        }).Count -ne 0) {
    throw "PACKAGE_REPARSE_POINT_BLOCKED"
}
$payloadFiles = @(
    $items |
        Where-Object { -not $_.PSIsContainer -and $_.FullName -ne $output }
)
if ($payloadFiles.Count -eq 0 -or $payloadFiles.Count -gt 8192) {
    throw "PACKAGE_MANIFEST_INPUT_INVALID"
}
$discovered = @(
    $payloadFiles |
        ForEach-Object {
            $_.FullName.Substring($root.Length).
                TrimStart("\", "/").Replace("\", "/")
        } |
        Sort-Object
)
if ($Channel -eq "production") {
    foreach ($requiredBinary in @("restoreplane-service.exe", "restoreplane.exe")) {
        if ($requiredBinary -notin $discovered) {
            throw "PACKAGE_MANIFEST_FILE_MISSING"
        }
    }
    if (@($discovered | Where-Object {
                $_ -notin @(
                    "restoreplane-service.exe",
                    "restoreplane.exe",
                    "provider-certifications.json"
                ) -and
                -not $_.StartsWith("tools/", [StringComparison]::Ordinal)
            }).Count -ne 0) {
        throw "PACKAGE_MANIFEST_FILE_INVALID"
    }
}
if ($Files.Count -gt 0) {
    $requested = @($Files | ForEach-Object { $_.Replace("\", "/") } | Sort-Object)
    if (($requested -join "`n") -cne ($discovered -join "`n")) {
        throw "PACKAGE_MANIFEST_FILE_SET_INCOMPLETE"
    }
}

$auditRelative = "tools/native-tools-audit.json"
$auditPath = Join-Path $root $auditRelative
$audit = $null
$nativeTools = $null
$vendorByPath = @{}
if (Test-Path -LiteralPath $auditPath -PathType Leaf) {
    try {
        $audit = Get-Content -LiteralPath $auditPath -Raw -Encoding UTF8 |
            ConvertFrom-Json
    }
    catch {
        throw "PACKAGE_NATIVE_TOOL_AUDIT_INVALID"
    }
    if ($audit.schemaVersion -ne 1 -or
        @($audit.files).Count -eq 0 -or
        @($audit.files).Count -gt 4096) {
        throw "PACKAGE_NATIVE_TOOL_AUDIT_INVALID"
    }
    foreach ($tool in @($audit.files)) {
        $relative = "tools/" + ([string] $tool.path).Replace("\", "/")
        if ($vendorByPath.ContainsKey($relative)) {
            throw "PACKAGE_NATIVE_TOOL_AUDIT_INVALID"
        }
        $vendorByPath[$relative] = $tool
    }
    $expectedToolPaths = @($vendorByPath.Keys + $auditRelative | Sort-Object)
    $actualToolPaths = @(
        $discovered |
            Where-Object { $_.StartsWith("tools/", [StringComparison]::Ordinal) } |
            Sort-Object
    )
    if (($expectedToolPaths -join "`n") -cne ($actualToolPaths -join "`n")) {
        throw "PACKAGE_NATIVE_TOOL_TREE_INCOMPLETE"
    }
    $nativeTools = [ordered] @{
        auditManifestPath = $auditRelative
        auditManifestSha256 = (
            Get-FileHash -LiteralPath $auditPath -Algorithm SHA256
        ).Hash.ToLowerInvariant()
        fileCount = @($audit.files).Count
    }
}
elseif ($Channel -eq "production") {
    throw "PACKAGE_NATIVE_TOOLS_REQUIRED"
}
if ($Channel -eq "production" -and
    "provider-certifications.json" -notin $discovered) {
    throw "PACKAGE_PROVIDER_CERTIFICATIONS_REQUIRED"
}

$seen = [Collections.Generic.HashSet[string]]::new(
    [StringComparer]::OrdinalIgnoreCase
)
$entries = foreach ($relative in $discovered) {
    if ([string]::IsNullOrWhiteSpace($relative) -or
        [IO.Path]::IsPathRooted($relative) -or
        $relative.Contains("..") -or
        -not $seen.Add($relative.Replace("\", "/"))) {
        throw "PACKAGE_MANIFEST_FILE_INVALID"
    }
    $path = Assert-RestorePlaneChildPath -Root $root -Candidate (
        Join-Path $root $relative
    )
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "PACKAGE_MANIFEST_FILE_MISSING"
    }
    $vendor = if ($vendorByPath.ContainsKey($relative)) {
        $vendorByPath[$relative]
    }
    else {
        $null
    }
    $isExecutable = [IO.Path]::GetExtension($relative).ToLowerInvariant() -in @(
        ".exe", ".dll", ".msi"
    )
    $authenticodeRequired = if ($null -ne $vendor) {
        $vendor.authenticodeRequired -eq $true
    }
    else {
        $Channel -eq "production" -and $isExecutable
    }
    $signerRole = if ($authenticodeRequired -and $null -ne $vendor) {
        "vendor"
    }
    elseif ($authenticodeRequired) {
        "release"
    }
    else {
        "hash_only"
    }
    [ordered] @{
        path = $relative.Replace("\", "/")
        sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
        sizeBytes = [long] (Get-Item -LiteralPath $path -Force).Length
        authenticodeRequired = $authenticodeRequired
        signerRole = $signerRole
        signerThumbprint = if ($signerRole -eq "vendor") {
            ([string] $vendor.signerThumbprint).Replace(" ", "").ToLowerInvariant()
        }
        else {
            ""
        }
        publisher = if ($null -ne $vendor) {
            [string] $vendor.publisher
        }
        else {
            $Publisher
        }
    }
}
$manifest = [ordered] @{
    schemaVersion = 1
    channel = $Channel
    version = $Version
    minimumReaderVersion = $MinimumReaderVersion
    buildId = $BuildId
    revision = $Revision
    sourceTreeDigest = $SourceTreeDigest
    files = @($entries)
}
if ($null -ne $nativeTools) {
    $manifest.nativeTools = $nativeTools
}
Write-RestorePlaneJsonAtomic -Path $output -Value $manifest
$manifest | ConvertTo-Json -Depth 16
