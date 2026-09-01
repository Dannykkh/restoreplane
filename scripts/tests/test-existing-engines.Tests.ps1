[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$scriptPath = Join-Path $repoRoot "scripts\test-existing-engines.ps1"
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "dbbackup-g2-contract-tests-" + [Guid]::NewGuid().ToString("N")
)

try {
    New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
    $startInfo = New-Object Diagnostics.ProcessStartInfo
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
            throw "G2 validate-only contract must exit 0"
        }
    }
    finally {
        $process.Dispose()
    }

    $contractPath = Join-Path $fixtureRoot "contract.json"
    if (-not (Test-Path -LiteralPath $contractPath -PathType Leaf)) {
        throw "G2 validate-only contract did not write contract.json"
    }
    $contract = Get-Content -LiteralPath $contractPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($contract.schemaVersion -ne 1 -or $contract.gateId -ne "G2") {
        throw "G2 contract identity is invalid"
    }
    $engines = @($contract.engines)
    if ($engines.Count -ne 3) {
        throw "G2 contract must contain PostgreSQL, MySQL, and MariaDB"
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
    foreach ($engine in $engines) {
        if ($engine.image -notmatch "@sha256:[0-9a-f]{64}$") {
            throw "engine image is not digest pinned: $($engine.engine)"
        }
        if (@($engine.requiredCapabilities).Count -ne 9) {
            throw "engine capability contract must contain exactly nine rows"
        }
        if ((@($engine.requiredCapabilities) -join ",") -ne ($expectedCapabilities -join ",")) {
            throw "engine capability names or order do not match the canonical provider contract"
        }
    }
    $expectedChecks = @(
        "provider-contract",
        "catalog-certification-safety",
        "restore-verification-safety",
        "durable-verification-notification"
    )
    if ((@($contract.contractChecks) -join ",") -ne ($expectedChecks -join ",")) {
        throw "G2 executable contract checks are incomplete"
    }
    $maria = $engines | Where-Object { $_.engine -eq "mariadb" }
    if ($maria.backupTool -ne "mariadb-dump" -or $maria.restoreTool -ne "mariadb") {
        throw "MariaDB must use vendor-current tool names"
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

Write-Output "test-existing-engines contract tests passed"
