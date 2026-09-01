[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $OutputRoot,
    [ValidateSet("all", "postgresql", "mysql", "mariadb")]
    [string] $Engine = "all",
    [switch] $ValidateOnly
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
    Join-Path $resolvedRoot "artifacts\g2"
}
elseif ([IO.Path]::IsPathRooted($OutputRoot)) {
    [IO.Path]::GetFullPath($OutputRoot)
}
else {
    [IO.Path]::GetFullPath((Join-Path $resolvedRoot $OutputRoot))
}
$contracts = @(
    [PSCustomObject] @{
        engine = "postgresql"
        image = "postgres@sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a"
        dumpTool = "pg_dump"
        restoreTool = "pg_restore"
    },
    [PSCustomObject] @{
        engine = "mysql"
        image = "mysql@sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb"
        dumpTool = "mysqldump"
        restoreTool = "mysql"
    },
    [PSCustomObject] @{
        engine = "mariadb"
        image = "mariadb@sha256:a794d9eb009e20de605858a11f32f63b4075cbd197c650436f0e3b457e4caed7"
        dumpTool = "mariadb-dump"
        restoreTool = "mariadb"
    }
)

if ($ValidateOnly) {
    [PSCustomObject] @{
        schemaVersion = 1
        gateId = "G2-selective-scope"
        mode = "validate_only"
        engines = @(
            $contracts | ForEach-Object {
                [PSCustomObject] @{
                    engine = $_.engine
                    image = $_.image
                    dumpTool = $_.dumpTool
                    restoreTool = $_.restoreTool
                }
            }
        )
        pipeline = [PSCustomObject] @{
            compressionFormat = "zstd"
            encryptionFormat = "chacha20poly1305_stream"
            signedSetManifestRequired = $true
            dekWrapRoundTripRequired = $true
            requiredReplicaReaderRoundTrip = $true
            maxPlaintextBytes = 67108864
            payloadExcludedFromEvidence = $true
        }
    } | ConvertTo-Json -Depth 8
    exit 0
}

New-Item -ItemType Directory -Path $resolvedOutput -Force | Out-Null
$dockerCommand = Get-Command -Name "docker" -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($null -eq $dockerCommand) {
    throw "docker executable is required"
}

function Write-Utf8Json {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [object] $Value
    )

    $json = $Value | ConvertTo-Json -Depth 32
    [IO.File]::WriteAllText($Path, $json, [Text.UTF8Encoding]::new($false))
}

function Invoke-DockerText {
    param([Parameter(Mandatory = $true)] [string[]] $Arguments)

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        $lines = @(& $dockerCommand.Source @Arguments 2>&1)
        $exitCode = [int] $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    if ($exitCode -ne 0) {
        $detail = @($lines | Select-Object -Last 16 | ForEach-Object { $_.ToString() }) -join "`n"
        throw "docker command failed (exit=$exitCode): $detail"
    }
    return (@($lines | ForEach-Object { $_.ToString() }) -join "`n").Trim()
}

function Invoke-DockerQuiet {
    param([Parameter(Mandatory = $true)] [string[]] $Arguments)

    Invoke-DockerText -Arguments $Arguments | Out-Null
}

function Test-DockerCommand {
    param([Parameter(Mandatory = $true)] [string[]] $Arguments)

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        & $dockerCommand.Source @Arguments 1>$null 2>$null
        return $LASTEXITCODE -eq 0
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
}

function Normalize-Oracle {
    param([AllowEmptyString()] [string] $Value)

    return (@(
            ($Value -replace "`r", "") -split "`n" |
                ForEach-Object { $_.Trim() } |
                Where-Object { $_.Length -gt 0 }
        ) -join "`n")
}

function Assert-OracleMarkers {
    param(
        [Parameter(Mandatory = $true)] [string] $Oracle,
        [Parameter(Mandatory = $true)] [string[]] $Expected
    )

    $lines = @($Oracle -split "`n")
    foreach ($marker in $Expected) {
        if ($lines -cnotcontains $marker) {
            throw "oracle marker missing: $marker`n$Oracle"
        }
    }
}

function Get-FileSha256 {
    param([Parameter(Mandatory = $true)] [string] $Path)

    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-TextSha256 {
    param([AllowEmptyString()] [string] $Value)

    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($Value)
        return ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
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
            "ls-files", "--cached", "--others", "--exclude-standard"
        )
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
    return Get-TextSha256 -Value $manifest.ToString()
}

