[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $NativeToolRoot,
    [Parameter(Mandatory = $true)] [string] $PackageRoot,
    [string] $AuditManifestPath,
    [string] $ExpectedAuditManifestSha256,
    [ValidateSet("development", "production")] [string] $Channel = "production",
    [switch] $AllowUnsignedDevelopmentFixture
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
Import-Module (
    Join-Path (Split-Path -Parent $PSScriptRoot) "installer\DbBackup.Install.Common.psm1"
) -Force

$maximumFileCount = 4096
$maximumFileBytes = 512MB
$maximumTreeBytes = 2GB
$nativeRoot = Resolve-DbBackupAbsolutePath -Path $NativeToolRoot
$package = Resolve-DbBackupAbsolutePath -Path $PackageRoot -AllowMissing
$nativeRootMetadata = Get-Item -LiteralPath $nativeRoot -Force
if (-not $nativeRootMetadata.PSIsContainer -or
    $nativeRootMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "NATIVE_TOOL_REPARSE_POINT_BLOCKED"
}
if ($Channel -eq "development" -and -not $AllowUnsignedDevelopmentFixture) {
    throw "NATIVE_TOOL_DEVELOPMENT_EXCEPTION_REQUIRED"
}
if ($Channel -eq "production" -and
    $ExpectedAuditManifestSha256 -notmatch "^[0-9a-fA-F]{64}$") {
    throw "NATIVE_TOOL_AUDIT_DIGEST_REQUIRED"
}

$auditPath = if ([string]::IsNullOrWhiteSpace($AuditManifestPath)) {
    Join-Path $nativeRoot "native-tools-audit.json"
}
else {
    Resolve-DbBackupAbsolutePath -Path $AuditManifestPath
}
$auditPath = Assert-DbBackupChildPath -Root $nativeRoot -Candidate $auditPath
if (-not (Test-Path -LiteralPath $auditPath -PathType Leaf)) {
    throw "NATIVE_TOOL_AUDIT_MISSING"
}
$auditMetadata = Get-Item -LiteralPath $auditPath -Force
if ($auditMetadata.Length -le 0 -or
    $auditMetadata.Length -gt 2MB -or
    $auditMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "NATIVE_TOOL_AUDIT_INVALID"
}
$auditHash = (
    Get-FileHash -LiteralPath $auditPath -Algorithm SHA256
).Hash.ToLowerInvariant()
if (-not [string]::IsNullOrWhiteSpace($ExpectedAuditManifestSha256) -and
    $auditHash -ne $ExpectedAuditManifestSha256.ToLowerInvariant()) {
    throw "NATIVE_TOOL_AUDIT_DIGEST_MISMATCH"
}
try {
    $audit = Get-Content -LiteralPath $auditPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
}
catch {
    throw "NATIVE_TOOL_AUDIT_INVALID"
}
foreach ($requiredProperty in @(
        "schemaVersion", "auditId", "approvedAt", "bundles", "files"
    )) {
    if ($null -eq $audit.PSObject.Properties[$requiredProperty]) {
        throw "NATIVE_TOOL_AUDIT_INVALID"
    }
}
if ($audit.schemaVersion -ne 1 -or
    [string]::IsNullOrWhiteSpace([string] $audit.auditId) -or
    [string]::IsNullOrWhiteSpace([string] $audit.approvedAt) -or
    @($audit.bundles).Count -ne 3 -or
    @($audit.files).Count -eq 0 -or
    @($audit.files).Count -gt $maximumFileCount) {
    throw "NATIVE_TOOL_AUDIT_INVALID"
}
$approvedAt = [DateTimeOffset]::MinValue
if (-not [DateTimeOffset]::TryParse([string] $audit.approvedAt, [ref] $approvedAt)) {
    throw "NATIVE_TOOL_AUDIT_INVALID"
}

function ConvertTo-NativeRelativePath {
    param([Parameter(Mandatory = $true)] [string] $Path)

    $normalized = $Path.Replace("\", "/")
    $segments = @($normalized.Split("/"))
    if ([string]::IsNullOrWhiteSpace($normalized) -or
        $normalized.Length -gt 512 -or
        $normalized.IndexOf([char]0) -ge 0 -or
        [IO.Path]::IsPathRooted($normalized) -or
        $normalized.Contains(":") -or
        $segments.Count -lt 2 -or
        @($segments | Where-Object { $_ -in @("", ".", "..") }).Count -ne 0 -or
        $segments[0] -notin @("pg", "mysql")) {
        throw "NATIVE_TOOL_PATH_INVALID"
    }
    return $normalized
}

$fileByPath = @{}
foreach ($entry in @($audit.files)) {
    foreach ($requiredProperty in @(
            "path", "sha256", "role", "authenticodeRequired",
            "signerThumbprint", "publisher"
        )) {
        if ($null -eq $entry.PSObject.Properties[$requiredProperty]) {
            throw "NATIVE_TOOL_FILE_ENTRY_INVALID"
        }
    }
    $relative = ConvertTo-NativeRelativePath -Path ([string] $entry.path)
    if ($fileByPath.ContainsKey($relative) -or
        ([string] $entry.sha256) -notmatch "^[0-9a-fA-F]{64}$" -or
        ([string] $entry.role) -notin @(
            "executable", "dependency", "license", "resource"
        ) -or
        -not ($entry.authenticodeRequired -is [bool]) -or
        [string]::IsNullOrWhiteSpace([string] $entry.publisher)) {
        throw "NATIVE_TOOL_FILE_ENTRY_INVALID"
    }
    $extension = [IO.Path]::GetExtension($relative).ToLowerInvariant()
    $isEntryPoint = $extension -in @(".exe", ".msi")
    $isAuthenticodeCapable = $extension -in @(".exe", ".dll", ".msi")
    if ($Channel -eq "production" -and $isEntryPoint -and
        $entry.authenticodeRequired -ne $true) {
        throw "NATIVE_TOOL_SIGNATURE_POLICY_INVALID"
    }
    if ($Channel -eq "production" -and
        $entry.authenticodeRequired -eq $true -and
        (-not $isAuthenticodeCapable -or
            ([string] $entry.signerThumbprint) -notmatch (
                "^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$"
            ))) {
        throw "NATIVE_TOOL_SIGNATURE_POLICY_INVALID"
    }
    $fileByPath[$relative] = $entry
}

$requiredContracts = [ordered] @{
    "postgresql-18" = [ordered] @{
        root = "pg/pg18"
        major = 18
        executables = @(
            "pg/pg18/pg_dump.exe",
            "pg/pg18/pg_dumpall.exe",
            "pg/pg18/pg_restore.exe",
            "pg/pg18/psql.exe"
        )
    }
    "mysql-8" = [ordered] @{
        root = "mysql/mysql8"
        major = 8
        executables = @(
            "mysql/mysql8/mysqldump.exe",
            "mysql/mysql8/mysql.exe"
        )
    }
    "mariadb-11" = [ordered] @{
        root = "mysql/mariadb11"
        major = 11
        executables = @(
            "mysql/mariadb11/mariadb-dump.exe",
            "mysql/mariadb11/mariadb.exe"
        )
    }
}
$bundleById = @{}
foreach ($bundle in @($audit.bundles)) {
    foreach ($requiredProperty in @(
            "id", "root", "major", "requiredExecutables", "dependencies", "licenses"
        )) {
        if ($null -eq $bundle.PSObject.Properties[$requiredProperty]) {
            throw "NATIVE_TOOL_BUNDLE_INVALID"
        }
    }
    $id = [string] $bundle.id
    if (-not $requiredContracts.Contains($id) -or $bundleById.ContainsKey($id)) {
        throw "NATIVE_TOOL_BUNDLE_INVALID"
    }
    $contract = $requiredContracts[$id]
    if (([string] $bundle.root).Replace("\", "/") -cne $contract.root -or
        [int] $bundle.major -ne $contract.major -or
        @($bundle.dependencies).Count -eq 0 -or
        @($bundle.licenses).Count -eq 0) {
        throw "NATIVE_TOOL_BUNDLE_INVALID"
    }
    $requiredExecutables = @(
        $bundle.requiredExecutables |
            ForEach-Object { ConvertTo-NativeRelativePath -Path ([string] $_) }
    )
    foreach ($declaredExecutable in $requiredExecutables) {
        if (-not $declaredExecutable.StartsWith(
                $contract.root + "/",
                [StringComparison]::Ordinal
            ) -or
            -not $fileByPath.ContainsKey($declaredExecutable) -or
            [string] $fileByPath[$declaredExecutable].role -ne "executable") {
            throw "NATIVE_TOOL_REQUIRED_EXECUTABLE_MISSING"
        }
    }
    foreach ($required in @($contract.executables)) {
        if ($required -notin $requiredExecutables -or
            -not $fileByPath.ContainsKey($required) -or
            [string] $fileByPath[$required].role -ne "executable") {
            throw "NATIVE_TOOL_REQUIRED_EXECUTABLE_MISSING"
        }
    }
    foreach ($dependency in @($bundle.dependencies)) {
        $relative = ConvertTo-NativeRelativePath -Path ([string] $dependency)
        if (-not $relative.StartsWith($contract.root + "/", [StringComparison]::Ordinal) -or
            -not $fileByPath.ContainsKey($relative) -or
            [string] $fileByPath[$relative].role -ne "dependency") {
            throw "NATIVE_TOOL_DEPENDENCY_INVALID"
        }
    }
    foreach ($license in @($bundle.licenses)) {
        $relative = ConvertTo-NativeRelativePath -Path ([string] $license)
        if (-not $relative.StartsWith($contract.root + "/", [StringComparison]::Ordinal) -or
            -not $fileByPath.ContainsKey($relative) -or
            [string] $fileByPath[$relative].role -ne "license") {
            throw "NATIVE_TOOL_LICENSE_INVALID"
        }
    }
    $bundleById[$id] = $bundle
}
if ($bundleById.Count -ne $requiredContracts.Count) {
    throw "NATIVE_TOOL_BUNDLE_INVALID"
}

$allItems = @(
    Get-ChildItem -LiteralPath $nativeRoot -Recurse -Force -ErrorAction Stop
)
if (@($allItems | Where-Object {
            $_.Attributes -band [IO.FileAttributes]::ReparsePoint
        }).Count -ne 0) {
    throw "NATIVE_TOOL_REPARSE_POINT_BLOCKED"
}
$actualFiles = @(
    $allItems |
        Where-Object { -not $_.PSIsContainer -and $_.FullName -ne $auditPath }
)
if ($actualFiles.Count -gt $maximumFileCount) {
    throw "NATIVE_TOOL_TREE_LIMIT_EXCEEDED"
}
$actualPaths = [Collections.Generic.HashSet[string]]::new(
    [StringComparer]::Ordinal
)
[int64] $treeBytes = 0
foreach ($file in $actualFiles) {
    $relative = $file.FullName.Substring($nativeRoot.Length).
        TrimStart("\", "/").Replace("\", "/")
    $relative = ConvertTo-NativeRelativePath -Path $relative
    if (-not $fileByPath.ContainsKey($relative)) {
        throw "NATIVE_TOOL_UNAUDITED_FILE"
    }
    if ($file.Length -gt $maximumFileBytes) {
        throw "NATIVE_TOOL_TREE_LIMIT_EXCEEDED"
    }
    $treeBytes += $file.Length
    if ($treeBytes -gt $maximumTreeBytes) {
        throw "NATIVE_TOOL_TREE_LIMIT_EXCEEDED"
    }
    $null = $actualPaths.Add($relative)
}
foreach ($relative in $fileByPath.Keys) {
    if (-not $actualPaths.Contains($relative)) {
        throw "NATIVE_TOOL_FILE_MISSING"
    }
    $path = Assert-DbBackupChildPath -Root $nativeRoot -Candidate (
        Join-Path $nativeRoot $relative
    )
    $actualHash = (
        Get-FileHash -LiteralPath $path -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    $entry = $fileByPath[$relative]
    if ($actualHash -ne ([string] $entry.sha256).ToLowerInvariant()) {
        throw "NATIVE_TOOL_HASH_MISMATCH"
    }
    if ($entry.authenticodeRequired -eq $true) {
        $signature = Get-AuthenticodeSignature -LiteralPath $path
        $expectedThumbprint = ([string] $entry.signerThumbprint).
            Replace(" ", "").ToUpperInvariant()
        if ($signature.Status -ne [Management.Automation.SignatureStatus]::Valid -or
            $null -eq $signature.SignerCertificate -or
            $signature.SignerCertificate.Thumbprint.Replace(
                " ", ""
            ).ToUpperInvariant() -ne $expectedThumbprint) {
            throw "NATIVE_TOOL_AUTHENTICODE_INVALID"
        }
        if (-not $signature.SignerCertificate.Subject.Contains(
                [string] $entry.publisher,
                [StringComparison]::OrdinalIgnoreCase
            )) {
            throw "NATIVE_TOOL_PUBLISHER_MISMATCH"
        }
    }
}

if ($Channel -eq "production") {
    $versionPatterns = [ordered] @{
        "pg/pg18/pg_dump.exe" = "\(PostgreSQL\)\s+18\."
        "pg/pg18/pg_dumpall.exe" = "\(PostgreSQL\)\s+18\."
        "pg/pg18/pg_restore.exe" = "\(PostgreSQL\)\s+18\."
        "pg/pg18/psql.exe" = "\(PostgreSQL\)\s+18\."
        "mysql/mysql8/mysqldump.exe" = "(?i)\b(?:Distrib\s+)?8\."
        "mysql/mysql8/mysql.exe" = "(?i)\b(?:Distrib\s+)?8\."
        "mysql/mariadb11/mariadb-dump.exe" = "(?i)Distrib\s+11\."
        "mysql/mariadb11/mariadb.exe" = "(?i)Distrib\s+11\."
    }
    foreach ($relative in $versionPatterns.Keys) {
        $path = Assert-DbBackupChildPath -Root $nativeRoot -Candidate (
            Join-Path $nativeRoot $relative
        )
        try {
            $lines = @(& $path --version 2>&1)
        }
        catch {
            throw "NATIVE_TOOL_VERSION_PROBE_FAILED"
        }
        if ($LASTEXITCODE -ne 0 -or
            ($lines -join "`n") -notmatch $versionPatterns[$relative]) {
            throw "NATIVE_TOOL_VERSION_PROBE_FAILED"
        }
    }
}

$toolsTarget = Join-Path $package "tools"
if (Test-Path -LiteralPath $toolsTarget) {
    throw "NATIVE_TOOL_PACKAGE_TARGET_NOT_EMPTY"
}
New-Item -ItemType Directory -Path $package -Force | Out-Null
$temporary = Join-Path $package (
    ".native-tools-staging-" + [Guid]::NewGuid().ToString("N")
)
try {
    New-Item -ItemType Directory -Path $temporary -Force | Out-Null
    foreach ($relative in $fileByPath.Keys) {
        $source = Assert-DbBackupChildPath -Root $nativeRoot -Candidate (
            Join-Path $nativeRoot $relative
        )
        $destination = Assert-DbBackupChildPath -Root $temporary -Candidate (
            Join-Path $temporary $relative
        )
        New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force |
            Out-Null
        Copy-Item -LiteralPath $source -Destination $destination
        if ((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ne
            ([string] $fileByPath[$relative].sha256).ToUpperInvariant()) {
            throw "NATIVE_TOOL_STAGED_HASH_MISMATCH"
        }
    }
    $stagedAuditPath = Join-Path $temporary "native-tools-audit.json"
    Copy-Item -LiteralPath $auditPath -Destination $stagedAuditPath
    $stagedAuditHash = (
        Get-FileHash -LiteralPath $stagedAuditPath -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    if ($stagedAuditHash -ne $auditHash) {
        throw "NATIVE_TOOL_AUDIT_DIGEST_MISMATCH"
    }
    Move-Item -LiteralPath $temporary -Destination $toolsTarget
}
finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
}

[ordered] @{
    schemaVersion = 1
    status = "staged"
    channel = $Channel
    auditId = [string] $audit.auditId
    auditManifestSha256 = $auditHash
    fileCount = $fileByPath.Count
    totalBytes = $treeBytes
    toolsPath = [IO.Path]::GetFullPath($toolsTarget)
} | ConvertTo-Json -Depth 8
