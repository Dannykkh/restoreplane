[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$collector = Join-Path $repoRoot "scripts\collect-g1-evidence.ps1"
$requirements =
    Get-Content `
        -LiteralPath (Join-Path $repoRoot "release\gates\g1-evidence-requirements.json") `
        -Raw `
        -Encoding UTF8 |
    ConvertFrom-Json
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "dbbackup-g1-collector-tests-" + [Guid]::NewGuid().ToString("N")
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
        gateId = "G1"
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

    $passMatrix = Join-Path $fixtureRoot "pass-matrix.json"
    $passOutput = Join-Path $fixtureRoot "pass-output"
    Write-Matrix -Path $passMatrix
    if ((Invoke-Collector -MatrixPath $passMatrix -OutputPath $passOutput) -ne 0) {
        throw "all-pass matrix must exit 0"
    }
    $passEvidence =
        Get-Content -LiteralPath (Join-Path $passOutput "evidence.json") -Raw -Encoding UTF8 |
        ConvertFrom-Json
    if ($passEvidence.status -ne "pass" -or $passEvidence.failureCount -ne 0) {
        throw "all-pass evidence summary is invalid"
    }
    $passRows = @($passEvidence.evidence)
    if ($passRows.Count -ne 9) {
        throw "all-pass evidence must contain nine rows"
    }
    if (@($passRows.buildId | Select-Object -Unique).Count -ne 1 -or
        @($passRows.sourceTreeDigest | Select-Object -Unique).Count -ne 1) {
        throw "all rows must share one build and source tree digest"
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
    $failingId = [string] $requirements.requiredEvidenceIds[3]
    Write-Matrix -Path $failMatrix -FailingId $failingId
    if ((Invoke-Collector -MatrixPath $failMatrix -OutputPath $failOutput) -eq 0) {
        throw "failing matrix must exit nonzero"
    }
    $failEvidence =
        Get-Content -LiteralPath (Join-Path $failOutput "evidence.json") -Raw -Encoding UTF8 |
        ConvertFrom-Json
    if ($failEvidence.status -ne "fail" -or $failEvidence.failureCount -ne 1) {
        throw "failure summary is invalid"
    }
    $failRows = @($failEvidence.evidence)
    if ($failRows.Count -ne 9 -or
        @($failRows | Where-Object { $_.id -eq $failingId -and $_.status -eq "fail" }).Count -ne 1) {
        throw "collector must continue after a failed command"
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

Write-Output "collect-g1-evidence tests passed"
