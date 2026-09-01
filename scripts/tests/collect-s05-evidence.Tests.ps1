[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$collector = Join-Path $repoRoot "scripts\collect-s05-evidence.ps1"
$requirements =
    Get-Content `
        -LiteralPath (Join-Path $repoRoot "release\gates\s05-evidence-requirements.json") `
        -Raw `
        -Encoding UTF8 |
    ConvertFrom-Json
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "dbbackup-s05-collector-tests-" + [Guid]::NewGuid().ToString("N")
)

function Write-Matrix {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [string] $FailingId
    )

    $hostPath = (Get-Process -Id $PID).Path
    $commands = @(
        $requirements.requiredEvidenceIds |
            ForEach-Object {
                $exitCode = if ($_ -eq $FailingId) { 7 } else { 0 }
                [PSCustomObject] @{
                    id = $_
                    display = "fixture $_"
                    executable = $hostPath
                    arguments = @("-NoProfile", "-Command", "exit $exitCode")
                    platform = "windows"
                }
            }
    )
    $json = [PSCustomObject] @{
        schemaVersion = 1
        gateId = "S05"
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
    if ($requiredIds.Count -lt 10) {
        throw "S05 requirements must cover every quality gate"
    }
    $mappedIds = @(
        $requirements.qualityGateMapping.PSObject.Properties.Value |
            ForEach-Object { @($_) }
    )
    foreach ($requiredId in $requiredIds) {
        if ($mappedIds -notcontains $requiredId) {
            throw "S05 evidence is not mapped to a quality gate: $requiredId"
        }
    }
    if (@($requirements.qualityGateMapping.PSObject.Properties).Count -ne 10) {
        throw "S05 requirements must map exactly ten quality gates"
    }
    $ciWorkflow = Get-Content `
        -LiteralPath (Join-Path $repoRoot ".github\workflows\ci.yml") `
        -Raw `
        -Encoding UTF8
    foreach ($requiredCiText in @(
        ".\scripts\tests\collect-s05-evidence.Tests.ps1",
        ".\scripts\collect-s05-evidence.ps1",
        "path: artifacts/s05"
    )) {
        if (-not $ciWorkflow.Contains($requiredCiText)) {
            throw "CI is missing the S05 evidence gate wiring: $requiredCiText"
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
    if ($passEvidence.gateId -ne "S05" -or
        $passEvidence.status -ne "pass" -or
        $passEvidence.failureCount -ne 0) {
        throw "all-pass S05 evidence summary is invalid"
    }
    $passRows = @($passEvidence.evidence)
    if ($passRows.Count -ne $requiredIds.Count) {
        throw "all-pass evidence row count is invalid"
    }
    if (@($passRows.buildId | Select-Object -Unique).Count -ne 1 -or
        @($passRows.sourceTreeDigest | Select-Object -Unique).Count -ne 1) {
        throw "all S05 rows must share one build and source tree digest"
    }
    foreach ($row in $passRows) {
        if ($row.status -ne "pass" -or $row.exitCode -ne 0) {
            throw "all-pass row is not pass: $($row.id)"
        }
        if ($null -ne $row.PSObject.Properties["stdout"] -or
            $null -ne $row.PSObject.Properties["stderr"] -or
            $null -ne $row.PSObject.Properties["output"]) {
            throw "raw command output must not be persisted"
        }
    }

    $failMatrix = Join-Path $fixtureRoot "fail-matrix.json"
    $failOutput = Join-Path $fixtureRoot "fail-output"
    $failingId = [string] $requiredIds[3]
    Write-Matrix -Path $failMatrix -FailingId $failingId
    $writtenFailMatrix = Get-Content -LiteralPath $failMatrix -Raw -Encoding UTF8 | ConvertFrom-Json
    $writtenFailCommand = @(
        $writtenFailMatrix.commands |
            Where-Object { $_.id -eq $failingId }
    )
    if ($writtenFailCommand.Count -ne 1 -or
        [string] $writtenFailCommand[0].arguments[-1] -ne "exit 7") {
        throw "failure fixture did not encode the requested nonzero command"
    }
    if ((Invoke-Collector -MatrixPath $failMatrix -OutputPath $failOutput) -eq 0) {
        throw "failing matrix must exit nonzero"
    }
    $failEvidence =
        Get-Content -LiteralPath (Join-Path $failOutput "evidence.json") -Raw -Encoding UTF8 |
        ConvertFrom-Json
    $failRows = @($failEvidence.evidence)
    if ($failEvidence.gateId -ne "S05" -or
        $failEvidence.status -ne "fail" -or
        $failEvidence.failureCount -ne 1 -or
        $failRows.Count -ne $requiredIds.Count -or
        @(
            $failRows |
                Where-Object { $_.id -eq $failingId -and $_.status -eq "fail" }
        ).Count -ne 1) {
        throw "S05 collector must preserve one failure and continue through the matrix"
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

Write-Output "collect-s05-evidence tests passed"
