[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $OutputRoot,
    [string] $MatrixPath,
    [string] $RequirementsPath,
    [ValidatePattern("^[A-Z][A-Z0-9]{0,15}$")]
    [string] $GateId = "G1"
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}

function Write-Utf8Json {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [object] $Value
    )

    $json = $Value | ConvertTo-Json -Depth 32
    [IO.File]::WriteAllText($Path, $json, (New-Object Text.UTF8Encoding($false)))
}

function Resolve-Application {
    param([Parameter(Mandatory = $true)] [string] $Name)

    return Get-Command -Name $Name -CommandType Application -ErrorAction SilentlyContinue |
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

function Read-And-ValidateMatrix {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $RequirementsPath,
        [Parameter(Mandatory = $true)] [string] $ExpectedGateId
    )

    $matrix = Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
    $requirements =
        Get-Content -LiteralPath $RequirementsPath -Raw -Encoding UTF8 | ConvertFrom-Json
    if ($matrix.schemaVersion -ne 1 -or $matrix.gateId -ne $ExpectedGateId) {
        throw "$ExpectedGateId matrix schemaVersion/gateId is invalid"
    }
    if ($requirements.schemaVersion -ne 1 -or
        $requirements.gateId -ne $ExpectedGateId -or
        $requirements.allRequired -ne $true) {
        throw "$ExpectedGateId requirements are invalid"
    }
    $commands = @($matrix.commands)
    $requiredIds = @($requirements.requiredEvidenceIds)
    if ($commands.Count -ne $requiredIds.Count) {
        throw "$ExpectedGateId matrix must contain exactly $($requiredIds.Count) commands"
    }
    $seen = @{}
    foreach ($command in $commands) {
        foreach ($property in @("id", "display", "executable", "platform")) {
            if ([string]::IsNullOrWhiteSpace([string] $command.$property)) {
                throw "$ExpectedGateId matrix command requires '$property'"
            }
        }
        $minimumProperty = $command.PSObject.Properties["minimumPassedTests"]
        if ($null -ne $minimumProperty) {
            $minimumPassedTests = 0
            if (-not [int]::TryParse(
                [string] $minimumProperty.Value,
                [ref] $minimumPassedTests
            ) -or
                $minimumPassedTests -lt 1 -or
                $minimumPassedTests -gt 1000) {
                throw "$ExpectedGateId matrix command has invalid 'minimumPassedTests'"
            }
        }
        if ($seen.ContainsKey([string] $command.id)) {
            throw "$ExpectedGateId matrix duplicates '$($command.id)'"
        }
        $seen[[string] $command.id] = $true
        if ($requiredIds -notcontains [string] $command.id) {
            throw "$ExpectedGateId matrix has unexpected evidence '$($command.id)'"
        }
    }
    foreach ($requiredId in $requiredIds) {
        if (-not $seen.ContainsKey([string] $requiredId)) {
            throw "$ExpectedGateId matrix is missing '$requiredId'"
        }
    }
    return $commands
}