function Invoke-VersionLine {
    param([Parameter(Mandatory = $true)] [string] $Executable)

    $application = Get-Command -Name $Executable -CommandType Application `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $application) {
        return "unavailable"
    }
    $lines = @(& $application.Source --version 2>&1)
    if ($LASTEXITCODE -ne 0 -or $lines.Count -eq 0) {
        return "unavailable"
    }
    return $lines[0].ToString().Trim()
}

function New-BuildIdentity {
    $revisionLines = @(Get-GitLines -Arguments @("rev-parse", "HEAD"))
    if ($revisionLines.Count -ne 1 -or
        $revisionLines[0].Trim() -notmatch "^[0-9a-f]{40}$") {
        throw "git source revision is unavailable"
    }
    $sourceRevision = $revisionLines[0].Trim()
    $sourceTreeDigest = Get-SourceTreeDigest
    $runnerOs = [Environment]::OSVersion.VersionString
    $rustc = Invoke-VersionLine -Executable "rustc"
    $cargo = Invoke-VersionLine -Executable "cargo"
    return [PSCustomObject] @{
        sourceRevision = $sourceRevision
        sourceTreeDigest = $sourceTreeDigest
        buildId = Get-TextSha256 -Value (
            "$sourceRevision`n$sourceTreeDigest`n$rustc`n$cargo`n$runnerOs"
        )
        runnerOs = $runnerOs
        runnerArchitecture = [Runtime.InteropServices.RuntimeInformation]::
            OSArchitecture.ToString()
        rustc = $rustc
        cargo = $cargo
    }
}

function Get-PinnedImageEvidence {
    param([Parameter(Mandatory = $true)] [string] $Image)

    if ($Image -notmatch "@sha256:[0-9a-f]{64}$") {
        throw "image reference is not digest pinned"
    }
    Invoke-DockerQuiet -Arguments @("pull", "--platform", "linux/amd64", $Image)
    $repoDigestsJson = Invoke-DockerText -Arguments @(
        "image", "inspect", "--format", "{{json .RepoDigests}}", $Image
    )
    $parsedRepoDigests = $repoDigestsJson | ConvertFrom-Json
    $repoDigests = @($parsedRepoDigests | ForEach-Object { $_ })
    if ($repoDigests -cnotcontains $Image) {
        throw "pulled image does not expose the required repository digest"
    }
    $identity = @(
        (Invoke-DockerText -Arguments @(
                "image", "inspect", "--format", "{{.Id}}`n{{.Os}}`n{{.Architecture}}", $Image
            )) -split "`n"
    )
    if ($identity.Count -ne 3 -or $identity[1] -ne "linux" -or $identity[2] -ne "amd64") {
        throw "pinned image platform identity is invalid"
    }
    return [PSCustomObject] @{
        reference = $Image
        digest = ($Image -split "@", 2)[1]
        imageId = $identity[0]
        os = $identity[1]
        architecture = $identity[2]
    }
}

function Get-ToolVersion {
    param(
        [Parameter(Mandatory = $true)] [string] $Container,
        [Parameter(Mandatory = $true)] [string] $Tool
    )

    return ((Invoke-DockerText -Arguments @("exec", $Container, $Tool, "--version")) -split "`n")[0].Trim()
}

function Get-ToolSha256 {
    param(
        [Parameter(Mandatory = $true)] [string] $Container,
        [Parameter(Mandatory = $true)] [string] $Tool
    )

    if ($Tool -notmatch "^[a-z0-9_-]+$") {
        throw "native tool name is invalid"
    }
    $toolPath = Invoke-DockerText -Arguments @(
        "exec", $Container, "sh", "-c", "command -v $Tool"
    )
    if ($toolPath -notmatch "^/[A-Za-z0-9_./+-]+$") {
        throw "native tool path is invalid"
    }
    $resolvedToolPath = Invoke-DockerText -Arguments @(
        "exec", $Container, "readlink", "-f", $toolPath
    )
    if ($resolvedToolPath -notmatch "^/[A-Za-z0-9_./+-]+$") {
        throw "resolved native tool path is invalid"
    }
    $digestLine = Invoke-DockerText -Arguments @(
        "exec", $Container, "sha256sum", $resolvedToolPath
    )
    if ($digestLine -notmatch "^(?<digest>[0-9a-f]{64})\s") {
        throw "native tool digest is invalid"
    }
    return $Matches.digest
}

