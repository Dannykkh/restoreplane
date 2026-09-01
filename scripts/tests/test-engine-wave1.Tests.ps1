[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$scriptPath = Join-Path $repoRoot "scripts\test-engine-wave1.ps1"
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "dbbackup-g4-contract-tests-" + [Guid]::NewGuid().ToString("N")
)

try {
    New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = (Get-Process -Id $PID).Path
    $startInfo.Arguments = (
        '-NoProfile -File "{0}" -RepositoryRoot "{1}" -OutputRoot "{2}" -ValidateOnly' -f
            $scriptPath.Replace('"', '\"'),
            $repoRoot.Replace('"', '\"'),
            $fixtureRoot.Replace('"', '\"')
    )
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $process = [Diagnostics.Process]::Start($startInfo)
    try {
        $process.WaitForExit()
        if ($process.ExitCode -ne 0) {
            throw "G4 validate-only contract must exit 0"
        }
    }
    finally {
        $process.Dispose()
    }

    $contractPath = Join-Path $fixtureRoot "contract.json"
    if (-not (Test-Path -LiteralPath $contractPath -PathType Leaf)) {
        throw "G4 validate-only contract did not write contract.json"
    }
    $contract = Get-Content -LiteralPath $contractPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    if ($contract.schemaVersion -ne 1 -or $contract.gateId -ne "G4") {
        throw "G4 contract identity is invalid"
    }
    if ((@($contract.engines) -join ",") -ne "sqlite,sqlserver") {
        throw "G4 must keep SQLite and SQL Server as independent engine rows"
    }
    $expectedCapabilities = @(
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
    if ((@($contract.requiredCapabilities) -join ",") -ne
        ($expectedCapabilities -join ",")) {
        throw "G4 capability names or order do not match the provider contract"
    }
    if ($contract.sqlServer.artifactKind -ne "bacpac" -or
        $contract.sqlServer.dacpacQualifiesAsDataBackup -ne $false -or
        $contract.sqlServer.authentication -ne "windows_integrated" -or
        $contract.sqlServer.liveRoundTripRequiresExplicitSwitch -ne $true) {
        throw "SQL Server candidate safety contract is invalid"
    }
    if ($contract.sqlite.backupApi -ne "sqlite_online_backup" -or
        $contract.sqlite.createNewDefault -ne $true -or
        $contract.sqlite.overwritePolicy -notmatch "identity_recheck") {
        throw "SQLite backup/restore safety contract is invalid"
    }

    $scriptText = Get-Content -LiteralPath $scriptPath -Raw -Encoding UTF8
    foreach ($requiredText in @(
        "SourceTrustServerCertificate:False",
        "TargetTrustServerCertificate:False",
        "Get-AuthenticodeSignature",
        "SQL Server BACPAC content oracle mismatch"
    )) {
        if (-not $scriptText.Contains($requiredText)) {
            throw "G4 live runner is missing '$requiredText'"
        }
    }
    if ($scriptText -match "(?i)password\s*=") {
        throw "G4 SQL Server runner must not accept or construct a password"
    }
}
finally {
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $resolvedFixture = [IO.Path]::GetFullPath($fixtureRoot)
    if ($resolvedFixture.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and
        $resolvedFixture -ne $tempRoot -and
        (Test-Path -LiteralPath $resolvedFixture)) {
        Remove-Item -LiteralPath $resolvedFixture -Recurse -Force
    }
}

Write-Output "test-engine-wave1 contract tests passed"
