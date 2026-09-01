[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $OutputRoot,
    [switch] $ValidateOnly,
    [switch] $RunLiveSqlServer,
    [string] $SqlServerName,
    [string] $SqlPackagePath,
    [string] $SqlPackageVersion,
    [string] $SqlPackageSha256,
    [string] $SqlPackagePublisher = "Microsoft Corporation"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding

if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
$resolvedRoot = [IO.Path]::GetFullPath($RepositoryRoot)
if (-not (Test-Path -LiteralPath $resolvedRoot -PathType Container)) {
    throw "repository root does not exist"
}
$resolvedOutput = if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    Join-Path $resolvedRoot "artifacts\g4"
}
elseif ([IO.Path]::IsPathRooted($OutputRoot)) {
    [IO.Path]::GetFullPath($OutputRoot)
}
else {
    [IO.Path]::GetFullPath((Join-Path $resolvedRoot $OutputRoot))
}
New-Item -ItemType Directory -Path $resolvedOutput -Force | Out-Null

$requiredCapabilities = @(
    "probe",
    "discovery",
    "tool_preflight",
    "backup",
    "verify_l1",
    "verify_l2",
    "restore_create_new",
    "validation",
    "restore_l3"
)
$requiredChecks = @(
    "sqlite-provider-conformance",
    "sqlserver-candidate-contract",
    "sqlite-service-integration",
    "engine-matrix-projection",
    "engine-matrix-ui",
    "engine-matrix-browser"
)

function Write-Utf8Json {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [object] $Value
    )

    $json = $Value | ConvertTo-Json -Depth 32
    [IO.File]::WriteAllText($Path, $json, [Text.UTF8Encoding]::new($false))
}

function Get-Sha256Text {
    param([AllowEmptyString()] [string] $Value)

    $bytes = [Text.Encoding]::UTF8.GetBytes($Value)
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
    }
}

function Get-FileSha256 {
    param([Parameter(Mandatory = $true)] [string] $Path)

    $stream = [IO.File]::OpenRead($Path)
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
        $stream.Dispose()
    }
}

function Get-GitLines {
    param([Parameter(Mandatory = $true)] [string[]] $Arguments)

    $lines = @(& git -C $resolvedRoot @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "git identity query failed"
    }
    return @($lines | ForEach-Object { $_.ToString() })
}