function Invoke-SelectivePipelineRoundTrip {
    param(
        [Parameter(Mandatory = $true)] [string] $EngineName,
        [Parameter(Mandatory = $true)] [string] $InputPath,
        [Parameter(Mandatory = $true)] [string] $DumpFormat,
        [Parameter(Mandatory = $true)] [string] $ScopeIdentity,
        [Parameter(Mandatory = $true)] [string] $ToolName,
        [Parameter(Mandatory = $true)] [string] $ToolVersion,
        [Parameter(Mandatory = $true)] [string] $ToolSha256,
        [Parameter(Mandatory = $true)] [object] $BuildIdentity,
        [Parameter(Mandatory = $true)] [string] $TemporaryRoot
    )

    $cargoCommand = Get-Command -Name "cargo" -CommandType Application `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $cargoCommand) {
        throw "cargo executable is required for selective pipeline evidence"
    }
    $engineToken = switch ($EngineName) {
        "postgresql" { "pg" }
        "mysql" { "my" }
        "mariadb" { "ma" }
        default { throw "unsupported selective pipeline engine" }
    }
    $pipelineRoot = Join-Path $TemporaryRoot "p-$engineToken"
    New-Item -ItemType Directory -Path $pipelineRoot -Force | Out-Null
    $destinationRoot = Join-Path $pipelineRoot "r"
    $recoveredPath = Join-Path $pipelineRoot "o"
    $evidencePath = Join-Path $pipelineRoot "e.json"
    $arguments = @(
        "run", "--locked", "--quiet", "-p", "restoreplane-pipeline",
        "--example", "selective_scope_pipeline", "--",
        "--input", $InputPath,
        "--destination", $destinationRoot,
        "--recovered", $recoveredPath,
        "--evidence", $evidencePath,
        "--engine", $EngineName,
        "--dump-format", $DumpFormat,
        "--scope-identity", $ScopeIdentity,
        "--tool-name", $ToolName,
        "--tool-version", $ToolVersion,
        "--tool-sha256", $ToolSha256,
        "--source-revision", $BuildIdentity.sourceRevision,
        "--source-tree-digest", $BuildIdentity.sourceTreeDigest,
        "--build-id", $BuildIdentity.buildId
    )
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    Push-Location $resolvedRoot
    try {
        $script:LASTEXITCODE = 0
        $lines = @(& $cargoCommand.Source @arguments 2>&1)
        $exitCode = [int] $LASTEXITCODE
    }
    finally {
        Pop-Location
        $ErrorActionPreference = $previousErrorAction
    }
    if ($exitCode -ne 0) {
        $detail = @($lines | Select-Object -Last 20 | ForEach-Object { $_.ToString() }) -join "`n"
        throw "selective RestorePlane pipeline failed (exit=$exitCode): $detail"
    }
    if (-not (Test-Path -LiteralPath $recoveredPath -PathType Leaf) -or
        -not (Test-Path -LiteralPath $evidencePath -PathType Leaf)) {
        throw "selective RestorePlane pipeline did not commit recovery evidence"
    }
    $evidence = Get-Content -LiteralPath $evidencePath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    if ($evidence.schemaVersion -ne 1 -or
        $evidence.engine -ne $EngineName -or
        $evidence.sourceRevision -ne $BuildIdentity.sourceRevision -or
        $evidence.sourceTreeDigest -ne $BuildIdentity.sourceTreeDigest -or
        $evidence.buildId -ne $BuildIdentity.buildId -or
        $evidence.scopeMode -ne "explicit_tables" -or
        $evidence.scopeIdentity -ne $ScopeIdentity -or
        $evidence.dumpFormat -ne $DumpFormat -or
        $evidence.toolName -ne $ToolName -or
        $evidence.toolExecutableSha256 -ne $ToolSha256 -or
        $evidence.compressionFormat -ne "zstd" -or
        $evidence.encryptionFormat -ne "chacha20poly1305_stream" -or
        $evidence.dekWrapGeneration -ne 1 -or
        $evidence.setManifestSchemaVersion -ne 2 -or
        $evidence.signedSetManifestVerified -ne $true -or
        $evidence.dekWrapRoundTripVerified -ne $true -or
        $evidence.requiredReplicaReaderEligible -ne $true -or
        $evidence.payloadReaderRoundTripVerified -ne $true -or
        $evidence.manifestReaderRoundTripVerified -ne $true -or
        $evidence.requiredReplicaRequirement -ne "required" -or
        $evidence.requiredReplicaReaderStatus -ne "eligible") {
        throw "selective RestorePlane pipeline evidence contract is invalid"
    }
    if ((Get-FileSha256 -Path $InputPath) -ne
        (Get-FileSha256 -Path $recoveredPath)) {
        throw "selective RestorePlane pipeline recovery bytes do not match the native dump"
    }
    return [PSCustomObject] @{
        recoveredPath = $recoveredPath
        evidence = $evidence
    }
}

function Wait-ContainerInitialization {
    param([Parameter(Mandatory = $true)] [string] $Container)

    for ($attempt = 1; $attempt -le 180; $attempt++) {
        $logs = Invoke-DockerText -Arguments @("logs", $Container)
        if ($logs -match "(?is)init process (complete|done).*database system is ready to accept connections" -or
            $logs -match "(?is)init process (complete|done).*ready for connections") {
            return
        }
        $running = Invoke-DockerText -Arguments @(
            "inspect", "--format", "{{.State.Running}}", $Container
        )
        if ($running -ne "true") {
            throw "database fixture container stopped during initialization"
        }
        Start-Sleep -Seconds 1
    }
    throw "database fixture final initialization did not complete"
}

function Wait-Postgres {
    param(
        [Parameter(Mandatory = $true)] [string] $Container,
        [Parameter(Mandatory = $true)] [string] $User,
        [Parameter(Mandatory = $true)] [string] $Database
    )

    Wait-ContainerInitialization -Container $Container
    $consecutiveReady = 0
    for ($attempt = 1; $attempt -le 120; $attempt++) {
        if (Test-DockerCommand -Arguments @(
                "exec", $Container, "pg_isready", "-q", "-h", "localhost",
                "-U", $User, "-d", $Database
            )) {
            $consecutiveReady++
            if ($consecutiveReady -ge 2) {
                return
            }
        }
        else {
            $consecutiveReady = 0
        }
        Start-Sleep -Seconds 1
    }
    throw "PostgreSQL fixture did not become ready"
}

