[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$collector = Join-Path $repoRoot "scripts\collect-s03-evidence.ps1"
$requirements =
    Get-Content `
        -LiteralPath (Join-Path $repoRoot "release\gates\s03-evidence-requirements.json") `
        -Raw `
        -Encoding UTF8 |
    ConvertFrom-Json
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "dbbackup-s03-collector-tests-" + [Guid]::NewGuid().ToString("N")
)

function Write-Matrix {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [string] $ZeroTestId
    )

    $hostPath = (Get-Process -Id $PID).Path
    $commands = @(
        $requirements.requiredEvidenceIds |
            ForEach-Object {
                $passed = if ($_ -eq $ZeroTestId) { 0 } else { 1 }
                [PSCustomObject] @{
                    id = $_
                    display = "fixture $_"
                    executable = $hostPath
                    arguments = @(
                        "-NoProfile",
                        "-Command",
                        "Write-Output 'test result: ok. $passed passed; 0 failed; 0 ignored; 0 measured; 0 filtered out'; exit 0"
                    )
                    platform = "windows"
                    minimumPassedTests = 1
                }
            }
    )
    $json = [PSCustomObject] @{
        schemaVersion = 1
        gateId = "S03"
        commands = $commands
    } | ConvertTo-Json -Depth 16
    [IO.File]::WriteAllText($Path, $json, (New-Object Text.UTF8Encoding($false)))
}

function Invoke-Collector {
    param(
        [Parameter(Mandatory = $true)] [string] $MatrixPath,
        [Parameter(Mandatory = $true)] [string] $OutputPath
    )

    $startInfo = New-Object Diagnostics.ProcessStartInfo
    $startInfo.FileName = (Get-Process -Id $PID).Path
    $startInfo.Arguments = (
        '-NoProfile -File "{0}" -MatrixPath "{1}" -OutputRoot "{2}"' -f
            $collector.Replace('"', '\"'),
            $MatrixPath.Replace('"', '\"'),
            $OutputPath.Replace('"', '\"')
    )
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $process = [Diagnostics.Process]::Start($startInfo)
    try {
        $process.WaitForExit()
        return [int] $process.ExitCode
    }
    finally {
        $process.Dispose()
    }
}

try {
    New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
    $requiredIds = @($requirements.requiredEvidenceIds)
    if ($requiredIds.Count -ne 14 -or
        $requirements.sameSourceRevision -ne $true -or
        $requirements.sameBuildId -ne $true -or
        $requirements.allRequired -ne $true) {
        throw "S03 requirements must bind exactly fourteen all-required same-build checks"
    }

    $realMatrix =
        Get-Content `
            -LiteralPath (Join-Path $repoRoot "release\gates\s03-quality-matrix.json") `
            -Raw `
            -Encoding UTF8 |
        ConvertFrom-Json
    $realCommands = @($realMatrix.commands)
    if ($realCommands.Count -ne $requiredIds.Count) {
        throw "S03 quality matrix command count is invalid"
    }
    foreach ($command in $realCommands) {
        $expectedMinimum = if ($command.id -eq "s03.offline_cutover_recovery") { 5 } else { 1 }
        if ($command.minimumPassedTests -ne $expectedMinimum) {
            throw "S03 command does not reject a zero-test false green: $($command.id)"
        }
    }
    $offlineRecovery = @(
        $realCommands |
            Where-Object { $_.id -eq "s03.offline_cutover_recovery" }
    )
    if ($offlineRecovery.Count -ne 1 -or
        ([string] $offlineRecovery[0].display).Contains(
            "clean_offline_recovery_commits_legacy_authority_before_task_restore_and_resumes"
        )) {
        throw "S03 offline recovery evidence must execute the complete non-ignored fault suite"
    }
    $legacyGuard = @(
        $realCommands |
            Where-Object { $_.id -eq "s03.legacy_direct_guard" }
    )
    if ($legacyGuard.Count -ne 1 -or
        -not ([string] $legacyGuard[0].display).Contains(
            "production_authority_blocks_legacy_execution"
        )) {
        throw "S03 legacy guard evidence must exercise the production blocking boundary"
    }
    $realSchedulerRecovery = @(
        $realCommands |
            Where-Object { $_.id -eq "s03.real_scheduler_recovery" }
    )
    if ($realSchedulerRecovery.Count -ne 1 -or
        -not ([string] $realSchedulerRecovery[0].display).Contains(
            "real_windows_scheduler_controller_restores_a_throwaway_dbbackup_task"
        ) -or
        -not ([string] $realSchedulerRecovery[0].display).Contains("--ignored --exact")) {
        throw "S03 scheduler recovery evidence must exercise the managed Windows task"
    }

    $ciWorkflow = Get-Content `
        -LiteralPath (Join-Path $repoRoot ".github\workflows\ci.yml") `
        -Raw `
        -Encoding UTF8
    foreach ($requiredCiText in @(
        ".\scripts\tests\collect-s03-evidence.Tests.ps1",
        ".\scripts\collect-s03-evidence.ps1",
        "path: artifacts/s03"
    )) {
        if (-not $ciWorkflow.Contains($requiredCiText)) {
            throw "CI is missing the S03 evidence gate wiring: $requiredCiText"
        }
    }

    $passMatrix = Join-Path $fixtureRoot "pass-matrix.json"
    $passOutput = Join-Path $fixtureRoot "pass-output"
    Write-Matrix -Path $passMatrix
    if ((Invoke-Collector -MatrixPath $passMatrix -OutputPath $passOutput) -ne 0) {
        throw "all-pass matrix must exit 0"
    }
    $passEvidence =
        Get-Content -LiteralPath (Join-Path $passOutput "evidence.json") -Raw -Encoding UTF8 |
        ConvertFrom-Json
    $passRows = @($passEvidence.evidence)
    if ($passEvidence.gateId -ne "S03" -or
        $passEvidence.status -ne "pass" -or
        $passEvidence.failureCount -ne 0 -or
        $passRows.Count -ne $requiredIds.Count) {
        throw "all-pass S03 evidence summary is invalid"
    }
    foreach ($row in $passRows) {
        if ($row.status -ne "pass" -or
            $row.exitCode -ne 0 -or
            $row.observedPassedTests -lt 1) {
            throw "all-pass row is invalid: $($row.id)"
        }
        if ($null -ne $row.PSObject.Properties["stdout"] -or
            $null -ne $row.PSObject.Properties["stderr"] -or
            $null -ne $row.PSObject.Properties["output"]) {
            throw "raw command output must not be persisted"
        }
    }

    $zeroMatrix = Join-Path $fixtureRoot "zero-matrix.json"
    $zeroOutput = Join-Path $fixtureRoot "zero-output"
    $zeroTestId = [string] $requiredIds[3]
    Write-Matrix -Path $zeroMatrix -ZeroTestId $zeroTestId
    $zeroExitCode = Invoke-Collector -MatrixPath $zeroMatrix -OutputPath $zeroOutput
    if ($zeroExitCode -eq 0) {
        throw "zero-test command must make the collector fail despite exit code 0"
    }
    $zeroEvidence =
        Get-Content -LiteralPath (Join-Path $zeroOutput "evidence.json") -Raw -Encoding UTF8 |
        ConvertFrom-Json
    $zeroRows = @($zeroEvidence.evidence)
    if ($zeroEvidence.status -ne "fail" -or
        $zeroEvidence.failureCount -ne 1 -or
        @(
            $zeroRows |
                Where-Object {
                    $_.id -eq $zeroTestId -and
                    $_.status -eq "fail" -and
                    $_.observedPassedTests -eq 0
                }
        ).Count -ne 1) {
        throw "S03 collector did not preserve the zero-test false-green failure"
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

Write-Output "collect-s03-evidence tests passed"
