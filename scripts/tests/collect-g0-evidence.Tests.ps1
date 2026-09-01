[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$collector = Join-Path $repoRoot "scripts\collect-g0-evidence.ps1"
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "restoreplane-g0-collector-tests-" + [Guid]::NewGuid().ToString("N")
)
$failures = New-Object System.Collections.Generic.List[string]

function Write-Utf8Json {
    param(
        [Parameter(Mandatory = $true)] [object] $Value,
        [Parameter(Mandatory = $true)] [string] $Path
    )

    $json = $Value | ConvertTo-Json -Depth 16
    [IO.File]::WriteAllText($Path, $json, (New-Object Text.UTF8Encoding($false)))
}

function Add-Failure {
    param([Parameter(Mandatory = $true)] [string] $Message)

    $failures.Add($Message)
}

function Invoke-Collector {
    param(
        [Parameter(Mandatory = $true)] [string] $PlanPath,
        [Parameter(Mandatory = $true)] [string] $OutputPath,
        [switch] $UseDefaultRepositoryRoot
    )

    $hostPath = (Get-Process -Id $PID).Path
    $arguments = @(
        "-NoProfile",
        "-ExecutionPolicy", "Bypass",
        "-File", $collector,
        "-OutputRoot", $OutputPath,
        "-CommandPlanPath", $PlanPath
    )
    if (-not $UseDefaultRepositoryRoot) {
        $arguments += @("-RepositoryRoot", $repoRoot)
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

function Read-Evidence {
    param(
        [Parameter(Mandatory = $true)] [string] $OutputPath,
        [string] $CollectorOutput
    )

    $path = Join-Path $OutputPath "evidence.json"
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        Add-Failure "evidence.json missing from $OutputPath; collector output: $CollectorOutput"
        return $null
    }
    return Get-Content -LiteralPath $path -Raw -Encoding UTF8 | ConvertFrom-Json
}

try {
    New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null

    $failurePlanPath = Join-Path $fixtureRoot "failure-plan.json"
    Write-Utf8Json -Path $failurePlanPath -Value @(
        @{
            id = "before-failure"
            display = "fixture before failure"
            executable = "powershell.exe"
            arguments = @(
                "-NoProfile",
                "-Command",
                "Write-Output 'TOP-SECRET-FIXTURE'; exit 0"
            )
            kind = "gate"
        },
        @{
            id = "expected-failure"
            display = "fixture expected failure"
            executable = "powershell.exe"
            arguments = @("-NoProfile", "-Command", "exit 7")
            kind = "gate"
        },
        @{
            id = "after-failure"
            display = "fixture after failure"
            executable = "powershell.exe"
            arguments = @("-NoProfile", "-Command", "Write-Output 'continued'; exit 0")
            kind = "gate"
        }
    )

    $failureOutput = Join-Path $fixtureRoot "failure-output"
    $failureResult = Invoke-Collector `
        -PlanPath $failurePlanPath `
        -OutputPath $failureOutput
    if ($failureResult.ExitCode -ne 1) {
        Add-Failure (
            "failure plan expected collector exit 1, got $($failureResult.ExitCode): " +
            $failureResult.Output
        )
    }
    $failedEvidence = Read-Evidence `
        -OutputPath $failureOutput `
        -CollectorOutput $failureResult.Output
    if ($null -ne $failedEvidence) {
        if (@($failedEvidence.commands).Count -ne 3) {
            Add-Failure "failure plan did not record all three commands"
        }
        if ($failedEvidence.failureCount -ne 1 -or $failedEvidence.status -ne "fail") {
            Add-Failure "failure plan summary is incorrect"
        }
        $afterFailure = @($failedEvidence.commands) |
            Where-Object { $_.id -eq "after-failure" }
        if (@($afterFailure).Count -ne 1 -or $afterFailure[0].exitCode -ne 0) {
            Add-Failure "collector did not continue after a failed command"
        }
        foreach ($command in @($failedEvidence.commands)) {
            foreach ($property in @(
                "startedAt",
                "finishedAt",
                "durationMs",
                "exitCode",
                "sourceRevision",
                "sourceTreeDigest",
                "runnerOs"
            )) {
                if ($null -eq $command.PSObject.Properties[$property]) {
                    Add-Failure "command '$($command.id)' is missing '$property'"
                }
            }
        }
        $rawEvidence = Get-Content `
            -LiteralPath (Join-Path $failureOutput "evidence.json") `
            -Raw `
            -Encoding UTF8
        if ($rawEvidence.Contains("TOP-SECRET-FIXTURE")) {
            Add-Failure "command output leaked into evidence.json"
        }
    }
    if (-not (Test-Path -LiteralPath (Join-Path $failureOutput "worktree.json"))) {
        Add-Failure "worktree.json missing after failed command plan"
    }

    $passPlanPath = Join-Path $fixtureRoot "pass-plan.json"
    Write-Utf8Json -Path $passPlanPath -Value @(
        @{
            id = "gate-pass"
            display = "fixture gate pass"
            executable = "powershell.exe"
            arguments = @("-NoProfile", "-Command", "exit 0")
            kind = "gate"
        },
        @{
            id = "ignored-list"
            display = "fixture ignored list"
            executable = "powershell.exe"
            arguments = @(
                "-NoProfile",
                "-Command",
                (
                    "Write-Output 'alpha::ignored_case: test'; " +
                    "Write-Output 'beta::ignored_benchmark: benchmark'; exit 0"
                )
            )
            kind = "ignored_list"
        }
    )

    $passOutput = Join-Path $fixtureRoot "pass-output"
    $passResult = Invoke-Collector -PlanPath $passPlanPath -OutputPath $passOutput
    if ($passResult.ExitCode -ne 0) {
        Add-Failure (
            "pass plan expected collector exit 0, got $($passResult.ExitCode): " +
            $passResult.Output
        )
    }
    $passEvidence = Read-Evidence `
        -OutputPath $passOutput `
        -CollectorOutput $passResult.Output
    if ($null -ne $passEvidence) {
        if ($passEvidence.failureCount -ne 0 -or $passEvidence.status -ne "pass") {
            Add-Failure "pass plan summary is incorrect"
        }
        if ($passEvidence.sourceTreeDigest -notmatch "^[0-9a-f]{64}$") {
            Add-Failure "pass evidence is missing the source tree digest"
        }
        foreach ($command in @($passEvidence.commands)) {
            if ($command.sourceTreeDigest -ne $passEvidence.sourceTreeDigest) {
                Add-Failure "command '$($command.id)' has stale source tree identity"
            }
        }
        $worktree = Get-Content `
            -LiteralPath (Join-Path $passOutput "worktree.json") `
            -Raw `
            -Encoding UTF8 |
            ConvertFrom-Json
        if ($worktree.sourceTreeDigest -ne $passEvidence.sourceTreeDigest) {
            Add-Failure "worktree and command evidence use different source tree identities"
        }
        $ignoredTests = @($passEvidence.ignoredTests)
        if ($ignoredTests.Count -ne 1 -or
            $ignoredTests[0].name -ne "alpha::ignored_case" -or
            $ignoredTests[0].status -ne "not_run") {
            Add-Failure "ignored test names were not normalized as not_run"
        }
    }
    $ignoredPath = Join-Path $passOutput "ignored-tests.txt"
    if (-not (Test-Path -LiteralPath $ignoredPath -PathType Leaf)) {
        Add-Failure "ignored-tests.txt missing from pass plan"
    }
    else {
        $ignoredText = Get-Content -LiteralPath $ignoredPath -Raw -Encoding UTF8
        if ($ignoredText.Trim() -ne "alpha::ignored_case") {
            Add-Failure "ignored-tests.txt contains unexpected content: '$($ignoredText.Trim())'"
        }
    }

    $defaultRootOutput = Join-Path $fixtureRoot "default-root-output"
    $defaultRootResult = Invoke-Collector `
        -PlanPath $passPlanPath `
        -OutputPath $defaultRootOutput `
        -UseDefaultRepositoryRoot
    if ($defaultRootResult.ExitCode -ne 0 -or
        -not (Test-Path -LiteralPath (
            Join-Path $defaultRootOutput "evidence.json"
        ) -PathType Leaf)) {
        Add-Failure (
            "child PowerShell default repository root failed: " +
            $defaultRootResult.Output
        )
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

Write-Output "collect-g0-evidence tests passed: 3"