function Invoke-MySqlQuery {
    param(
        [Parameter(Mandatory = $true)] [string] $Container,
        [Parameter(Mandatory = $true)] [string] $Client,
        [AllowEmptyString()] [string] $Database,
        [Parameter(Mandatory = $true)] [string] $Sql,
        [Parameter(Mandatory = $true)] [string] $Password
    )

    $arguments = @(
        "exec", "--env", "MYSQL_PWD=$Password", $Container, $Client,
        "--user=root", "--batch", "--skip-column-names"
    )
    if (-not [string]::IsNullOrWhiteSpace($Database)) {
        $arguments += @("--database", $Database)
    }
    $arguments += @("--execute", $Sql)
    return Invoke-DockerText -Arguments $arguments
}

function Wait-MySqlFamily {
    param(
        [Parameter(Mandatory = $true)] [string] $Container,
        [Parameter(Mandatory = $true)] [string] $Client,
        [Parameter(Mandatory = $true)] [string] $Database,
        [Parameter(Mandatory = $true)] [string] $Password
    )

    Wait-ContainerInitialization -Container $Container
    $consecutiveReady = 0
    for ($attempt = 1; $attempt -le 180; $attempt++) {
        if (Test-DockerCommand -Arguments @(
                "exec", "--env", "MYSQL_PWD=$Password", $Container, $Client,
                "--user=root", "--batch", "--skip-column-names",
                "--database", $Database, "--execute", "SELECT 1"
            )) {
            $consecutiveReady++
            if ($consecutiveReady -ge 2) {
                return
            }
        }
        else {
            $consecutiveReady = 0
        }
        Start-Sleep -Seconds 1
    }
    throw "MySQL-family fixture did not become ready"
}

function Remove-TestContainers {
    param([System.Collections.Generic.List[string]] $Containers)

    foreach ($container in $Containers) {
        & $dockerCommand.Source rm --force $container 1>$null 2>$null
    }
}

