[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $OutputRoot,
    [string] $CommandPlanPath
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}

function Write-Utf8Text {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [AllowEmptyString()] [string] $Value
    )

    [IO.File]::WriteAllText($Path, $Value, (New-Object Text.UTF8Encoding($false)))
}

function Write-Utf8Json {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [object] $Value
    )

    $json = $Value | ConvertTo-Json -Depth 32
    Write-Utf8Text -Path $Path -Value $json
}

function Resolve-Application {
    param([Parameter(Mandatory = $true)] [string] $Name)

    return Get-Command `
        -Name $Name `
        -CommandType Application `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1
}

function Invoke-ShortCommand {
    param(
        [Parameter(Mandatory = $true)] [string] $Executable,
        [string[]] $Arguments = @()
    )

    $application = Resolve-Application -Name $Executable
    if ($null -eq $application) {
        return "unavailable"
    }

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        $lines = @(& $application.Source @Arguments 2>&1)
        $exitCode = $LASTEXITCODE
    }
    catch {
        return "unavailable"
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }

    if ($exitCode -ne 0 -or $lines.Count -eq 0) {
        return "unavailable"
    }
    return $lines[0].ToString().Trim()
}

function Get-GitValue {
    param(
        [Parameter(Mandatory = $true)] [string] $Root,
        [Parameter(Mandatory = $true)] [string[]] $Arguments
    )

    $git = Resolve-Application -Name "git"
    if ($null -eq $git) {
        return "unknown"
    }

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        $lines = @(& $git.Source -C $Root @Arguments 2>&1)
        $exitCode = $LASTEXITCODE
    }
    catch {
        return "unknown"
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }

    if ($exitCode -ne 0 -or $lines.Count -eq 0) {
        return "unknown"
    }
    return $lines[0].ToString().Trim()
}

function Get-GitLines {
    param(
        [Parameter(Mandatory = $true)] [string] $Root,
        [Parameter(Mandatory = $true)] [string[]] $Arguments
    )

    $git = Resolve-Application -Name "git"
    if ($null -eq $git) {
        throw "git executable is required"
    }
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        $lines = @(& $git.Source -C $Root @Arguments 2>&1)
        $exitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    if ($exitCode -ne 0) {
        throw "git command failed: git $($Arguments -join ' ')"
    }
    return @($lines | ForEach-Object { $_.ToString() })
}

function Get-Sha256Text {
    param([AllowEmptyString()] [string] $Value)

    $bytes = [Text.Encoding]::UTF8.GetBytes($Value)
    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString($sha.ComputeHash($bytes))).
            Replace("-", "").
            ToLowerInvariant()
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
        return ([BitConverter]::ToString($sha.ComputeHash($stream))).
            Replace("-", "").
            ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
        $stream.Dispose()
    }
}

function Get-SourceTreeDigest {
    param([Parameter(Mandatory = $true)] [string] $Root)

    $paths = @(
        Get-GitLines `
            -Root $Root `
            -Arguments @("ls-files", "--cached", "--others", "--exclude-standard")
    ) |
        Where-Object {
            if ([string]::IsNullOrWhiteSpace($_)) {
                return $false
            }
            $normalized = $_.Replace("\", "/")
            return -not $normalized.StartsWith(
                "artifacts/",
                [StringComparison]::OrdinalIgnoreCase
            ) -and
            -not $normalized.StartsWith(
                "docs/chronos/",
                [StringComparison]::OrdinalIgnoreCase
            )
        } |
        Sort-Object -Unique
    $manifest = New-Object Text.StringBuilder
    foreach ($relative in $paths) {
        $absolute = Join-Path $Root $relative
        if (-not (Test-Path -LiteralPath $absolute -PathType Leaf)) {
            continue
        }
        [void] $manifest.Append($relative.Replace("\", "/"))
        [void] $manifest.Append("`t")
        [void] $manifest.Append((Get-FileSha256 -Path $absolute))
        [void] $manifest.Append("`n")
    }
    return Get-Sha256Text -Value $manifest.ToString()
}

function Get-DefaultCommandPlan {
    $npmExecutable = if ($env:OS -eq "Windows_NT") { "npm.cmd" } else { "npm" }
    return @(
        [PSCustomObject] @{
            id = "cargo-fmt"
            display = "cargo fmt --all -- --check"
            executable = "cargo"
            arguments = @("fmt", "--all", "--", "--check")
            kind = "gate"
        },
        [PSCustomObject] @{
            id = "cargo-clippy"
            display = "cargo clippy --workspace --all-targets -- -D warnings"
            executable = "cargo"
            arguments = @(
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings"
            )
            kind = "gate"
        },
        [PSCustomObject] @{
            id = "cargo-test"
            display = "cargo test --workspace"
            executable = "cargo"
            arguments = @("test", "--workspace")
            kind = "gate"
        },
        [PSCustomObject] @{
            id = "cargo-ignored-list"
            display = "cargo test --workspace -- --ignored --list"
            executable = "cargo"
            arguments = @("test", "--workspace", "--", "--ignored", "--list")
            kind = "ignored_list"
        },
        [PSCustomObject] @{
            id = "npm-ci"
            display = "npm ci --prefix apps/console"
            executable = $npmExecutable
            arguments = @("ci", "--prefix", "apps/console")
            kind = "gate"
        },
        [PSCustomObject] @{
            id = "npm-build"
            display = "npm run build --prefix apps/console"
            executable = $npmExecutable
            arguments = @("run", "build", "--prefix", "apps/console")
            kind = "gate"
        }
    )
}

