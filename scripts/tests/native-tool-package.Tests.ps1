[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$stageScript = Join-Path $repoRoot "scripts\stage-native-tools.ps1"
$manifestScript = Join-Path $repoRoot "scripts\new-package-manifest.ps1"
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "restoreplane-native-tool-contract-" + [Guid]::NewGuid().ToString("N")
)

function Write-FixtureFile {
    param(
        [Parameter(Mandatory = $true)] [string] $Root,
        [Parameter(Mandatory = $true)] [string] $RelativePath
    )

    $path = Join-Path $Root $RelativePath
    New-Item -ItemType Directory -Path (Split-Path -Parent $path) -Force |
        Out-Null
    [IO.File]::WriteAllText(
        $path,
        "fixture:$($RelativePath.Replace('\', '/'))",
        [Text.UTF8Encoding]::new($false)
    )
}

function New-FixtureAudit {
    param(
        [Parameter(Mandatory = $true)] [string] $NativeToolRoot,
        [Parameter(Mandatory = $true)] [string] $AuditPath
    )

    $specifications = @(
        [ordered] @{
            id = "postgresql-18"
            root = "pg/pg18"
            major = 18
            requiredExecutables = @(
                "pg/pg18/pg_dump.exe",
                "pg/pg18/pg_dumpall.exe",
                "pg/pg18/pg_restore.exe",
                "pg/pg18/psql.exe"
            )
            dependencies = @("pg/pg18/libpq.dll")
            licenses = @("pg/pg18/LICENSE")
        },
        [ordered] @{
            id = "mysql-8"
            root = "mysql/mysql8"
            major = 8
            requiredExecutables = @(
                "mysql/mysql8/mysqldump.exe",
                "mysql/mysql8/mysql.exe"
            )
            dependencies = @("mysql/mysql8/libmysql.dll")
            licenses = @("mysql/mysql8/LICENSE")
        },
        [ordered] @{
            id = "mariadb-11"
            root = "mysql/mariadb11"
            major = 11
            requiredExecutables = @(
                "mysql/mariadb11/mariadb-dump.exe",
                "mysql/mariadb11/mariadb.exe"
            )
            dependencies = @("mysql/mariadb11/libmariadb.dll")
            licenses = @("mysql/mariadb11/COPYING")
        }
    )
    $allPaths = @(
        $specifications |
            ForEach-Object {
                @($_.requiredExecutables) + @($_.dependencies) + @($_.licenses)
            }
    )
    foreach ($relative in $allPaths) {
        Write-FixtureFile -Root $NativeToolRoot -RelativePath $relative
    }
    $files = foreach ($relative in $allPaths) {
        $extension = [IO.Path]::GetExtension($relative)
        $role = if ($extension -eq ".exe") {
            "executable"
        }
        elseif ($extension -eq ".dll") {
            "dependency"
        }
        else {
            "license"
        }
        [ordered] @{
            path = $relative
            sha256 = (
                Get-FileHash -LiteralPath (Join-Path $NativeToolRoot $relative) `
                    -Algorithm SHA256
            ).Hash.ToLowerInvariant()
            role = $role
            authenticodeRequired = $false
            signerThumbprint = ""
            publisher = "RestorePlane development fixture"
        }
    }
    $audit = [ordered] @{
        schemaVersion = 1
        auditId = "fixture-audit"
        approvedAt = "2026-08-24T00:00:00Z"
        bundles = $specifications
        files = @($files)
    }
    [IO.File]::WriteAllText(
        $AuditPath,
        ($audit | ConvertTo-Json -Depth 16),
        [Text.UTF8Encoding]::new($false)
    )
    return $audit
}

function Assert-ThrowsCode {
    param(
        [Parameter(Mandatory = $true)] [scriptblock] $Action,
        [Parameter(Mandatory = $true)] [string] $Code
    )

    $blocked = $false
    try {
        & $Action
    }
    catch {
        $blocked = $_.Exception.Message.Contains($Code)
    }
    if (-not $blocked) {
        throw "expected failure code was not observed: $Code"
    }
}

try {
    $nativeRoot = Join-Path $fixtureRoot "audited-native-tools"
    $packageRoot = Join-Path $fixtureRoot "package"
    New-Item -ItemType Directory -Path $nativeRoot -Force | Out-Null
    New-Item -ItemType Directory -Path $packageRoot -Force | Out-Null
    $auditPath = Join-Path $nativeRoot "native-tools-audit.json"
    $audit = New-FixtureAudit -NativeToolRoot $nativeRoot -AuditPath $auditPath
    $auditHash = (
        Get-FileHash -LiteralPath $auditPath -Algorithm SHA256
    ).Hash.ToLowerInvariant()

    Assert-ThrowsCode -Code "NATIVE_TOOL_DEVELOPMENT_EXCEPTION_REQUIRED" -Action {
        & $stageScript `
            -NativeToolRoot $nativeRoot `
            -PackageRoot $packageRoot `
            -Channel development | Out-Null
    }
    Assert-ThrowsCode -Code "NATIVE_TOOL_AUDIT_DIGEST_REQUIRED" -Action {
        & $stageScript `
            -NativeToolRoot $nativeRoot `
            -PackageRoot $packageRoot `
            -Channel production | Out-Null
    }
    Assert-ThrowsCode -Code "NATIVE_TOOL_AUDIT_DIGEST_MISMATCH" -Action {
        & $stageScript `
            -NativeToolRoot $nativeRoot `
            -PackageRoot $packageRoot `
            -ExpectedAuditManifestSha256 ("0" * 64) `
            -Channel production | Out-Null
    }
    Assert-ThrowsCode -Code "NATIVE_TOOL_SIGNATURE_POLICY_INVALID" -Action {
        & $stageScript `
            -NativeToolRoot $nativeRoot `
            -PackageRoot $packageRoot `
            -ExpectedAuditManifestSha256 $auditHash `
            -Channel production | Out-Null
    }

    $vendorPolicyRoot = Join-Path $fixtureRoot "vendor-policy"
    Copy-Item -LiteralPath $nativeRoot -Destination $vendorPolicyRoot -Recurse
    $vendorPolicyAuditPath = Join-Path $vendorPolicyRoot "native-tools-audit.json"
    $vendorPolicyAudit = Get-Content -LiteralPath $vendorPolicyAuditPath `
        -Raw -Encoding UTF8 | ConvertFrom-Json
    foreach ($entry in @($vendorPolicyAudit.files)) {
        if ([IO.Path]::GetExtension([string] $entry.path) -eq ".exe") {
            $entry.authenticodeRequired = $true
            $entry.signerThumbprint = "c" * 40
            $entry.publisher = "Fixture vendor"
        }
    }
    [IO.File]::WriteAllText(
        $vendorPolicyAuditPath,
        ($vendorPolicyAudit | ConvertTo-Json -Depth 16),
        [Text.UTF8Encoding]::new($false)
    )
    $vendorPolicyAuditHash = (
        Get-FileHash -LiteralPath $vendorPolicyAuditPath -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    Assert-ThrowsCode -Code "NATIVE_TOOL_AUTHENTICODE_INVALID" -Action {
        & $stageScript `
            -NativeToolRoot $vendorPolicyRoot `
            -PackageRoot $packageRoot `
            -ExpectedAuditManifestSha256 $vendorPolicyAuditHash `
            -Channel production | Out-Null
    }

    $missingRoot = Join-Path $fixtureRoot "missing-root"
    Assert-ThrowsCode -Code "INSTALL_PATH_NOT_FOUND" -Action {
        & $stageScript `
            -NativeToolRoot $missingRoot `
            -PackageRoot $packageRoot `
            -Channel development `
            -AllowUnsignedDevelopmentFixture | Out-Null
    }

    $barePackageRoot = Join-Path $fixtureRoot "bare-package"
    New-Item -ItemType Directory -Path $barePackageRoot -Force | Out-Null
    Write-FixtureFile -Root $barePackageRoot -RelativePath "restoreplane-service.exe"
    Write-FixtureFile -Root $barePackageRoot -RelativePath "restoreplane.exe"
    Assert-ThrowsCode -Code "PACKAGE_NATIVE_TOOLS_REQUIRED" -Action {
        & $manifestScript `
            -PackageRoot $barePackageRoot `
            -OutputPath (Join-Path $barePackageRoot "package-manifest.json") `
            -Version "0.1.0" `
            -MinimumReaderVersion "0.1.0" `
            -BuildId ("a" * 64) `
            -Revision ("b" * 40) `
            -SourceTreeDigest ("c" * 64) `
            -Publisher "RestorePlane fixture" `
            -Channel production | Out-Null
    }

    $partialRoot = Join-Path $fixtureRoot "partial"
    Copy-Item -LiteralPath $nativeRoot -Destination $partialRoot -Recurse
    Remove-Item -LiteralPath (Join-Path $partialRoot "mysql\mariadb11\mariadb.exe")
    Assert-ThrowsCode -Code "NATIVE_TOOL_FILE_MISSING" -Action {
        & $stageScript `
            -NativeToolRoot $partialRoot `
            -PackageRoot $packageRoot `
            -Channel development `
            -AllowUnsignedDevelopmentFixture | Out-Null
    }

    $extraRoot = Join-Path $fixtureRoot "extra"
    Copy-Item -LiteralPath $nativeRoot -Destination $extraRoot -Recurse
    Write-FixtureFile -Root $extraRoot -RelativePath "pg/pg18/unaudited.dll"
    Assert-ThrowsCode -Code "NATIVE_TOOL_UNAUDITED_FILE" -Action {
        & $stageScript `
            -NativeToolRoot $extraRoot `
            -PackageRoot $packageRoot `
            -Channel development `
            -AllowUnsignedDevelopmentFixture | Out-Null
    }

    $stage = (& $stageScript `
        -NativeToolRoot $nativeRoot `
        -PackageRoot $packageRoot `
        -Channel development `
        -AllowUnsignedDevelopmentFixture) | ConvertFrom-Json
    if ($stage.status -ne "staged" -or
        $stage.auditManifestSha256 -ne $auditHash -or
        $stage.fileCount -ne @($audit.files).Count) {
        throw "native tool staging contract is invalid"
    }
    foreach ($relative in @($audit.files.path)) {
        if (-not (Test-Path -LiteralPath (
                    Join-Path $packageRoot ("tools/" + $relative)
                ) -PathType Leaf)) {
            throw "staging did not preserve native tool relative path: $relative"
        }
    }
    if (-not (Test-Path -LiteralPath (
                Join-Path $packageRoot "tools/native-tools-audit.json"
            ) -PathType Leaf)) {
        throw "staging did not preserve the native tool audit manifest"
    }

    Write-FixtureFile -Root $packageRoot -RelativePath "restoreplane-service.exe"
    Write-FixtureFile -Root $packageRoot -RelativePath "restoreplane.exe"
    $manifestPath = Join-Path $packageRoot "package-manifest.json"
    $manifest = (& $manifestScript `
        -PackageRoot $packageRoot `
        -OutputPath $manifestPath `
        -Version "0.1.0" `
        -MinimumReaderVersion "0.1.0" `
        -BuildId ("a" * 64) `
        -Revision ("b" * 40) `
        -SourceTreeDigest ("c" * 64) `
        -Publisher "RestorePlane development fixture" `
        -Channel development `
        -AllowUnsignedDevelopmentPackage) | ConvertFrom-Json

    $payloadFiles = @(
        Get-ChildItem -LiteralPath $packageRoot -Recurse -File |
            Where-Object { $_.FullName -ne $manifestPath }
    )
    if ($manifest.sourceTreeDigest -ne ("c" * 64) -or
        @($manifest.files).Count -ne $payloadFiles.Count -or
        $manifest.nativeTools.auditManifestSha256 -ne $auditHash -or
        $manifest.nativeTools.fileCount -ne @($audit.files).Count) {
        throw "package manifest did not cover the complete staged native tool tree"
    }
    foreach ($required in @(
            "tools/pg/pg18/pg_dump.exe",
            "tools/pg/pg18/pg_dumpall.exe",
            "tools/pg/pg18/pg_restore.exe",
            "tools/pg/pg18/psql.exe",
            "tools/mysql/mysql8/mysqldump.exe",
            "tools/mysql/mysql8/mysql.exe",
            "tools/mysql/mariadb11/mariadb-dump.exe",
            "tools/mysql/mariadb11/mariadb.exe"
        )) {
        if ($required -notin @($manifest.files.path)) {
            throw "package manifest is missing required native tool: $required"
        }
    }

    $installRoot = Join-Path $fixtureRoot "install"
    $dataRoot = Join-Path $fixtureRoot "data"
    $common = @{
        PackageRoot = $packageRoot
        ManifestPath = $manifestPath
        InstallRoot = $installRoot
        DataRoot = $dataRoot
        ExpectedPackageManifestSha256 = (
            Get-FileHash -LiteralPath $manifestPath -Algorithm SHA256
        ).Hash
        ExpectedNativeToolAuditSha256 = $auditHash
        AllowUnsignedDevelopmentPackage = $true
        ValidateOnly = $true
    }
    foreach ($scriptName in @("install.ps1", "repair.ps1", "update.ps1")) {
        $contract = (& (Join-Path $repoRoot "scripts/$scriptName") @common) |
            ConvertFrom-Json
        if ($contract.nativeToolsIncluded -ne $true -or
            $contract.nativeToolAuditSha256 -ne $auditHash) {
            throw "$scriptName did not expose the native tool install contract"
        }
    }

    Import-Module (
        Join-Path $repoRoot "installer\RestorePlane.Install.Common.psm1"
    ) -Force
    $loaded = Read-RestorePlanePackageManifest -ManifestPath $manifestPath
    Test-RestorePlanePackage `
        -PackageRoot $packageRoot `
        -Manifest $loaded `
        -ExpectedNativeToolAuditSha256 $auditHash `
        -AllowUnsignedDevelopmentPackage
    Copy-RestorePlanePackageFiles `
        -PackageRoot $packageRoot `
        -InstallRoot $installRoot `
        -Manifest $loaded
    foreach ($relative in @($loaded.files.path)) {
        if (-not (Test-Path -LiteralPath (Join-Path $installRoot $relative) `
                -PathType Leaf)) {
            throw "installer copy did not preserve package path: $relative"
        }
    }
    Write-FixtureFile -Root $installRoot `
        -RelativePath "tools/pg/pg17/stale-client.dll"
    Write-FixtureFile -Root $installRoot -RelativePath "rogue-helper.exe"
    $quarantine = Move-RestorePlaneUnmanifestedInstallFiles `
        -InstallRoot $installRoot `
        -DataRoot $dataRoot `
        -Manifest $loaded
    if ($quarantine.quarantinedCount -ne 2 -or
        (Test-Path -LiteralPath (Join-Path $installRoot "rogue-helper.exe")) -or
        (Test-Path -LiteralPath (
                Join-Path $installRoot "tools/pg/pg17/stale-client.dll"
            )) -or
        -not (Test-Path -LiteralPath (
                Join-Path $quarantine.quarantinePath "rogue-helper.exe"
            ) -PathType Leaf)) {
        throw "stale native-tool/install payload was not quarantined"
    }
    Test-RestorePlanePackage `
        -PackageRoot $installRoot `
        -Manifest $loaded `
        -ExpectedNativeToolAuditSha256 $auditHash `
        -AllowUnsignedDevelopmentPackage

    Write-FixtureFile -Root $packageRoot -RelativePath "unmanifested.bin"
    Assert-ThrowsCode -Code "PACKAGE_UNMANIFESTED_FILE" -Action {
        Test-RestorePlanePackage `
            -PackageRoot $packageRoot `
            -Manifest $loaded `
            -ExpectedNativeToolAuditSha256 $auditHash `
            -AllowUnsignedDevelopmentPackage
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

Write-Output "native tool package contract tests passed"