function Invoke-PostgresqlSelectiveRoundTrip {
    param(
        [Parameter(Mandatory = $true)] [object] $Contract,
        [Parameter(Mandatory = $true)] [object] $BuildIdentity,
        [Parameter(Mandatory = $true)] [string] $RunId,
        [Parameter(Mandatory = $true)] [string] $TemporaryRoot
    )

    $password = "restoreplane_g2_scope_ephemeral"
    $sourceContainer = "restoreplane-g2-scope-pg-source-$RunId"
    $targetContainer = "restoreplane-g2-scope-pg-target-$RunId"
    $containers = [System.Collections.Generic.List[string]]::new()
    $dumpPath = Join-Path $TemporaryRoot "postgresql-selected.dump"
    try {
        $imageEvidence = Get-PinnedImageEvidence -Image $Contract.image
        Invoke-DockerQuiet -Arguments @(
            "run", "--detach", "--rm", "--pull=never", "--name", $sourceContainer,
            "--env", "POSTGRES_USER=source_admin",
            "--env", "POSTGRES_PASSWORD=$password",
            "--env", "POSTGRES_DB=source_db",
            $Contract.image
        )
        $containers.Add($sourceContainer)
        Wait-Postgres -Container $sourceContainer -User "source_admin" -Database "source_db"

        $fixtureSql = @'
CREATE TABLE public.selected_records(id integer PRIMARY KEY, note text NOT NULL);
INSERT INTO public.selected_records VALUES (1, 'alpha'), (2, 'beta');
CREATE TABLE public.excluded_records(id integer PRIMARY KEY, note text NOT NULL);
INSERT INTO public.excluded_records VALUES (9, 'must-not-restore');
CREATE FUNCTION public.scope_wide_helper() RETURNS text
LANGUAGE sql IMMUTABLE AS $$ SELECT 'must-not-restore'::text $$;
'@
        Invoke-DockerQuiet -Arguments @(
            "exec", "--env", "PGPASSWORD=$password", $sourceContainer,
            "psql", "-v", "ON_ERROR_STOP=1", "-h", "localhost", "-U", "source_admin",
            "-d", "source_db", "-c", $fixtureSql
        )
        $sourceOracleSql = (
            "SELECT 'selected_rows=' || count(*) FROM public.selected_records; " +
            "SELECT 'excluded_table=' || CASE WHEN to_regclass('public.excluded_records') IS NULL THEN 'absent' ELSE 'present' END; " +
            "SELECT 'routine=' || CASE WHEN to_regprocedure('public.scope_wide_helper()') IS NULL THEN 'absent' ELSE 'present' END;"
        )
        $sourceOracle = Normalize-Oracle -Value (Invoke-DockerText -Arguments @(
                "exec", "--env", "PGPASSWORD=$password", $sourceContainer,
                "psql", "-At", "-v", "ON_ERROR_STOP=1", "-h", "localhost",
                "-U", "source_admin", "-d", "source_db", "-c",
                $sourceOracleSql
            ))
        Assert-OracleMarkers -Oracle $sourceOracle -Expected @(
            "selected_rows=2", "excluded_table=present", "routine=present"
        )

        $dumpArguments = @(
            "-h", "localhost", "-p", "5432", "-U", "source_admin", "--no-password",
            "-Fc", "-f", "/tmp/selected.dump", "--strict-names",
            '--table="public"."selected_records"', "source_db"
        )
        Invoke-DockerQuiet -Arguments (@(
                "exec", "--env", "PGPASSWORD=$password", $sourceContainer,
                $Contract.dumpTool
            ) + $dumpArguments)
        Invoke-DockerQuiet -Arguments @(
            "exec", $sourceContainer, $Contract.restoreTool, "--list", "/tmp/selected.dump"
        )
        $toc = Normalize-Oracle -Value (Invoke-DockerText -Arguments @(
                "exec", $sourceContainer, $Contract.restoreTool, "--list", "/tmp/selected.dump"
            ))
        if ($toc -notmatch "selected_records" -or
            $toc -match "excluded_records|scope_wide_helper") {
            throw "PostgreSQL selective archive TOC contains the wrong objects"
        }
        Invoke-DockerQuiet -Arguments @("cp", "${sourceContainer}:/tmp/selected.dump", $dumpPath)
        if ((Get-Item -LiteralPath $dumpPath).Length -le 0) {
            throw "PostgreSQL selective dump is empty"
        }
        $dumpToolVersion = Get-ToolVersion -Container $sourceContainer `
            -Tool $Contract.dumpTool
        $dumpToolSha256 = Get-ToolSha256 -Container $sourceContainer `
            -Tool $Contract.dumpTool
        $pipeline = Invoke-SelectivePipelineRoundTrip `
            -EngineName "postgresql" `
            -InputPath $dumpPath `
            -DumpFormat "custom" `
            -ScopeIdentity "public.selected_records" `
            -ToolName $Contract.dumpTool `
            -ToolVersion $dumpToolVersion `
            -ToolSha256 $dumpToolSha256 `
            -BuildIdentity $BuildIdentity `
            -TemporaryRoot $TemporaryRoot

        Invoke-DockerQuiet -Arguments @(
            "run", "--detach", "--rm", "--pull=never", "--name", $targetContainer,
            "--env", "POSTGRES_USER=target_admin",
            "--env", "POSTGRES_PASSWORD=$password",
            "--env", "POSTGRES_DB=target_admin",
            $Contract.image
        )
        $containers.Add($targetContainer)
        Wait-Postgres -Container $targetContainer -User "target_admin" -Database "target_admin"
        Invoke-DockerQuiet -Arguments @(
            "exec", "--env", "PGPASSWORD=$password", $targetContainer,
            "createdb", "-h", "localhost", "-U", "target_admin", "restored_db"
        )
        Invoke-DockerQuiet -Arguments @(
            "cp", $pipeline.recoveredPath, "${targetContainer}:/tmp/selected.dump"
        )
        Invoke-DockerQuiet -Arguments @(
            "exec", "--env", "PGPASSWORD=$password", $targetContainer,
            $Contract.restoreTool, "-h", "localhost", "-U", "target_admin",
            "-d", "restored_db", "--exit-on-error", "--single-transaction",
            "--no-owner", "--no-acl", "/tmp/selected.dump"
        )
        $restoredOracleSql = (
            "SELECT 'selected_rows=' || count(*) FROM public.selected_records; " +
            "SELECT 'selected_values=' || string_agg(id::text || ':' || note, ',' ORDER BY id) FROM public.selected_records; " +
            "SELECT 'excluded_table=' || CASE WHEN to_regclass('public.excluded_records') IS NULL THEN 'absent' ELSE 'present' END; " +
            "SELECT 'routine=' || CASE WHEN to_regprocedure('public.scope_wide_helper()') IS NULL THEN 'absent' ELSE 'present' END;"
        )
        $restoredOracle = Normalize-Oracle -Value (Invoke-DockerText -Arguments @(
                "exec", "--env", "PGPASSWORD=$password", $targetContainer,
                "psql", "-At", "-v", "ON_ERROR_STOP=1", "-h", "localhost",
                "-U", "target_admin", "-d", "restored_db", "-c",
                $restoredOracleSql
            ))
        Assert-OracleMarkers -Oracle $restoredOracle -Expected @(
            "selected_rows=2", "selected_values=1:alpha,2:beta",
            "excluded_table=absent", "routine=absent"
        )

        return [PSCustomObject] @{
            engine = "postgresql"
            status = "pass"
            image = $imageEvidence
            tools = [PSCustomObject] @{
                dump = $dumpToolVersion
                dumpExecutableSha256 = $dumpToolSha256
                restore = Get-ToolVersion -Container $targetContainer -Tool $Contract.restoreTool
            }
            scope = [PSCustomObject] @{
                mode = "explicit_tables"
                database = "source_db"
                selectedTables = @("public.selected_records")
                excludedObjects = @("public.excluded_records", "public.scope_wide_helper()")
            }
            dump = [PSCustomObject] @{
                nativeProcessCount = 1
                format = "custom"
                arguments = $dumpArguments
                strictNames = $true
                globalsDumpInvocations = 0
                bytes = (Get-Item -LiteralPath $dumpPath).Length
                sha256 = Get-FileSha256 -Path $dumpPath
                tocSha256 = Get-TextSha256 -Value $toc
            }
            pipeline = $pipeline.evidence
            restore = [PSCustomObject] @{
                cleanDatabase = $true
                database = "restored_db"
                oracle = $restoredOracle -split "`n"
                oracleSha256 = Get-TextSha256 -Value $restoredOracle
            }
            assertions = [PSCustomObject] @{
                selectedContentRestored = $true
                excludedTableAbsent = $true
                databaseRoutineAbsent = $true
                databaseGlobalsOmitted = $true
            }
        }
    }
    finally {
        Remove-TestContainers -Containers $containers
    }
}

