[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$checker = Join-Path $repoRoot "scripts\check-g0-truth.ps1"
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "dbbackup-g0-truth-tests-" + [Guid]::NewGuid().ToString("N")
)
$failures = New-Object System.Collections.Generic.List[string]

function Write-Utf8Json {
    param(
        [Parameter(Mandatory = $true)] [object] $Value,
        [Parameter(Mandatory = $true)] [string] $Path
    )

    $json = $Value | ConvertTo-Json -Depth 32
    [IO.File]::WriteAllText($Path, $json, (New-Object Text.UTF8Encoding($false)))
}

function New-CaseFixture {
    param([Parameter(Mandatory = $true)] [string] $Name)

    $caseRoot = Join-Path $fixtureRoot $Name
    $paths = @(
        "release\gates",
        "docs\release",
        "docs\plan\connected-backup-platform-v2\adrs",
        "apps\console\src\screens",
        "crates",
        ".github\workflows"
    )
    foreach ($relative in $paths) {
        New-Item -ItemType Directory -Path (Join-Path $caseRoot $relative) -Force | Out-Null
    }

    Copy-Item -LiteralPath (Join-Path $repoRoot "release\gates\g0-claims.json") `
        -Destination (Join-Path $caseRoot "release\gates\g0-claims.json")
    Copy-Item -LiteralPath (Join-Path $repoRoot "release\gates\g1-evidence-requirements.json") `
        -Destination (Join-Path $caseRoot "release\gates\g1-evidence-requirements.json")
    Copy-Item -LiteralPath (Join-Path $repoRoot "release\gates\g1-fault-matrix.json") `
        -Destination (Join-Path $caseRoot "release\gates\g1-fault-matrix.json")
    Copy-Item -LiteralPath (Join-Path $repoRoot "docs\release\g0-direct-execution-inventory.md") `
        -Destination (Join-Path $caseRoot "docs\release\g0-direct-execution-inventory.md")
    Copy-Item -LiteralPath (Join-Path $repoRoot "docs\release\g0-truth-baseline.md") `
        -Destination (Join-Path $caseRoot "docs\release\g0-truth-baseline.md")
    Get-ChildItem -LiteralPath (
        Join-Path $repoRoot "docs\plan\connected-backup-platform-v2\adrs"
    ) -Filter "ADR-*.md" | ForEach-Object {
        Copy-Item -LiteralPath $_.FullName -Destination (
            Join-Path $caseRoot "docs\plan\connected-backup-platform-v2\adrs\$($_.Name)"
        )
    }

    [IO.File]::WriteAllText(
        (Join-Path $caseRoot "README.md"),
        "Capability claims are defined in release/gates/g0-claims.json.",
        (New-Object Text.UTF8Encoding($false))
    )
    [IO.File]::WriteAllText(
        (Join-Path $caseRoot "SECURITY.md"),
        "Restore and verification guarantees follow the current claims matrix.",
        (New-Object Text.UTF8Encoding($false))
    )
    [IO.File]::WriteAllText(
        (Join-Path $caseRoot "apps\console\src\screens\Dashboard.tsx"),
        "export const copy = '기록된 실행 증적';",
        (New-Object Text.UTF8Encoding($false))
    )
    [IO.File]::WriteAllText(
        (Join-Path $caseRoot "apps\console\src\screens\Restore.tsx"),
        "export const copy = '평문 덤프 복호화 완료';",
        (New-Object Text.UTF8Encoding($false))
    )
    [IO.File]::WriteAllText(
        (Join-Path $caseRoot "apps\console\src\screens\Jobs.tsx"),
        "export const copy = '정책 저장됨, 자동 시행 미배선';",
        (New-Object Text.UTF8Encoding($false))
    )
    [IO.File]::WriteAllText(
        (Join-Path $caseRoot "apps\console\src\screens\Schedule.tsx"),
        "export const copy = 'legacy job별 Task Scheduler';",
        (New-Object Text.UTF8Encoding($false))
    )
    foreach ($screen in @("Databases.tsx", "Destinations.tsx")) {
        [IO.File]::WriteAllText(
            (Join-Path $caseRoot "apps\console\src\screens\$screen"),
            "export const copy = 'live 연결 점검은 실행하지 않았습니다.';",
            (New-Object Text.UTF8Encoding($false))
        )
    }
    [IO.File]::WriteAllText(
        (Join-Path $caseRoot ".github\workflows\ci.yml"),
        @"
uses: actions/setup-node@v4
run: .\scripts\collect-g0-evidence.ps1
run: .\scripts\check-g0-truth.ps1 -EvidencePath artifacts/g0/evidence.json
if: always()
uses: actions/upload-artifact@v4
path: artifacts/g0
"@,
        (New-Object Text.UTF8Encoding($false))
    )

    return $caseRoot
}

function Invoke-TruthChecker {
    param(
        [Parameter(Mandatory = $true)] [string] $CaseRoot,
        [string] $DurableEvidencePath,
        [string] $EvidencePath
    )

    $hostPath = (Get-Process -Id $PID).Path
    $arguments = @(
        "-NoProfile",
        "-ExecutionPolicy", "Bypass",
        "-File", $checker,
        "-RepositoryRoot", $CaseRoot
    )
    if ($DurableEvidencePath) {
        $arguments += @("-DurableEvidencePath", $DurableEvidencePath)
    }
    if ($EvidencePath) {
        $arguments += @("-EvidencePath", $EvidencePath)
    }
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $output = & $hostPath @arguments 2>&1 | Out-String
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    return @{
        ExitCode = $exitCode
        Output = $output.Trim()
    }
}

function New-G0EvidenceFixture {
    param(
        [Parameter(Mandatory = $true)] [string] $CaseRoot,
        [switch] $FailedCommand,
        [switch] $IgnoredMarkedPass,
        [switch] $StaleTreeRow
    )

    $ids = @(
        "cargo-fmt",
        "cargo-clippy",
        "cargo-test",
        "cargo-ignored-list",
        "npm-ci",
        "npm-build"
    )
    $commands = @()
    foreach ($id in $ids) {
        $failed = $FailedCommand -and $id -eq "cargo-test"
        $commands += [PSCustomObject] @{
            id = $id
            command = "fixture $id"
            kind = if ($id -eq "cargo-ignored-list") { "ignored_list" } else { "gate" }
            startedAt = "2026-07-29T00:00:00.0000000+00:00"
            finishedAt = "2026-07-29T00:00:01.0000000+00:00"
            durationMs = 1000
            exitCode = if ($failed) { 1 } else { 0 }
            status = if ($failed) { "fail" } else { "pass" }
            sourceRevision = "fixture-revision"
            sourceTreeDigest = "fixture-tree"
            runnerOs = "fixture-os"
        }
    }
    if ($StaleTreeRow) {
        $commands[0].sourceTreeDigest = "stale-tree"
    }
    $path = Join-Path $CaseRoot "g0-evidence.json"
    Write-Utf8Json -Path $path -Value ([PSCustomObject] @{
        schemaVersion = 1
        gateId = "G0"
        sourceRevision = "fixture-revision"
        sourceTreeDigest = "fixture-tree"
        startedAt = "2026-07-29T00:00:00.0000000+00:00"
        finishedAt = "2026-07-29T00:00:06.0000000+00:00"
        runner = [PSCustomObject] @{
            os = "fixture-os"
            architecture = "64-bit"
            powershell = "5.1"
            rustc = "rustc fixture"
            cargo = "cargo fixture"
            node = "vfixture"
            npm = "fixture"
        }
        commands = $commands
        failureCount = if ($FailedCommand) { 1 } else { 0 }
        status = if ($FailedCommand) { "fail" } else { "pass" }
        ignoredTests = @(
            [PSCustomObject] @{
                name = "fixture::ignored"
                status = if ($IgnoredMarkedPass) { "pass" } else { "not_run" }
            }
        )
    })
    return $path
}

function New-G1EvidenceFixture {
    param(
        [Parameter(Mandatory = $true)] [string] $CaseRoot,
        [switch] $StaleRowSource
    )

    $matrixPath = Join-Path $CaseRoot "release\gates\g1-fault-matrix.json"
    $matrix = Get-Content -LiteralPath $matrixPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    $rows = @()
    foreach ($command in @($matrix.commands)) {
        $rows += [PSCustomObject] @{
            id = $command.id
            command = $command.display
            status = "pass"
            exitCode = 0
            startedAt = "2026-07-29T00:00:00.0000000+00:00"
            finishedAt = "2026-07-29T00:00:01.0000000+00:00"
            durationMs = 1000
            sourceRevision = "revision-a"
            sourceTreeDigest = "tree-a"
            buildId = "build-a"
            runnerOs = "fixture-os"
        }
    }
    if ($StaleRowSource) {
        $rows[0].sourceRevision = "revision-stale"
    }
    $path = Join-Path $CaseRoot "g1-result.json"
    Write-Utf8Json -Path $path -Value ([PSCustomObject] @{
        schemaVersion = 1
        gateId = "G1"
        sourceRevision = "revision-a"
        sourceTreeDigest = "tree-a"
        buildId = "build-a"
        startedAt = "2026-07-29T00:00:00.0000000+00:00"
        finishedAt = "2026-07-29T00:00:09.0000000+00:00"
        runner = [PSCustomObject] @{
            os = "fixture-os"
        }
        evidence = $rows
        failureCount = 0
        status = "pass"
    })
    return $path
}

function Assert-G0EvidenceCase {
    param(
        [Parameter(Mandatory = $true)] [string] $Name,
        [Parameter(Mandatory = $true)] [bool] $ShouldPass,
        [switch] $FailedCommand,
        [switch] $IgnoredMarkedPass,
        [switch] $StaleTreeRow
    )

    $caseRoot = New-CaseFixture -Name $Name
    $evidencePath = New-G0EvidenceFixture `
        -CaseRoot $caseRoot `
        -FailedCommand:$FailedCommand `
        -IgnoredMarkedPass:$IgnoredMarkedPass `
        -StaleTreeRow:$StaleTreeRow
    $result = Invoke-TruthChecker `
        -CaseRoot $caseRoot `
        -EvidencePath $evidencePath
    $passed = $result.ExitCode -eq 0
    if ($passed -ne $ShouldPass) {
        $failures.Add(
            "$Name expected pass=$ShouldPass, exit=$($result.ExitCode), output=$($result.Output)"
        )
    }
}

function Assert-Case {
    param(
        [Parameter(Mandatory = $true)] [string] $Name,
        [Parameter(Mandatory = $true)] [bool] $ShouldPass,
        [Parameter(Mandatory = $true)] [scriptblock] $Arrange
    )

    $caseRoot = New-CaseFixture -Name $Name
    $durableEvidencePath = & $Arrange $caseRoot
    $result = Invoke-TruthChecker -CaseRoot $caseRoot -DurableEvidencePath $durableEvidencePath
    $passed = $result.ExitCode -eq 0
    if ($passed -ne $ShouldPass) {
        $failures.Add(
            "$Name expected pass=$ShouldPass, exit=$($result.ExitCode), output=$($result.Output)"
        )
    }
}

try {
    New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null

    Assert-Case -Name "valid" -ShouldPass $true -Arrange {
        param($caseRoot)
        return $null
    }
    Assert-Case -Name "duplicate-id" -ShouldPass $false -Arrange {
        param($caseRoot)
        $path = Join-Path $caseRoot "release\gates\g0-claims.json"
        $claims = Get-Content -LiteralPath $path -Raw -Encoding UTF8 | ConvertFrom-Json
        $duplicate = $claims.capabilities[0] | ConvertTo-Json -Depth 8 | ConvertFrom-Json
        $claims.capabilities += $duplicate
        Write-Utf8Json -Value $claims -Path $path
        return $null
    }
    Assert-Case -Name "unknown-claim" -ShouldPass $false -Arrange {
        param($caseRoot)
        $path = Join-Path $caseRoot "release\gates\g0-claims.json"
        $claims = Get-Content -LiteralPath $path -Raw -Encoding UTF8 | ConvertFrom-Json
        $claims.capabilities[0].claim = "complete"
        Write-Utf8Json -Value $claims -Path $path
        return $null
    }
    Assert-Case -Name "unsupported-promotion" -ShouldPass $false -Arrange {
        param($caseRoot)
        $path = Join-Path $caseRoot "release\gates\g0-claims.json"
        $claims = Get-Content -LiteralPath $path -Raw -Encoding UTF8 | ConvertFrom-Json
        $capability = $claims.capabilities | Where-Object { $_.id -eq "backup.postgresql" }
        $capability.claim = "supported"
        $capability.managedEvidence = "not_run"
        Write-Utf8Json -Value $claims -Path $path
        return $null
    }
    Assert-Case -Name "missing-adr" -ShouldPass $false -Arrange {
        param($caseRoot)
        Remove-Item -LiteralPath (
            Join-Path $caseRoot "docs\plan\connected-backup-platform-v2\adrs\ADR-007-local-ipc-authorization.md"
        )
        return $null
    }
    Assert-Case -Name "banned-copy" -ShouldPass $false -Arrange {
        param($caseRoot)
        [IO.File]::WriteAllText(
            (Join-Path $caseRoot "apps\console\src\screens\Dashboard.tsx"),
            "export const copy = '모든 백업이 안전합니다';",
            (New-Object Text.UTF8Encoding($false))
        )
        return $null
    }
    Assert-Case -Name "missing-policy-copy" -ShouldPass $false -Arrange {
        param($caseRoot)
        [IO.File]::WriteAllText(
            (Join-Path $caseRoot "apps\console\src\screens\Jobs.tsx"),
            "export const copy = 'L3 실제 복원';",
            (New-Object Text.UTF8Encoding($false))
        )
        return $null
    }
    Assert-Case -Name "missing-schedule-copy" -ShouldPass $false -Arrange {
        param($caseRoot)
        [IO.File]::WriteAllText(
            (Join-Path $caseRoot "apps\console\src\screens\Schedule.tsx"),
            "export const copy = 'RPO 보장 완료';",
            (New-Object Text.UTF8Encoding($false))
        )
        return $null
    }
    Assert-Case -Name "missing-registration-copy" -ShouldPass $false -Arrange {
        param($caseRoot)
        $claimsPath = Join-Path $caseRoot "release\gates\g0-claims.json"
        $claims = Get-Content -LiteralPath $claimsPath -Raw -Encoding UTF8 |
            ConvertFrom-Json
        $probeClaim = @(
            $claims.capabilities |
                Where-Object { $_.id -eq "connection.probe" }
        )[0]
        $probeClaim.claim = "unavailable"
        [IO.File]::WriteAllText(
            $claimsPath,
            ($claims | ConvertTo-Json -Depth 16),
            (New-Object Text.UTF8Encoding($false))
        )
        [IO.File]::WriteAllText(
            (Join-Path $caseRoot "apps\console\src\screens\Destinations.tsx"),
            "export const copy = '연결 성공';",
            (New-Object Text.UTF8Encoding($false))
        )
        return $null
    }
    Assert-Case -Name "missing-ci-evidence-wiring" -ShouldPass $false -Arrange {
        param($caseRoot)
        [IO.File]::WriteAllText(
            (Join-Path $caseRoot ".github\workflows\ci.yml"),
            "run: cargo test --workspace",
            (New-Object Text.UTF8Encoding($false))
        )
        return $null
    }
    Assert-Case -Name "stale-durable-evidence" -ShouldPass $false -Arrange {
        param($caseRoot)
        return New-G1EvidenceFixture -CaseRoot $caseRoot -StaleRowSource
    }
    Assert-Case -Name "valid-durable-evidence" -ShouldPass $true -Arrange {
        param($caseRoot)
        return New-G1EvidenceFixture -CaseRoot $caseRoot
    }
    Assert-G0EvidenceCase -Name "valid-g0-evidence" -ShouldPass $true
    Assert-G0EvidenceCase `
        -Name "failed-g0-evidence" `
        -ShouldPass $false `
        -FailedCommand
    Assert-G0EvidenceCase `
        -Name "ignored-marked-pass" `
        -ShouldPass $false `
        -IgnoredMarkedPass
    Assert-G0EvidenceCase `
        -Name "stale-g0-tree-row" `
        -ShouldPass $false `
        -StaleTreeRow
    $hostPath = (Get-Process -Id $PID).Path
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        & $hostPath `
            -NoProfile `
            -ExecutionPolicy Bypass `
            -File $checker 2>&1 |
            Out-Null
        if ($LASTEXITCODE -ne 0) {
            $failures.Add("default repository root invocation must pass")
        }
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
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

if ($failures.Count -gt 0) {
    $failures | ForEach-Object { Write-Error $_ }
    exit 1
}

Write-Output "check-g0-truth tests passed: 17"