function Get-SourceTreeDigest {
    $paths = @(
        Get-GitLines -Arguments @(
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard"
        )
    ) |
        Where-Object {
            -not [string]::IsNullOrWhiteSpace($_) -and
            -not $_.Replace("\", "/").StartsWith(
                "artifacts/",
                [StringComparison]::OrdinalIgnoreCase
            ) -and
            -not $_.Replace("\", "/").StartsWith(
                "docs/chronos/",
                [StringComparison]::OrdinalIgnoreCase
            )
        } |
        Sort-Object -Unique
    $manifest = [Text.StringBuilder]::new()
    foreach ($relative in $paths) {
        $absolute = Join-Path $resolvedRoot $relative
        if (Test-Path -LiteralPath $absolute -PathType Leaf) {
            [void] $manifest.Append($relative.Replace("\", "/"))
            [void] $manifest.Append("`t")
            [void] $manifest.Append((Get-FileSha256 -Path $absolute))
            [void] $manifest.Append("`n")
        }
    }
    return Get-Sha256Text -Value $manifest.ToString()
}

function Invoke-EvidenceCommand {
    param(
        [Parameter(Mandatory = $true)] [string] $Id,
        [Parameter(Mandatory = $true)] [string] $Executable,
        [Parameter(Mandatory = $true)] [string[]] $Arguments
    )

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        $startedAt = [DateTimeOffset]::Now
        $lines = @(& $Executable @Arguments 2>&1)
        $exitCode = [int] $LASTEXITCODE
        $finishedAt = [DateTimeOffset]::Now
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    $output = @($lines | ForEach-Object { $_.ToString() }) -join "`n"
    if ($exitCode -ne 0) {
        $tail = @($lines | Select-Object -Last 20 | ForEach-Object { $_.ToString() }) -join "`n"
        [Console]::Error.WriteLine("G4 check $Id failed: $tail")
    }
    return [PSCustomObject] @{
        id = $Id
        command = "$Executable " + ($Arguments -join " ")
        startedAt = $startedAt.ToString("o")
        finishedAt = $finishedAt.ToString("o")
        exitCode = $exitCode
        status = if ($exitCode -eq 0) { "pass" } else { "fail" }
        outputSha256 = Get-Sha256Text -Value $output
    }
}

function New-CapabilityRows {
    param(
        [Parameter(Mandatory = $true)] [string] $Outcome,
        [Parameter(Mandatory = $true)] [string] $EvidenceLocator,
        [string] $Limitation
    )

    return @(
        $requiredCapabilities | ForEach-Object {
            [PSCustomObject] @{
                capability = $_
                outcome = $Outcome
                evidenceLocator = $EvidenceLocator
                limitation = if ([string]::IsNullOrWhiteSpace($Limitation)) {
                    $null
                }
                else {
                    $Limitation
                }
            }
        }
    )
}

function Open-SqlConnection {
    param(
        [Parameter(Mandatory = $true)] [string] $Server,
        [Parameter(Mandatory = $true)] [string] $Database
    )

    $builder = [System.Data.SqlClient.SqlConnectionStringBuilder]::new()
    $builder["Data Source"] = $Server
    $builder["Initial Catalog"] = $Database
    $builder["Integrated Security"] = $true
    $builder["Encrypt"] = $true
    $builder["TrustServerCertificate"] = $false
    $builder["Connect Timeout"] = 30
    $builder["Application Name"] = "RestorePlane G4 certification"
    $connection = [System.Data.SqlClient.SqlConnection]::new($builder.ConnectionString)
    $connection.Open()
    return $connection
}

function Invoke-SqlNonQuery {
    param(
        [Parameter(Mandatory = $true)] [string] $Server,
        [Parameter(Mandatory = $true)] [string] $Database,
        [Parameter(Mandatory = $true)] [string] $Sql
    )

    $connection = Open-SqlConnection -Server $Server -Database $Database
    try {
        $command = $connection.CreateCommand()
        $command.CommandTimeout = 120
        $command.CommandText = $Sql
        [void] $command.ExecuteNonQuery()
    }
    finally {
        $connection.Dispose()
    }
}

function Get-SqlFixtureOracle {
    param(
        [Parameter(Mandatory = $true)] [string] $Server,
        [Parameter(Mandatory = $true)] [string] $Database
    )

    $connection = Open-SqlConnection -Server $Server -Database $Database
    try {
        $command = $connection.CreateCommand()
        $command.CommandTimeout = 120
        $command.CommandText = @"
SELECT fixture_id, fixture_key, CONVERT(varchar(max), payload, 2),
       COALESCE(note, N'<NULL>')
FROM dbo.restoreplane_wave1_fixture
ORDER BY fixture_id;
"@
        $reader = $command.ExecuteReader()
        $rows = [System.Collections.Generic.List[string]]::new()
        try {
            while ($reader.Read()) {
                $rows.Add(
                    "{0}`t{1}`t{2}`t{3}" -f
                    $reader.GetInt32(0),
                    $reader.GetString(1),
                    $reader.GetString(2),
                    $reader.GetString(3)
                )
            }
        }
        finally {
            $reader.Dispose()
        }
        return $rows -join "`n"
    }
    finally {
        $connection.Dispose()
    }
}

function Test-LiveSqlServerRoundTrip {
    param(
        [Parameter(Mandatory = $true)] [string] $Server,
        [Parameter(Mandatory = $true)] [string] $ToolPath,
        [Parameter(Mandatory = $true)] [string] $ExpectedVersion,
        [Parameter(Mandatory = $true)] [string] $ExpectedSha256,
        [Parameter(Mandatory = $true)] [string] $ExpectedPublisher
    )

    $resolvedTool = [IO.Path]::GetFullPath($ToolPath)
    if (-not (Test-Path -LiteralPath $resolvedTool -PathType Leaf)) {
        throw "SqlPackage executable does not exist"
    }
    if ($ExpectedSha256 -notmatch "^[0-9a-fA-F]{64}$") {
        throw "SqlPackage SHA-256 policy is invalid"
    }
    $actualSha256 = Get-FileSha256 -Path $resolvedTool
    if ($actualSha256 -ne $ExpectedSha256.ToLowerInvariant()) {
        throw "SqlPackage SHA-256 does not match policy"
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $resolvedTool
    if ($signature.Status -ne "Valid" -or
        $null -eq $signature.SignerCertificate -or
        -not $signature.SignerCertificate.Subject.Contains($ExpectedPublisher)) {
        throw "SqlPackage Authenticode publisher does not match policy"
    }
    $versionLines = @(& $resolvedTool "/Version" 2>&1)
    if ($LASTEXITCODE -ne 0 -or
        (@($versionLines | ForEach-Object { $_.ToString() }) -join "`n") -notmatch
        [Regex]::Escape($ExpectedVersion)) {
        throw "SqlPackage version does not match policy"
    }

    $suffix = "{0:x}-{1:x}" -f $PID, [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $sourceDatabase = "restoreplane_g4_source_$suffix"
    $restoreDatabase = "restoreplane_g4_restore_$suffix"
    $bacpacPath = Join-Path $resolvedOutput "$sourceDatabase.bacpac"
    if (Test-Path -LiteralPath $bacpacPath) {
        throw "G4 BACPAC output already exists"
    }
    $sourceCreated = $false
    $restoreCreated = $false
    try {
        Invoke-SqlNonQuery -Server $Server -Database "master" -Sql (
            "CREATE DATABASE [$sourceDatabase];"
        )
        $sourceCreated = $true
        Invoke-SqlNonQuery -Server $Server -Database $sourceDatabase -Sql @"
CREATE TABLE dbo.restoreplane_wave1_fixture (
    fixture_id int NOT NULL PRIMARY KEY,
    fixture_key nvarchar(128) NOT NULL,
    payload varbinary(max) NOT NULL,
    note nvarchar(400) NULL
);
INSERT dbo.restoreplane_wave1_fixture(fixture_id, fixture_key, payload, note)
VALUES
    (1, N'한글-quote-''-line', 0x000102FEFF, N'BACPAC roundtrip'),
    (2, N'empty-note', 0x, N''),
    (3, N'null-note', 0xDEADBEEF, NULL);
"@
        $sourceOracle = Get-SqlFixtureOracle -Server $Server -Database $sourceDatabase
        $export = Invoke-EvidenceCommand `
            -Id "sqlserver-bacpac-export" `
            -Executable $resolvedTool `
            -Arguments @(
                "/Action:Export",
                "/SourceServerName:$Server",
                "/SourceDatabaseName:$sourceDatabase",
                "/TargetFile:$bacpacPath",
                "/SourceEncryptConnection:True",
                "/SourceTrustServerCertificate:False",
                "/SourceTimeout:300",
                "/MaxParallelism:4",
                "/OverwriteFiles:False",
                "/Quiet:True"
            )
        if ($export.exitCode -ne 0 -or
            -not (Test-Path -LiteralPath $bacpacPath -PathType Leaf)) {
            throw "SqlPackage export did not produce a BACPAC"
        }
        $import = Invoke-EvidenceCommand `
            -Id "sqlserver-bacpac-import" `
            -Executable $resolvedTool `
            -Arguments @(
                "/Action:Import",
                "/SourceFile:$bacpacPath",
                "/TargetServerName:$Server",
                "/TargetDatabaseName:$restoreDatabase",
                "/TargetEncryptConnection:True",
                "/TargetTrustServerCertificate:False",
                "/TargetTimeout:300",
                "/MaxParallelism:4",
                "/Quiet:True"
            )
        if ($import.exitCode -ne 0) {
            throw "SqlPackage import failed"
        }
        $restoreCreated = $true
        $restoredOracle = Get-SqlFixtureOracle -Server $Server -Database $restoreDatabase
        if ($sourceOracle -cne $restoredOracle) {
            throw "SQL Server BACPAC content oracle mismatch"
        }
        return [PSCustomObject] @{
            status = "pass"
            sourceDatabase = $sourceDatabase
            restoreDatabase = $restoreDatabase
            sourceOracleSha256 = Get-Sha256Text -Value $sourceOracle
            restoredOracleSha256 = Get-Sha256Text -Value $restoredOracle
            bacpacSha256 = Get-FileSha256 -Path $bacpacPath
            bacpacBytes = (Get-Item -LiteralPath $bacpacPath).Length
            export = $export
            import = $import
            tool = [PSCustomObject] @{
                version = $ExpectedVersion
                sha256 = $actualSha256
                publisher = $ExpectedPublisher
                signatureStatus = "valid"
            }
        }
    }
    finally {
        if ($restoreCreated) {
            Invoke-SqlNonQuery -Server $Server -Database "master" -Sql (
                "ALTER DATABASE [$restoreDatabase] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; " +
                "DROP DATABASE [$restoreDatabase];"
            )
        }
        if ($sourceCreated) {
            Invoke-SqlNonQuery -Server $Server -Database "master" -Sql (
                "ALTER DATABASE [$sourceDatabase] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; " +
                "DROP DATABASE [$sourceDatabase];"
            )
        }
    }
}

$contract = [PSCustomObject] @{
    schemaVersion = 1
    gateId = "G4"
    engines = @("sqlite", "sqlserver")
    requiredCapabilities = $requiredCapabilities
    requiredChecks = $requiredChecks
    sqlServer = [PSCustomObject] @{
        artifactKind = "bacpac"
        dacpacQualifiesAsDataBackup = $false
        authentication = "windows_integrated"
        liveRoundTripRequiresExplicitSwitch = $true
    }
    sqlite = [PSCustomObject] @{
        backupApi = "sqlite_online_backup"
        createNewDefault = $true
        overwritePolicy = "stage_validate_fsync_identity_recheck_atomic_replace"
    }
}
Write-Utf8Json -Path (Join-Path $resolvedOutput "contract.json") -Value $contract
if ($ValidateOnly) {
    Write-Output "G4 engine-wave contract valid"
    exit 0
}

$revisionLines = @(Get-GitLines -Arguments @("rev-parse", "HEAD"))
if ($revisionLines.Count -ne 1) {
    throw "git revision identity is ambiguous"
}
$sourceRevision = $revisionLines[0].Trim()
$sourceTreeDigest = Get-SourceTreeDigest
$runnerOs = [Environment]::OSVersion.VersionString
$rustcLines = @(& rustc --version 2>&1)
$cargoLines = @(& cargo --version 2>&1)
$nodeLines = @(& node --version 2>&1)
$buildId = Get-Sha256Text -Value (
    "$sourceRevision`n$sourceTreeDigest`n$($rustcLines[0])`n" +
    "$($cargoLines[0])`n$($nodeLines[0])`n$runnerOs"
)
$identity = [PSCustomObject] @{
    sourceRevision = $sourceRevision
    sourceTreeDigest = $sourceTreeDigest
    buildId = $buildId
    runnerOs = $runnerOs
    runnerArchitecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
}

Push-Location $resolvedRoot
try {
    $checkRows = [System.Collections.Generic.List[object]]::new()
    $checkRows.Add(
        (Invoke-EvidenceCommand `
            -Id "sqlite-provider-conformance" `
            -Executable "cargo" `
            -Arguments @("test", "-p", "restoreplane-source-sqlite", "--test", "conformance"))
    )
    $checkRows.Add(
        (Invoke-EvidenceCommand `
            -Id "sqlserver-candidate-contract" `
            -Executable "cargo" `
            -Arguments @("test", "-p", "restoreplane-source-sqlserver", "--test", "candidate_contract"))
    )
    $checkRows.Add(
        (Invoke-EvidenceCommand `
            -Id "sqlite-service-integration" `
            -Executable "cargo" `
            -Arguments @(
                "test",
                "-p",
                "restoreplane-service",
                "--test",
                "connection_operation_worker",
                "native_sqlite"
            ))
    )
    $checkRows.Add(
        (Invoke-EvidenceCommand `
            -Id "engine-matrix-projection" `
            -Executable "cargo" `
            -Arguments @(
                "test",
                "-p",
                "restoreplane-catalog",
                "--test",
                "certification_repository",
                "engine_matrix"
            ))
    )
    $checkRows.Add(
        (Invoke-EvidenceCommand `
            -Id "engine-matrix-ui" `
            -Executable "npm" `
            -Arguments @(
                "--prefix",
                "apps/console",
                "test",
                "--",
                "--run",
                "src/components/BackupPlanWizard.test.ts"
            ))
    )
    $checkRows.Add(
        (Invoke-EvidenceCommand `
            -Id "engine-matrix-browser" `
            -Executable "npm" `
            -Arguments @(
                "--prefix",
                "apps/console",
                "run",
                "test:e2e",
                "--",
                "--grep",
                "Plan preflight blocks SQL Server"
            ))
    )
    $checks = $checkRows.ToArray()
}
finally {
    Pop-Location
}
$failureCount = @($checks | Where-Object { $_.exitCode -ne 0 }).Count
$sqliteCheckIds = @(
    "sqlite-provider-conformance",
    "sqlite-service-integration",
    "engine-matrix-projection",
    "engine-matrix-ui",
    "engine-matrix-browser"
)
$sqliteFailureCount = @(
    $checks |
        Where-Object {
            $sqliteCheckIds -contains $_.id -and $_.exitCode -ne 0
        }
).Count
$sqliteStatus = if ($sqliteFailureCount -eq 0) { "supported" } else { "candidate" }
$sqliteEvidence = [PSCustomObject] @{
    schemaVersion = 1
    gateId = "G4"
    engine = "sqlite"
    supportStatus = $sqliteStatus
    identity = $identity
    checks = @($checks | Where-Object { $_.id -ne "sqlserver-candidate-contract" })
    capabilities = New-CapabilityRows `
        -Outcome $(if ($sqliteFailureCount -eq 0) { "passed" } else { "failed" }) `
        -EvidenceLocator "artifacts/g4/sqlite.json"
    limitations = @(
        "local regular SQLite files only",
        "encrypted SQLite variants require a separately certified provider"
    )
}
Write-Utf8Json -Path (Join-Path $resolvedOutput "sqlite.json") -Value $sqliteEvidence

$liveSqlEvidence = $null
$sqlServerStatus = "candidate"
$sqlServerReason = "SQLSERVER_LIVE_FIXTURE_NOT_REQUESTED"
if ($RunLiveSqlServer) {
    foreach ($value in @(
        $SqlServerName,
        $SqlPackagePath,
        $SqlPackageVersion,
        $SqlPackageSha256,
        $SqlPackagePublisher
    )) {
        if ([string]::IsNullOrWhiteSpace($value)) {
            throw "live SQL Server certification requires server and exact SqlPackage identity policy"
        }
    }
    $liveSqlEvidence = Test-LiveSqlServerRoundTrip `
        -Server $SqlServerName `
        -ToolPath $SqlPackagePath `
        -ExpectedVersion $SqlPackageVersion `
        -ExpectedSha256 $SqlPackageSha256 `
        -ExpectedPublisher $SqlPackagePublisher
    $sqlServerReason = "SQLSERVER_LIVE_BACPAC_ROUNDTRIP_PASSED_CANDIDATE_REVIEW_REQUIRED"
}
$sqlCandidatePassed = @(
    $checks |
        Where-Object {
            $_.id -eq "sqlserver-candidate-contract" -and $_.exitCode -eq 0
        }
).Count -eq 1
$sqlLiveCapabilities = @(
    "backup",
    "verify_l1",
    "verify_l2",
    "restore_create_new",
    "validation",
    "restore_l3"
)
$sqlServerCapabilities = @(
    $requiredCapabilities | ForEach-Object {
        $outcome = if ($_ -eq "tool_preflight" -and $sqlCandidatePassed) {
            "passed"
        }
        elseif ($null -ne $liveSqlEvidence -and $sqlLiveCapabilities -contains $_) {
            "passed"
        }
        else {
            "not_run"
        }
        [PSCustomObject] @{
            capability = $_
            outcome = $outcome
            evidenceLocator = "artifacts/g4/sqlserver.json"
            limitation = if ($outcome -eq "passed") {
                $null
            }
            else {
                "candidate until exact-tool live BACPAC export/import/oracle evidence is reviewed"
            }
        }
    }
)
$sqlServerEvidence = [PSCustomObject] @{
    schemaVersion = 1
    gateId = "G4"
    engine = "sqlserver"
    supportStatus = $sqlServerStatus
    reasonCode = $sqlServerReason
    identity = $identity
    dacpacQualifiesAsDataBackup = $false
    candidateContractCheck = @(
        $checks | Where-Object { $_.id -eq "sqlserver-candidate-contract" }
    )[0]
    liveRoundTrip = $liveSqlEvidence
    capabilities = $sqlServerCapabilities
    limitations = @(
        "server logins and passwords excluded",
        "server roles and SQL Server Agent jobs excluded",
        "server and cross-database permissions require separate recovery",
        "unsupported Azure SQL model surface and encrypted keys are limited",
        "BACPAC is not encrypted before the common encrypted Replica pipeline",
        "transactional consistency requires a quiesced or consistent source copy",
        "integrated authentication only; SQL password argv is unsupported"
    )
}
Write-Utf8Json -Path (Join-Path $resolvedOutput "sqlserver.json") -Value $sqlServerEvidence

$aggregate = [PSCustomObject] @{
    schemaVersion = 1
    gateId = "G4"
    identity = $identity
    startedAt = @($checks)[0].startedAt
    finishedAt = [DateTimeOffset]::Now.ToString("o")
    status = if ($failureCount -eq 0) { "pass" } else { "fail" }
    failureCount = $failureCount
    checks = $checks
    engines = @(
        [PSCustomObject] @{
            engine = "sqlite"
            supportStatus = $sqliteStatus
            evidence = "artifacts/g4/sqlite.json"
        },
        [PSCustomObject] @{
            engine = "sqlserver"
            supportStatus = $sqlServerStatus
            evidence = "artifacts/g4/sqlserver.json"
            reasonCode = $sqlServerReason
        }
    )
}
Write-Utf8Json -Path (Join-Path $resolvedOutput "engine-wave1.json") -Value $aggregate
if ($failureCount -ne 0) {
    throw "G4 engine-wave checks failed: $failureCount"
}
Write-Output "G4 engine-wave checks passed"
Write-Output "sqlite=supported"
Write-Output "sqlserver=$sqlServerStatus"