function Invoke-MySqlFamilySelectiveRoundTrip {
    param(
        [Parameter(Mandatory = $true)] [object] $Contract,
        [Parameter(Mandatory = $true)] [object] $BuildIdentity,
        [Parameter(Mandatory = $true)] [string] $RunId,
        [Parameter(Mandatory = $true)] [string] $TemporaryRoot
    )

    $engineName = [string] $Contract.engine
    $isMaria = $engineName -eq "mariadb"
    $client = if ($isMaria) { "mariadb" } else { "mysql" }
    $password = "restoreplane_g2_scope_ephemeral"
    $sourceContainer = "restoreplane-g2-scope-$engineName-source-$RunId"
    $targetContainer = "restoreplane-g2-scope-$engineName-target-$RunId"
    $containers = [System.Collections.Generic.List[string]]::new()
    $dumpPath = Join-Path $TemporaryRoot "$engineName-selected.sql"
    try {
        $imageEvidence = Get-PinnedImageEvidence -Image $Contract.image
        $rootPassword = if ($isMaria) {
            "MARIADB_ROOT_PASSWORD=$password"
        }
        else {
            "MYSQL_ROOT_PASSWORD=$password"
        }
        $sourceDatabase = if ($isMaria) {
            "MARIADB_DATABASE=source_db"
        }
        else {
            "MYSQL_DATABASE=source_db"
        }
        Invoke-DockerQuiet -Arguments @(
            "run", "--detach", "--rm", "--pull=never", "--name", $sourceContainer,
            "--env", $rootPassword, "--env", $sourceDatabase,
            $Contract.image, "--event-scheduler=ON", "--local-infile=0"
        )
        $containers.Add($sourceContainer)
        Wait-MySqlFamily -Container $sourceContainer -Client $client `
            -Database "source_db" -Password $password

        $fixtureSql = @'
CREATE TABLE selected_records(id INT PRIMARY KEY, note VARCHAR(64) NOT NULL) ENGINE=InnoDB;
CREATE TRIGGER selected_records_bi BEFORE INSERT ON selected_records
FOR EACH ROW SET NEW.note = UPPER(NEW.note);
INSERT INTO selected_records VALUES (1, 'alpha'), (2, 'beta');
CREATE TABLE excluded_records(id INT PRIMARY KEY, note VARCHAR(64) NOT NULL) ENGINE=InnoDB;
INSERT INTO excluded_records VALUES (9, 'must-not-restore');
CREATE PROCEDURE scope_wide_procedure() SELECT 'must-not-restore';
CREATE EVENT scope_wide_event ON SCHEDULE EVERY 1 DAY
STARTS CURRENT_TIMESTAMP + INTERVAL 1 DAY DO SET @scope_event_marker = 1;
'@
        Invoke-MySqlQuery -Container $sourceContainer -Client $client -Database "source_db" `
            -Sql $fixtureSql -Password $password | Out-Null
        $sourceOracleSql = @'
SELECT CONCAT('selected_rows=', COUNT(*)) FROM selected_records;
SELECT CONCAT('excluded_table=', COUNT(*)) FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'excluded_records';
SELECT CONCAT('routine=', COUNT(*)) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE() AND ROUTINE_NAME = 'scope_wide_procedure';
SELECT CONCAT('event=', COUNT(*)) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = DATABASE() AND EVENT_NAME = 'scope_wide_event';
SELECT CONCAT('trigger=', COUNT(*)) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = DATABASE() AND TRIGGER_NAME = 'selected_records_bi';
'@
        $sourceOracle = Normalize-Oracle -Value (Invoke-MySqlQuery `
                -Container $sourceContainer -Client $client -Database "source_db" `
                -Sql $sourceOracleSql -Password $password)
        Assert-OracleMarkers -Oracle $sourceOracle -Expected @(
            "selected_rows=2", "excluded_table=1", "routine=1", "event=1", "trigger=1"
        )

        $dumpArguments = @(
            "--user=root", "--single-transaction", "--triggers",
            "--default-character-set=utf8mb4", "--result-file=/tmp/selected.sql",
            "--", "source_db", "selected_records"
        )
        Invoke-DockerQuiet -Arguments (@(
                "exec", "--env", "MYSQL_PWD=$password", $sourceContainer, $Contract.dumpTool
            ) + $dumpArguments)
        Invoke-DockerQuiet -Arguments @("cp", "${sourceContainer}:/tmp/selected.sql", $dumpPath)
        if ((Get-Item -LiteralPath $dumpPath).Length -le 0) {
            throw "$engineName selective dump is empty"
        }
        $dumpText = Get-Content -LiteralPath $dumpPath -Raw
        if ($dumpText -notmatch "selected_records" -or
            $dumpText -match "excluded_records|scope_wide_procedure|scope_wide_event") {
            throw "$engineName selective dump contains the wrong objects"
        }
        $dumpToolVersion = Get-ToolVersion -Container $sourceContainer `
            -Tool $Contract.dumpTool
        $dumpToolSha256 = Get-ToolSha256 -Container $sourceContainer `
            -Tool $Contract.dumpTool
        $pipeline = Invoke-SelectivePipelineRoundTrip `
            -EngineName $engineName `
            -InputPath $dumpPath `
            -DumpFormat "plain" `
            -ScopeIdentity "source_db.selected_records" `
            -ToolName $Contract.dumpTool `
            -ToolVersion $dumpToolVersion `
            -ToolSha256 $dumpToolSha256 `
            -BuildIdentity $BuildIdentity `
            -TemporaryRoot $TemporaryRoot

        $targetDatabase = if ($isMaria) {
            "MARIADB_DATABASE=restored_db"
        }
        else {
            "MYSQL_DATABASE=restored_db"
        }
        Invoke-DockerQuiet -Arguments @(
            "run", "--detach", "--rm", "--pull=never", "--name", $targetContainer,
            "--env", $rootPassword, "--env", $targetDatabase,
            $Contract.image, "--event-scheduler=ON", "--local-infile=0"
        )
        $containers.Add($targetContainer)
        Wait-MySqlFamily -Container $targetContainer -Client $client `
            -Database "restored_db" -Password $password
        $emptyTarget = Normalize-Oracle -Value (Invoke-MySqlQuery `
                -Container $targetContainer -Client $client -Database "restored_db" `
                -Sql "SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE();" `
                -Password $password)
        if ($emptyTarget -ne "0") {
            throw "$engineName restore target is not clean: $emptyTarget"
        }
        Invoke-DockerQuiet -Arguments @(
            "cp", $pipeline.recoveredPath, "${targetContainer}:/tmp/selected.sql"
        )
        Invoke-MySqlQuery -Container $targetContainer -Client $client -Database "restored_db" `
            -Sql "source /tmp/selected.sql" -Password $password | Out-Null
        $restoredOracleSql = @'
SELECT CONCAT('selected_rows=', COUNT(*)) FROM selected_records;
SELECT CONCAT('selected_values=', GROUP_CONCAT(CONCAT(id, ':', note) ORDER BY id SEPARATOR ',')) FROM selected_records;
SELECT CONCAT('excluded_table=', IF(COUNT(*) = 0, 'absent', 'present')) FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'excluded_records';
SELECT CONCAT('routine=', IF(COUNT(*) = 0, 'absent', 'present')) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE() AND ROUTINE_NAME = 'scope_wide_procedure';
SELECT CONCAT('event=', IF(COUNT(*) = 0, 'absent', 'present')) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = DATABASE() AND EVENT_NAME = 'scope_wide_event';
SELECT CONCAT('trigger=', IF(COUNT(*) = 1, 'present', 'absent')) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = DATABASE() AND TRIGGER_NAME = 'selected_records_bi';
INSERT INTO selected_records VALUES (3, 'gamma');
SELECT CONCAT('trigger_effect=', note) FROM selected_records WHERE id = 3;
'@
        $restoredOracle = Normalize-Oracle -Value (Invoke-MySqlQuery `
                -Container $targetContainer -Client $client -Database "restored_db" `
                -Sql $restoredOracleSql -Password $password)
        Assert-OracleMarkers -Oracle $restoredOracle -Expected @(
            "selected_rows=2", "selected_values=1:ALPHA,2:BETA",
            "excluded_table=absent", "routine=absent", "event=absent",
            "trigger=present", "trigger_effect=GAMMA"
        )
        if ($dumpArguments -contains "--routines" -or $dumpArguments -contains "--events") {
            throw "$engineName explicit scope included database-wide dump flags"
        }

        return [PSCustomObject] @{
            engine = $engineName
            status = "pass"
            image = $imageEvidence
            tools = [PSCustomObject] @{
                dump = $dumpToolVersion
                dumpExecutableSha256 = $dumpToolSha256
                restore = Get-ToolVersion -Container $targetContainer -Tool $Contract.restoreTool
            }
            scope = [PSCustomObject] @{
                mode = "explicit_tables"
                database = "source_db"
                selectedTables = @("source_db.selected_records")
                excludedObjects = @(
                    "source_db.excluded_records",
                    "source_db.scope_wide_procedure()",
                    "source_db.scope_wide_event"
                )
            }
            dump = [PSCustomObject] @{
                nativeProcessCount = 1
                format = "plain"
                arguments = $dumpArguments
                routinesFlagPresent = $false
                eventsFlagPresent = $false
                triggersFlagPresent = $true
                bytes = (Get-Item -LiteralPath $dumpPath).Length
                sha256 = Get-FileSha256 -Path $dumpPath
            }
            pipeline = $pipeline.evidence
            restore = [PSCustomObject] @{
                cleanDatabase = $true
                database = "restored_db"
                oracle = $restoredOracle -split "`n"
                oracleSha256 = Get-TextSha256 -Value $restoredOracle
            }
            assertions = [PSCustomObject] @{
                selectedContentRestored = $true
                excludedTableAbsent = $true
                databaseRoutineAbsent = $true
                databaseEventAbsent = $true
                selectedTableTriggerRestored = $true
            }
        }
    }
    finally {
        Remove-TestContainers -Containers $containers
    }
}

$mutexCreated = $false
$gateMutex = [Threading.Mutex]::new(
    $false,
    "Local\restoreplane-g2-selective-scope-gate",
    [ref] $mutexCreated
)
$mutexAcquired = $false
$temporaryRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "dbs2-" + [Guid]::NewGuid().ToString("N").Substring(0, 12)
)
try {
    try {
        $mutexAcquired = $gateMutex.WaitOne(0)
    }
    catch [Threading.AbandonedMutexException] {
        $mutexAcquired = $true
    }
    if (-not $mutexAcquired) {
        throw "another selective-scope G2 collector is already running"
    }

    Invoke-DockerQuiet -Arguments @("version")
    New-Item -ItemType Directory -Path $temporaryRoot -Force | Out-Null
    $buildIdentity = New-BuildIdentity
    $runId = [Guid]::NewGuid().ToString("N").Substring(0, 12)
    $selectedContracts = if ($Engine -eq "all") {
        @($contracts)
    }
    else {
        @($contracts | Where-Object { $_.engine -eq $Engine })
    }
    $results = @(
        foreach ($contract in $selectedContracts) {
            Write-Host "G2 selective scope: $($contract.engine)"
            if ($contract.engine -eq "postgresql") {
                Invoke-PostgresqlSelectiveRoundTrip -Contract $contract `
                    -BuildIdentity $buildIdentity -RunId $runId `
                    -TemporaryRoot $temporaryRoot
            }
            else {
                Invoke-MySqlFamilySelectiveRoundTrip -Contract $contract `
                    -BuildIdentity $buildIdentity -RunId $runId `
                    -TemporaryRoot $temporaryRoot
            }
        }
    )
    if (@($results).Count -ne @($selectedContracts).Count -or
        @($results | Where-Object { $_.status -ne "pass" }).Count -ne 0) {
        throw "selective-scope evidence set is incomplete"
    }

    $dirtyLines = @(& git -C $resolvedRoot status --porcelain --untracked-files=normal)
    if ($LASTEXITCODE -ne 0) {
        throw "git source status is unavailable"
    }
    $artifact = [PSCustomObject] @{
        schemaVersion = 1
        gateId = "G2-selective-scope"
        generatedAtUtc = [DateTimeOffset]::UtcNow.ToString("o")
        sourceRevision = $buildIdentity.sourceRevision
        sourceTreeDigest = $buildIdentity.sourceTreeDigest
        buildId = $buildIdentity.buildId
        sourceTreeDirty = $dirtyLines.Count -gt 0
        engineCount = @($results).Count
        failureCount = 0
        runner = [PSCustomObject] @{
            os = $buildIdentity.runnerOs
            architecture = $buildIdentity.runnerArchitecture
            rustc = $buildIdentity.rustc
            cargo = $buildIdentity.cargo
            dockerServer = (Invoke-DockerText -Arguments @("version", "--format", "{{.Server.Version}}"))
        }
        contract = [PSCustomObject] @{
            scopeMode = "explicit_tables"
            oneNativeDumpProcessPerScope = $true
            restoreTargetMustBeClean = $true
            selectedContentMustMatch = $true
            unselectedDatabaseObjectsMustBeAbsent = $true
            compressionFormat = "zstd"
            encryptionFormat = "chacha20poly1305_stream"
            signedSetManifestRequired = $true
            dekWrapRoundTripRequired = $true
            requiredReplicaReaderRoundTrip = $true
            maxPlaintextBytes = 67108864
            payloadExcludedFromEvidence = $true
        }
        engines = $results
        status = "pass"
    }
    $serialized = $artifact | ConvertTo-Json -Depth 32
    if ($serialized.Contains("restoreplane_g2_scope_ephemeral") -or
        $serialized.Contains('"signatureEd25519"') -or
        $serialized.Contains('"verifyingKey"') -or
        $serialized.Contains('"dekWrap.')) {
        throw "selective-scope evidence contains fixture credentials"
    }
    $artifactPath = Join-Path $resolvedOutput "selective-scopes.json"
    Write-Utf8Json -Path $artifactPath -Value $artifact
    Write-Output "G2 selective-scope evidence passed: $artifactPath"
}
finally {
    if ($mutexAcquired) {
        $gateMutex.ReleaseMutex()
    }
    $gateMutex.Dispose()
    if (Test-Path -LiteralPath $temporaryRoot -PathType Container) {
        $resolvedTemporaryRoot = [IO.Path]::GetFullPath($temporaryRoot)
        $resolvedSystemTemporary = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
        if (-not $resolvedTemporaryRoot.StartsWith(
                $resolvedSystemTemporary,
                [StringComparison]::OrdinalIgnoreCase
            ) -or $resolvedTemporaryRoot -eq $resolvedSystemTemporary) {
            throw "refusing to remove an unsafe temporary path"
        }
        try {
            Remove-Item -LiteralPath $resolvedTemporaryRoot -Recurse -Force
        }
        catch {
            $longPath = "\\?\$resolvedTemporaryRoot"
            [IO.Directory]::Delete($longPath, $true)
        }
        if (Test-Path -LiteralPath $resolvedTemporaryRoot) {
            throw "selective-scope temporary payload cleanup failed"
        }
    }
}