function Read-CommandPlan {
    param([string] $Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return @(Get-DefaultCommandPlan)
    }
    $resolvedPath = [IO.Path]::GetFullPath($Path)
    if (-not (Test-Path -LiteralPath $resolvedPath -PathType Leaf)) {
        throw "command plan does not exist: $resolvedPath"
    }
    $document = Get-Content -LiteralPath $resolvedPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    return $document | ForEach-Object { $_ }
}

function Test-CommandPlan {
    param([Parameter(Mandatory = $true)] [object[]] $Plan)

    if ($Plan.Count -eq 0) {
        throw "command plan must not be empty"
    }
    $seen = @{}
    foreach ($command in $Plan) {
        foreach ($property in @("id", "display", "executable", "kind")) {
            $member = $command.PSObject.Properties[$property]
            if ($null -eq $member -or
                [string]::IsNullOrWhiteSpace([string] $member.Value)) {
                throw "command plan entry requires '$property'"
            }
        }
        if ($seen.ContainsKey([string] $command.id)) {
            throw "command plan contains duplicate id '$($command.id)'"
        }
        $seen[[string] $command.id] = $true
        if (@("gate", "ignored_list") -notcontains [string] $command.kind) {
            throw "command '$($command.id)' has invalid kind '$($command.kind)'"
        }
    }
}

function Get-WorktreeEvidence {
    param(
        [Parameter(Mandatory = $true)] [string] $Root,
        [Parameter(Mandatory = $true)] [string] $SourceRevision,
        [Parameter(Mandatory = $true)] [string] $SourceTreeDigest
    )

    $entries = New-Object System.Collections.Generic.List[object]
    $git = Resolve-Application -Name "git"
    if ($null -ne $git) {
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $script:LASTEXITCODE = 0
            $statusLines = @(
                & $git.Source -C $Root status --short --untracked-files=all 2>&1
            )
            $statusExitCode = $LASTEXITCODE
        }
        catch {
            $statusLines = @()
            $statusExitCode = 1
        }
        finally {
            $ErrorActionPreference = $previousErrorAction
        }

        if ($statusExitCode -eq 0) {
            foreach ($rawLine in $statusLines) {
                $line = $rawLine.ToString()
                if ($line.Length -lt 4) {
                    continue
                }
                $entries.Add([PSCustomObject] @{
                    status = $line.Substring(0, 2)
                    path = $line.Substring(3)
                })
            }
        }
    }

    return [PSCustomObject] @{
        schemaVersion = 1
        sourceRevision = $SourceRevision
        sourceTreeDigest = $SourceTreeDigest
        branch = Get-GitValue -Root $Root -Arguments @("branch", "--show-current")
        dirty = $entries.Count -gt 0
        entries = $entries.ToArray()
    }
}

function Invoke-EvidenceCommand {
    param(
        [Parameter(Mandatory = $true)] [object] $Command,
        [Parameter(Mandatory = $true)] [string] $SourceRevision,
        [Parameter(Mandatory = $true)] [string] $SourceTreeDigest,
        [Parameter(Mandatory = $true)] [string] $RunnerOs
    )

    $started = [DateTimeOffset]::Now
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $outputLines = New-Object System.Collections.Generic.List[string]
    $application = Resolve-Application -Name ([string] $Command.executable)
    $exitCode = 127

    Write-Host "G0 evidence: $($Command.display)"
    if ($null -eq $application) {
        [Console]::Error.WriteLine(
            "command executable not found: $($Command.executable)"
        )
    }
    else {
        $arguments = @($Command.arguments | ForEach-Object { [string] $_ })
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $script:LASTEXITCODE = 0
            & $application.Source @arguments 2>&1 | ForEach-Object {
                $line = $_.ToString()
                $outputLines.Add($line)
                Write-Host $line
            }
            $exitCode = [int] $LASTEXITCODE
        }
        catch {
            $exitCode = 1
            [Console]::Error.WriteLine($_.Exception.Message)
        }
        finally {
            $ErrorActionPreference = $previousErrorAction
        }
    }

    $timer.Stop()
    $finished = [DateTimeOffset]::Now
    return [PSCustomObject] @{
        record = [PSCustomObject] @{
            id = [string] $Command.id
            command = [string] $Command.display
            kind = [string] $Command.kind
            startedAt = $started.ToString("o")
            finishedAt = $finished.ToString("o")
            durationMs = [Math]::Round($timer.Elapsed.TotalMilliseconds, 3)
            exitCode = $exitCode
            status = if ($exitCode -eq 0) { "pass" } else { "fail" }
            sourceRevision = $SourceRevision
            sourceTreeDigest = $SourceTreeDigest
            runnerOs = $RunnerOs
        }
        outputLines = $outputLines.ToArray()
    }
}