function Invoke-EvidenceCommand {
    param(
        [Parameter(Mandatory = $true)] [object] $Command,
        [Parameter(Mandatory = $true)] [string] $SourceRevision,
        [Parameter(Mandatory = $true)] [string] $SourceTreeDigest,
        [Parameter(Mandatory = $true)] [string] $BuildId,
        [Parameter(Mandatory = $true)] [string] $RunnerOs,
        [Parameter(Mandatory = $true)] [string] $EvidenceGateId
    )

    $started = [DateTimeOffset]::Now
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $application = Resolve-Application -Name ([string] $Command.executable)
    $exitCode = 127
    $outputLines = New-Object System.Collections.Generic.List[string]
    Write-Host "$EvidenceGateId evidence: $($Command.display)"
    if ($null -eq $application) {
        [Console]::Error.WriteLine("command executable not found: $($Command.executable)")
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
    $observedPassedTests = $null
    $minimumProperty = $Command.PSObject.Properties["minimumPassedTests"]
    if ($null -ne $minimumProperty) {
        $minimumPassedTests = [int] $minimumProperty.Value
        $observedPassedTests = 0
        foreach ($line in $outputLines) {
            if ($line -match "^\s*test result: ok\.\s+([0-9]+) passed;") {
                $observedPassedTests += [int] $Matches[1]
            }
        }
        if ($exitCode -eq 0 -and $observedPassedTests -lt $minimumPassedTests) {
            $exitCode = 86
            [Console]::Error.WriteLine(
                "test command passed fewer tests than required: " +
                "observed=$observedPassedTests, minimum=$minimumPassedTests"
            )
        }
    }
    $timer.Stop()
    $finished = [DateTimeOffset]::Now
    return [PSCustomObject] @{
        id = [string] $Command.id
        command = [string] $Command.display
        status = if ($exitCode -eq 0) { "pass" } else { "fail" }
        exitCode = $exitCode
        startedAt = $started.ToString("o")
        finishedAt = $finished.ToString("o")
        durationMs = [Math]::Round($timer.Elapsed.TotalMilliseconds, 3)
        observedPassedTests = $observedPassedTests
        sourceRevision = $SourceRevision
        sourceTreeDigest = $SourceTreeDigest
        buildId = $BuildId
        runnerOs = $RunnerOs
    }
}

$resolvedRoot = [IO.Path]::GetFullPath($RepositoryRoot)
if (-not (Test-Path -LiteralPath $resolvedRoot -PathType Container)) {
    [Console]::Error.WriteLine("repository root does not exist: $resolvedRoot")
    exit 1
}
$gateSlug = $GateId.ToLowerInvariant()
$resolvedMatrix = if ([string]::IsNullOrWhiteSpace($MatrixPath)) {
    $matrixName = if ($GateId -eq "G1") {
        "g1-fault-matrix.json"
    }
    else {
        "$gateSlug-quality-matrix.json"
    }
    Join-Path $resolvedRoot "release\gates\$matrixName"
}
elseif ([IO.Path]::IsPathRooted($MatrixPath)) {
    [IO.Path]::GetFullPath($MatrixPath)
}
else {
    [IO.Path]::GetFullPath((Join-Path $resolvedRoot $MatrixPath))
}
$resolvedRequirements = if ([string]::IsNullOrWhiteSpace($RequirementsPath)) {
    Join-Path $resolvedRoot "release\gates\$gateSlug-evidence-requirements.json"
}
elseif ([IO.Path]::IsPathRooted($RequirementsPath)) {
    [IO.Path]::GetFullPath($RequirementsPath)
}
else {
    [IO.Path]::GetFullPath((Join-Path $resolvedRoot $RequirementsPath))
}
$commands = @(
    Read-And-ValidateMatrix `
        -Path $resolvedMatrix `
        -RequirementsPath $resolvedRequirements `
        -ExpectedGateId $GateId
)
$resolvedOutput = if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    Join-Path $resolvedRoot "artifacts\$gateSlug"
}
elseif ([IO.Path]::IsPathRooted($OutputRoot)) {
    [IO.Path]::GetFullPath($OutputRoot)
}
else {
    [IO.Path]::GetFullPath((Join-Path $resolvedRoot $OutputRoot))
}
New-Item -ItemType Directory -Path $resolvedOutput -Force | Out-Null

$revisionLines = @(Get-GitLines -Root $resolvedRoot -Arguments @("rev-parse", "HEAD"))
if ($revisionLines.Count -ne 1) {
    throw "git rev-parse HEAD must return exactly one line"
}
$sourceRevision = ([string] $revisionLines[0]).Trim()
$sourceTreeDigest = Get-SourceTreeDigest -Root $resolvedRoot
$runnerOs = [Environment]::OSVersion.VersionString
$rustc = Invoke-ShortCommand -Executable "rustc" -Arguments @("--version")
$cargo = Invoke-ShortCommand -Executable "cargo" -Arguments @("--version")
$buildId = Get-Sha256Text -Value (
    "$sourceRevision`n$sourceTreeDigest`n$rustc`n$cargo`n$runnerOs"
)
$startedAt = [DateTimeOffset]::Now
$records = New-Object System.Collections.Generic.List[object]
$failureCount = 0
foreach ($command in $commands) {
    $record = Invoke-EvidenceCommand `
        -Command $command `
        -SourceRevision $sourceRevision `
        -SourceTreeDigest $sourceTreeDigest `
        -BuildId $buildId `
        -RunnerOs $runnerOs `
        -EvidenceGateId $GateId
    $records.Add($record)
    if ($record.exitCode -ne 0) {
        $failureCount++
    }
}
$finishedAt = [DateTimeOffset]::Now
$evidence = [PSCustomObject] @{
    schemaVersion = 1
    gateId = $GateId
    sourceRevision = $sourceRevision
    sourceTreeDigest = $sourceTreeDigest
    buildId = $buildId
    startedAt = $startedAt.ToString("o")
    finishedAt = $finishedAt.ToString("o")
    runner = [PSCustomObject] @{
        os = $runnerOs
        architecture = if ([IntPtr]::Size -eq 8) { "64-bit" } else { "32-bit" }
        powershell = $PSVersionTable.PSVersion.ToString()
        rustc = $rustc
        cargo = $cargo
    }
    evidence = $records.ToArray()
    failureCount = $failureCount
    status = if ($failureCount -eq 0) { "pass" } else { "fail" }
}
$evidencePath = Join-Path $resolvedOutput "evidence.json"
Write-Utf8Json -Path $evidencePath -Value $evidence
Write-Output (
    "$GateId evidence collected: status=$($evidence.status), " +
    "failures=$failureCount, buildId=$buildId, output=$evidencePath"
)
if ($failureCount -gt 0) {
    exit 1
}
exit 0