$resolvedRoot = [IO.Path]::GetFullPath($RepositoryRoot)
if (-not (Test-Path -LiteralPath $resolvedRoot -PathType Container)) {
    [Console]::Error.WriteLine("repository root does not exist: $resolvedRoot")
    exit 1
}

$resolvedOutput = if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    Join-Path $resolvedRoot "artifacts\g0"
}
elseif ([IO.Path]::IsPathRooted($OutputRoot)) {
    [IO.Path]::GetFullPath($OutputRoot)
}
else {
    [IO.Path]::GetFullPath((Join-Path $resolvedRoot $OutputRoot))
}
New-Item -ItemType Directory -Path $resolvedOutput -Force | Out-Null

$plan = @(Read-CommandPlan -Path $CommandPlanPath)
Test-CommandPlan -Plan $plan

$sourceRevision = Get-GitValue -Root $resolvedRoot -Arguments @("rev-parse", "HEAD")
$sourceTreeDigest = Get-SourceTreeDigest -Root $resolvedRoot
$runnerOs = [Environment]::OSVersion.VersionString
$runner = [PSCustomObject] @{
    os = $runnerOs
    architecture = if ([IntPtr]::Size -eq 8) { "64-bit" } else { "32-bit" }
    powershell = $PSVersionTable.PSVersion.ToString()
    rustc = Invoke-ShortCommand -Executable "rustc" -Arguments @("--version")
    cargo = Invoke-ShortCommand -Executable "cargo" -Arguments @("--version")
    node = Invoke-ShortCommand -Executable "node" -Arguments @("--version")
    npm = Invoke-ShortCommand `
        -Executable $(if ($env:OS -eq "Windows_NT") { "npm.cmd" } else { "npm" }) `
        -Arguments @("--version")
}

$worktree = Get-WorktreeEvidence `
    -Root $resolvedRoot `
    -SourceRevision $sourceRevision `
    -SourceTreeDigest $sourceTreeDigest
Write-Utf8Json -Path (Join-Path $resolvedOutput "worktree.json") -Value $worktree

$startedAt = [DateTimeOffset]::Now
$commandRecords = New-Object System.Collections.Generic.List[object]
$ignoredNames = New-Object System.Collections.Generic.List[string]
$seenIgnored = @{}
$failureCount = 0

foreach ($command in $plan) {
    $result = Invoke-EvidenceCommand `
        -Command $command `
        -SourceRevision $sourceRevision `
        -SourceTreeDigest $sourceTreeDigest `
        -RunnerOs $runnerOs
    $commandRecords.Add($result.record)
    if ($result.record.exitCode -ne 0) {
        $failureCount++
    }
    if ($command.kind -eq "ignored_list") {
        foreach ($line in @($result.outputLines)) {
            if ($line -match '^\s*(.+):\s+test\s*$') {
                $name = $Matches[1].Trim()
                if (-not [string]::IsNullOrWhiteSpace($name) -and
                    -not $seenIgnored.ContainsKey($name)) {
                    $seenIgnored[$name] = $true
                    $ignoredNames.Add($name)
                }
            }
        }
    }
}

$ignoredTests = @(
    $ignoredNames |
        Sort-Object |
        ForEach-Object {
            [PSCustomObject] @{
                name = $_
                status = "not_run"
            }
        }
)
$ignoredText = if ($ignoredTests.Count -eq 0) {
    ""
}
else {
    (@($ignoredTests | ForEach-Object { $_.name }) -join [Environment]::NewLine) +
        [Environment]::NewLine
}
Write-Utf8Text `
    -Path (Join-Path $resolvedOutput "ignored-tests.txt") `
    -Value $ignoredText

$finishedAt = [DateTimeOffset]::Now
$evidence = [PSCustomObject] @{
    schemaVersion = 1
    gateId = "G0"
    sourceRevision = $sourceRevision
    sourceTreeDigest = $sourceTreeDigest
    startedAt = $startedAt.ToString("o")
    finishedAt = $finishedAt.ToString("o")
    runner = $runner
    commands = $commandRecords.ToArray()
    failureCount = $failureCount
    status = if ($failureCount -eq 0) { "pass" } else { "fail" }
    ignoredTests = $ignoredTests
}
Write-Utf8Json -Path (Join-Path $resolvedOutput "evidence.json") -Value $evidence

Write-Output (
    "G0 evidence collected: status=$($evidence.status), " +
    "failures=$failureCount, ignored=$($ignoredTests.Count), output=$resolvedOutput"
)
if ($failureCount -gt 0) {
    exit 1
}
exit 0
