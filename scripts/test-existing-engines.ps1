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
$seoulLabel = ([string] [char] 0xC11C) + ([char] 0xC6B8)

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
New-Item -ItemType Directory -Path $resolvedOutput -Force | Out-Null

$mutexCreated = $false
$g2Mutex = [Threading.Mutex]::new(
    $false,
    "Local\dbbackup-g2-existing-engine-gate",
    [ref] $mutexCreated
)
try {
    $g2MutexAcquired = $g2Mutex.WaitOne(0)
}
catch [Threading.AbandonedMutexException] {
    $g2MutexAcquired = $true
}
if (-not $g2MutexAcquired) {
    throw "another G2 existing-engine collector is already running"
}

$requiredCapabilities = @(
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
$requiredContractChecks = @(
    "provider-contract",
    "catalog-certification-safety",
    "restore-verification-safety",
    "durable-verification-notification"
)
$contracts = @(
    [PSCustomObject] @{
        engine = "postgresql"
        image = "postgres@sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a"
        fixture = "scripts/fixtures/g2-postgresql.sql"
        backupTool = "pg_dump"
        restoreTool = "pg_restore"
        requiredCapabilities = $requiredCapabilities
    },
    [PSCustomObject] @{
        engine = "mysql"
        image = "mysql@sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb"
        fixture = "scripts/fixtures/g2-mysql.sql"
        backupTool = "mysqldump"
        restoreTool = "mysql"
        requiredCapabilities = $requiredCapabilities
    },
    [PSCustomObject] @{
        engine = "mariadb"
        image = "mariadb@sha256:a794d9eb009e20de605858a11f32f63b4075cbd197c650436f0e3b457e4caed7"
        fixture = "scripts/fixtures/g2-mariadb.sql"
        backupTool = "mariadb-dump"
        restoreTool = "mariadb"
        requiredCapabilities = $requiredCapabilities
    }
)

function Write-Utf8Json {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [object] $Value
    )

    $json = $Value | ConvertTo-Json -Depth 32
    [IO.File]::WriteAllText($Path, $json, [Text.UTF8Encoding]::new($false))
}

function Assert-Contract {
    foreach ($contract in $contracts) {
        if ($contract.image -notmatch "@sha256:[0-9a-f]{64}$") {
            throw "engine image is not digest pinned"
        }
        if (@($contract.requiredCapabilities).Count -ne 9 -or
            @($contract.requiredCapabilities | Select-Object -Unique).Count -ne 9) {
            throw "provider capability contract is incomplete"
        }
        $fixture = Join-Path $resolvedRoot $contract.fixture
        if (-not (Test-Path -LiteralPath $fixture -PathType Leaf)) {
            throw "engine fixture is missing"
        }
    }
    $maria = @($contracts | Where-Object { $_.engine -eq "mariadb" })
    if ($maria.Count -ne 1 -or
        $maria[0].backupTool -ne "mariadb-dump" -or
        $maria[0].restoreTool -ne "mariadb") {
        throw "MariaDB tool contract is invalid"
    }
}

Assert-Contract
if ($ValidateOnly) {
    Write-Utf8Json -Path (Join-Path $resolvedOutput "contract.json") -Value (
        [PSCustomObject] @{
            schemaVersion = 1
            gateId = "G2"
            contractChecks = $requiredContractChecks
            engines = $contracts
        }
    )
    Write-Output "G2 existing-engine contract valid"
    exit 0
}

$dockerCommand = Get-Command -Name "docker" -CommandType Application -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($null -eq $dockerCommand) {
    throw "docker executable is required"
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
        $detail = @($lines | Select-Object -Last 12 | ForEach-Object { $_.ToString() }) -join "`n"
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

function Wait-ContainerInitialization {
    param([string] $Container)

    for ($attempt = 1; $attempt -le 180; $attempt++) {
        $logs = Invoke-DockerText -Arguments @("logs", $Container)
        if ($logs -match "(?i)init process (complete|done)" -and
            $logs -match "(?i)ready for start up") {
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
        [string] $Container,
        [string] $User,
        [string] $Database
    )

    Wait-ContainerInitialization -Container $Container
    $consecutiveReady = 0
    for ($attempt = 1; $attempt -le 90; $attempt++) {
        if (Test-DockerCommand -Arguments @(
                "exec", $Container, "pg_isready", "-q", "-U", $User, "-d", $Database
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

function Wait-MySqlFamily {
    param(
        [string] $Container,
        [string] $Client,
        [string] $Database
    )

    Wait-ContainerInitialization -Container $Container
    $consecutiveReady = 0
    for ($attempt = 1; $attempt -le 120; $attempt++) {
        if (Test-DockerCommand -Arguments @(
                "exec", $Container, $Client,
                "--defaults-extra-file=/tmp/dbbackup-client.cnf",
                "--batch", "--skip-column-names",
                $Database, "--execute", "SELECT 1"
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

function Initialize-MySqlClientConfig {
    param(
        [string] $Container,
        [string] $PasswordEnvironment
    )

    $shellCommand = (
        "umask 077; echo [client] > /tmp/dbbackup-client.cnf; " +
        "echo user=root >> /tmp/dbbackup-client.cnf; " +
        "printf password= >> /tmp/dbbackup-client.cnf; printenv $PasswordEnvironment " +
        ">> /tmp/dbbackup-client.cnf"
    )
    Invoke-DockerQuiet -Arguments @(
        "exec", $Container, "sh", "-c", $shellCommand
    )
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
        Get-GitLines -Arguments @("ls-files", "--cached", "--others", "--exclude-standard")
    ) |
        Where-Object {
            if ([string]::IsNullOrWhiteSpace($_)) {
                return $false
            }
            return -not $_.Replace("\", "/").StartsWith(
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
    return Get-Sha256Text -Value $manifest.ToString()
}

function Invoke-VersionLine {
    param([string] $Executable)

    $lines = @(& $Executable --version 2>&1)
    if ($LASTEXITCODE -ne 0 -or $lines.Count -eq 0) {
        return "unavailable"
    }
    return $lines[0].ToString().Trim()
}

function Invoke-CargoEvidence {
    param(
        [Parameter(Mandatory = $true)] [string] $Id,
        [Parameter(Mandatory = $true)] [string[]] $Arguments
    )

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        $startedAt = [DateTimeOffset]::Now
        $lines = @(& cargo @Arguments 2>&1)
        $exitCode = [int] $LASTEXITCODE
        $finishedAt = [DateTimeOffset]::Now
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    $output = @($lines | ForEach-Object { $_.ToString() }) -join "`n"
    if ($exitCode -ne 0) {
        $detail = @($lines | Select-Object -Last 20 | ForEach-Object { $_.ToString() }) -join "`n"
        [Console]::Error.WriteLine("G2 contract check $Id failed: $detail")
    }
    return [PSCustomObject] @{
        id = $Id
        command = "cargo " + ($Arguments -join " ")
        startedAt = $startedAt.ToString("o")
        finishedAt = $finishedAt.ToString("o")
        exitCode = $exitCode
        status = if ($exitCode -eq 0) { "pass" } else { "fail" }
        outputSha256 = Get-Sha256Text -Value $output
    }
}

function Get-ToolIdentity {
    param(
        [string] $Container,
        [string] $Tool,
        [string] $Image
    )

    $path = Invoke-DockerText -Arguments @(
        "exec", $Container, "sh", "-c", "command -v $Tool"
    )
    $canonical = Invoke-DockerText -Arguments @(
        "exec", $Container, "readlink", "-f", $path
    )
    if ([IO.Path]::GetFileName($canonical) -eq "pg_wrapper") {
        $bindir = Invoke-DockerText -Arguments @(
            "exec", $Container, "pg_config", "--bindir"
        )
        $canonical = Invoke-DockerText -Arguments @(
            "exec", $Container, "readlink", "-f", "$bindir/$Tool"
        )
    }
    $digestLine = Invoke-DockerText -Arguments @(
        "exec", $Container, "sha256sum", $canonical
    )
    $digest = ($digestLine -split "\s+")[0].Trim().ToLowerInvariant()
    if ($digest -notmatch "^[0-9a-f]{64}$") {
        throw "native tool digest is invalid"
    }
    $versionText = Invoke-DockerText -Arguments @(
        "exec", $Container, $canonical, "--version"
    )
    $version = ($versionText -split "\r?\n")[0].Trim()
    return [PSCustomObject] @{
        name = $Tool
        canonicalPath = $canonical
        version = $version
        sha256 = $digest
        trust = "image_digest_pinned"
        trustAuthority = $Image
    }
}

function Assert-PinnedImage {
    param([string] $Image)

    Invoke-DockerQuiet -Arguments @("pull", $Image)
    $repoDigestsText = Invoke-DockerText -Arguments @(
        "image", "inspect", "--format", "{{json .RepoDigests}}", $Image
    )
    [string[]] $repoDigests = $repoDigestsText | ConvertFrom-Json
    $expectedDigest = ($Image -split "@", 2)[1]
    if (@($repoDigests | Where-Object { $_ -like "*@$expectedDigest" }).Count -eq 0) {
        throw "pulled image identity does not match pinned digest"
    }
    $platform = Invoke-DockerText -Arguments @(
        "image", "inspect", "--format", "{{.Os}}/{{.Architecture}}", $Image
    )
    return [PSCustomObject] @{
        reference = $Image
        digest = $expectedDigest
        repoDigests = $repoDigests
        platform = $platform
    }
}

function Copy-ToContainer {
    param(
        [string] $Source,
        [string] $Container,
        [string] $Destination
    )

    $resolvedSource = (Resolve-Path -LiteralPath $Source).Path
    Invoke-DockerQuiet -Arguments @(
        "cp", $resolvedSource, "${Container}:$Destination"
    )
}

function Copy-FromContainer {
    param(
        [string] $Container,
        [string] $Source,
        [string] $Destination
    )

    Invoke-DockerQuiet -Arguments @(
        "cp", "${Container}:$Source", $Destination
    )
}

function Normalize-Oracle {
    param([AllowEmptyString()] [string] $Value)

    return @(
        $Value -split "\r?\n" |
            ForEach-Object { $_.Trim() } |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
            Sort-Object
    ) -join "`n"
}

function Assert-OracleMarkers {
    param(
        [string] $Oracle,
        [string[]] $Exact,
        [string[]] $Patterns
    )

    $lines = @($Oracle -split "\n")
    foreach ($marker in $Exact) {
        if ($lines -notcontains $marker) {
            throw "content oracle is missing required marker: $marker; available=$Oracle"
        }
    }
    foreach ($pattern in $Patterns) {
        if (@($lines | Where-Object { $_ -match $pattern }).Count -ne 1) {
            throw "content oracle pattern did not match exactly once: $pattern"
        }
    }
}

function New-CapabilityEvidence {
    param([string] $EngineName)

    return @(
        $requiredCapabilities |
            ForEach-Object {
                [PSCustomObject] @{
                    capability = $_
                    outcome = "passed"
                    evidenceRef = "artifacts/g2/$EngineName.json#/$($_)"
                    limitation = $null
                }
            }
    )
}

function Assert-EngineEvidence {
    param(
        [object] $Evidence,
        [object] $Contract,
        [object] $BuildIdentity
    )

    if ($Evidence.schemaVersion -ne 1 -or
        $Evidence.gateId -ne "G2" -or
        $Evidence.engine -ne $Contract.engine -or
        $Evidence.supportStatus -ne "supported" -or
        $Evidence.sourceRevision -ne $BuildIdentity.sourceRevision -or
        $Evidence.sourceTreeDigest -ne $BuildIdentity.sourceTreeDigest -or
        $Evidence.buildId -ne $BuildIdentity.buildId) {
        throw "$($Contract.engine) evidence identity or support status is invalid"
    }
    $capabilities = @($Evidence.capabilities)
    if ($capabilities.Count -ne $requiredCapabilities.Count -or
        (@($capabilities.capability) -join ",") -ne ($requiredCapabilities -join ",") -or
        @($capabilities | Where-Object { $_.outcome -ne "passed" }).Count -ne 0) {
        throw "$($Contract.engine) capability evidence is incomplete"
    }
    if ($Evidence.oracle.matched -ne $true -or
        $Evidence.oracle.sourceSha256 -notmatch "^[0-9a-f]{64}$" -or
        $Evidence.oracle.sourceSha256 -cne $Evidence.oracle.restoredSha256) {
        throw "$($Contract.engine) restore content oracle evidence is invalid"
    }
    $tools = @($Evidence.tools)
    if ($tools.Count -lt 2 -or
        @($tools | Where-Object {
                $_.canonicalPath -notmatch "^/" -or
                $_.sha256 -notmatch "^[0-9a-f]{64}$" -or
                $_.trust -ne "image_digest_pinned" -or
                $_.trustAuthority -ne $Contract.image
            }).Count -ne 0 -or
        @($tools | Where-Object { $_.name -eq $Contract.backupTool }).Count -ne 1 -or
        @($tools | Where-Object { $_.name -eq $Contract.restoreTool }).Count -ne 1) {
        throw "$($Contract.engine) native tool identity evidence is invalid"
    }
    if ($Evidence.image.digest -notmatch "^sha256:[0-9a-f]{64}$" -or
        @($Evidence.image.repoDigests | Where-Object {
                $_ -like "*@$($Evidence.image.digest)"
            }).Count -eq 0 -or
        [string]::IsNullOrWhiteSpace($Evidence.server.version) -or
        [string]::IsNullOrWhiteSpace($Evidence.server.stableId)) {
        throw "$($Contract.engine) server or pinned image evidence is invalid"
    }
    $backupBytes = if (
        $Evidence.backup.PSObject.Properties.Name -contains "databasePartBytes"
    ) {
        [long] $Evidence.backup.databasePartBytes
    }
    else {
        [long] $Evidence.backup.bytes
    }
    if ($backupBytes -le 0) {
        throw "$($Contract.engine) backup evidence is empty"
    }
    $serialized = $Evidence | ConvertTo-Json -Depth 32
    if ($serialized.Contains("dbbackup_g2_ephemeral")) {
        throw "$($Contract.engine) evidence contains fixture credentials"
    }
}

function Remove-Containers {
    param([System.Collections.Generic.List[string]] $Containers)

    foreach ($container in $Containers) {
        & $dockerCommand.Source rm --force $container 1>$null 2>$null
    }
}

function Invoke-PostgresqlRoundTrip {
    param(
        [object] $Contract,
        [object] $BuildIdentity,
        [string] $RunId
    )

    $containers = [System.Collections.Generic.List[string]]::new()
    $sourceContainer = "dbbackup-g2-pg-source-$RunId"
    $targetContainer = "dbbackup-g2-pg-target-$RunId"
    $engineOutput = Join-Path $resolvedOutput "postgresql"
    New-Item -ItemType Directory -Path $engineOutput -Force | Out-Null
    try {
        Write-Host "G2 PostgreSQL: pulling pinned image"
        $imageEvidence = Assert-PinnedImage -Image $Contract.image
        Invoke-DockerQuiet -Arguments @(
            "run", "--detach", "--rm", "--name", $sourceContainer,
            "--env", "POSTGRES_USER=source_admin",
            "--env", "POSTGRES_PASSWORD=dbbackup_g2_ephemeral",
            "--env", "POSTGRES_DB=source_db",
            $Contract.image
        )
        $containers.Add($sourceContainer)
        Wait-Postgres -Container $sourceContainer -User "source_admin" -Database "source_db"

        $fixture = Join-Path $resolvedRoot $Contract.fixture
        Copy-ToContainer -Source $fixture -Container $sourceContainer -Destination "/tmp/fixture.sql"
        Invoke-DockerQuiet -Arguments @(
            "exec", $sourceContainer, "psql", "-v", "ON_ERROR_STOP=1",
            "-U", "source_admin", "-d", "source_db", "-f", "/tmp/fixture.sql"
        )
        $serverVersion = Invoke-DockerText -Arguments @(
            "exec", $sourceContainer, "psql", "-At", "-U", "source_admin",
            "-d", "source_db", "-c", "SHOW server_version"
        )
        $stableId = Invoke-DockerText -Arguments @(
            "exec", $sourceContainer, "psql", "-At", "-U", "source_admin",
            "-d", "source_db", "-c", "SELECT system_identifier FROM pg_control_system()"
        )
        $discovered = Invoke-DockerText -Arguments @(
            "exec", $sourceContainer, "psql", "-At", "-U", "source_admin",
            "-d", "source_db", "-c",
            "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname"
        )
        if (@($discovered -split "\r?\n") -notcontains "source_db") {
            throw "PostgreSQL discovery did not return source_db"
        }

        $tools = @(
            "pg_dump", "pg_dumpall", "pg_restore", "psql" |
                ForEach-Object {
                    Get-ToolIdentity -Container $sourceContainer -Tool $_ -Image $Contract.image
                }
        )
        Write-Host "G2 PostgreSQL: backup and native L2"
        Invoke-DockerQuiet -Arguments @(
            "exec", $sourceContainer, "pg_dumpall", "-U", "source_admin",
            "--globals-only", "--no-role-passwords", "--file=/tmp/globals.sql"
        )
        Invoke-DockerQuiet -Arguments @(
            "exec", $sourceContainer, "pg_dump", "-U", "source_admin",
            "-d", "source_db", "--format=custom", "--file=/tmp/database.dump"
        )
        Invoke-DockerQuiet -Arguments @(
            "exec", $sourceContainer, "pg_restore", "--list",
            "--file=/tmp/toc.list", "/tmp/database.dump"
        )
        if (-not (Test-DockerCommand -Arguments @(
                    "exec", $sourceContainer, "test", "-s", "/tmp/database.dump"
                )) -or
            -not (Test-DockerCommand -Arguments @(
                    "exec", $sourceContainer, "test", "-s", "/tmp/globals.sql"
                )) -or
            -not (Test-DockerCommand -Arguments @(
                    "exec", $sourceContainer, "test", "-s", "/tmp/toc.list"
                ))) {
            throw "PostgreSQL backup or L2 TOC is empty"
        }
        $dumpPath = Join-Path $engineOutput "database.dump"
        $globalsPath = Join-Path $engineOutput "globals.sql"
        $tocPath = Join-Path $engineOutput "toc.list"
        Copy-FromContainer -Container $sourceContainer -Source "/tmp/database.dump" -Destination $dumpPath
        Copy-FromContainer -Container $sourceContainer -Source "/tmp/globals.sql" -Destination $globalsPath
        Copy-FromContainer -Container $sourceContainer -Source "/tmp/toc.list" -Destination $tocPath

        $oracleSql = @"
SELECT 'rows=' || count(*) FROM app.items;
SELECT 'items=' || string_agg(id::text || ':' || label || ':' || encode(payload, 'hex'), ',' ORDER BY id) FROM app.items;
SELECT 'view=' || string_agg(id::text || ':' || label || ':' || payload_hex, ',' ORDER BY id) FROM app.item_summary;
SELECT 'function=' || app.item_label(2);
SELECT 'trigger=' || count(*) FROM pg_trigger WHERE tgname = 'items_normalize_label' AND NOT tgisinternal;
SELECT 'blob=' || encode(digest(lo_get(object_id), 'sha256'), 'hex') FROM app.large_objects WHERE id = 1;
SELECT 'owner=' || tableowner FROM pg_tables WHERE schemaname = 'app' AND tablename = 'items';
SELECT 'extension=' || count(*) FROM pg_extension WHERE extname = 'pgcrypto';
"@
        $sourceOracle = Normalize-Oracle -Value (
            Invoke-DockerText -Arguments @(
                "exec", $sourceContainer, "psql", "-At", "-v", "ON_ERROR_STOP=1",
                "-U", "source_admin", "-d", "source_db", "-c", $oracleSql
            )
        )
        Assert-OracleMarkers -Oracle $sourceOracle -Exact @(
            "rows=2",
            "items=1:${seoulLabel}:000102ff,2:RESTORE-PROOF:deadbeef",
            "view=1:${seoulLabel}:000102ff,2:RESTORE-PROOF:deadbeef",
            "function=RESTORE-PROOF",
            "trigger=1",
            "owner=app_owner",
            "extension=1"
        ) -Patterns @("^blob=[0-9a-f]{64}$")

        Write-Host "G2 PostgreSQL: create-new restore and oracle"
        Invoke-DockerQuiet -Arguments @(
            "run", "--detach", "--rm", "--name", $targetContainer,
            "--env", "POSTGRES_USER=target_admin",
            "--env", "POSTGRES_PASSWORD=dbbackup_g2_ephemeral",
            "--env", "POSTGRES_DB=target_admin",
            $Contract.image
        )
        $containers.Add($targetContainer)
        Wait-Postgres -Container $targetContainer -User "target_admin" -Database "target_admin"
        Copy-ToContainer -Source $globalsPath -Container $targetContainer -Destination "/tmp/globals.sql"
        Copy-ToContainer -Source $dumpPath -Container $targetContainer -Destination "/tmp/database.dump"
        Invoke-DockerQuiet -Arguments @(
            "exec", $targetContainer, "psql", "-v", "ON_ERROR_STOP=1",
            "-U", "target_admin", "-d", "target_admin", "-f", "/tmp/globals.sql"
        )
        Invoke-DockerQuiet -Arguments @(
            "exec", $targetContainer, "createdb", "-U", "target_admin",
            "--owner=app_owner", "restored_db"
        )
        Invoke-DockerQuiet -Arguments @(
            "exec", $targetContainer, "pg_restore", "-U", "target_admin",
            "-d", "restored_db", "--exit-on-error", "--single-transaction",
            "/tmp/database.dump"
        )
        $targetOracle = Normalize-Oracle -Value (
            Invoke-DockerText -Arguments @(
                "exec", $targetContainer, "psql", "-At", "-v", "ON_ERROR_STOP=1",
                "-U", "target_admin", "-d", "restored_db", "-c", $oracleSql
            )
        )
        if ($sourceOracle -cne $targetOracle) {
            throw "PostgreSQL content oracle mismatch after restore"
        }

        return [PSCustomObject] @{
            schemaVersion = 1
            gateId = "G2"
            engine = "postgresql"
            supportStatus = "supported"
            consistencyGrade = "transaction_consistent"
            sourceRevision = $BuildIdentity.sourceRevision
            sourceTreeDigest = $BuildIdentity.sourceTreeDigest
            buildId = $BuildIdentity.buildId
            runnerOs = $BuildIdentity.runnerOs
            runnerArchitecture = $BuildIdentity.runnerArchitecture
            image = $imageEvidence
            server = [PSCustomObject] @{
                version = $serverVersion.Trim()
                stableId = $stableId.Trim()
                sourceDatabase = "source_db"
                restoreDatabase = "restored_db"
            }
            tools = $tools
            backup = [PSCustomObject] @{
                format = "custom"
                databasePartSha256 = Get-FileSha256 -Path $dumpPath
                databasePartBytes = (Get-Item -LiteralPath $dumpPath).Length
                globalsPartSha256 = Get-FileSha256 -Path $globalsPath
                globalsPartBytes = (Get-Item -LiteralPath $globalsPath).Length
                tocSha256 = Get-FileSha256 -Path $tocPath
                globalsRequired = $true
            }
            oracle = [PSCustomObject] @{
                sourceSha256 = Get-Sha256Text -Value $sourceOracle
                restoredSha256 = Get-Sha256Text -Value $targetOracle
                matched = $true
                markerCount = @($targetOracle -split "\n").Count
            }
            capabilities = New-CapabilityEvidence -EngineName "postgresql"
            limitations = @(
                "logical dump only; physical/WAL recovery is outside this certification",
                "restore executes only digest-pinned official image tools in an isolated fixture"
            )
        }
    }
    finally {
        Remove-Containers -Containers $containers
    }
}

function Invoke-MySqlQuery {
    param(
        [string] $Container,
        [string] $Client,
        [string] $Database,
        [string] $Sql
    )

    $arguments = @(
        "exec", $Container, $Client,
        "--defaults-extra-file=/tmp/dbbackup-client.cnf",
        "--batch", "--skip-column-names"
    )
    if (-not [string]::IsNullOrWhiteSpace($Database)) {
        $arguments += $Database
    }
    $arguments += @("--execute", $Sql)
    return Invoke-DockerText -Arguments $arguments
}

function Get-MySqlOracle {
    param(
        [string] $Container,
        [string] $Client,
        [string] $Database
    )

    $sql = @"
SET @fixture_total = 0;
CALL item_count(@fixture_total);
SELECT CONCAT('rows=', COUNT(*)) FROM items;
SELECT CONCAT('audit=', COUNT(*)) FROM audit_log;
SELECT CONCAT('items=', GROUP_CONCAT(CONCAT(id, ':', HEX(label), ':', HEX(payload)) ORDER BY id SEPARATOR ',')) FROM items;
SELECT CONCAT('view=', GROUP_CONCAT(CONCAT(id, ':', HEX(label), ':', payload_hex) ORDER BY id SEPARATOR ',')) FROM item_summary;
SELECT CONCAT('function=', item_label(2));
SELECT CONCAT('procedure=', @fixture_total);
SELECT CONCAT('routines=', GROUP_CONCAT(CONCAT(ROUTINE_TYPE, ':', ROUTINE_NAME) ORDER BY ROUTINE_TYPE, ROUTINE_NAME SEPARATOR ',')) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = DATABASE() AND ROUTINE_NAME IN ('item_count', 'item_label');
SELECT CONCAT('trigger=', COUNT(*)) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = DATABASE() AND TRIGGER_NAME = 'items_after_insert';
SELECT CONCAT('event=', COUNT(*)) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = DATABASE() AND EVENT_NAME = 'daily_fixture_event';
SELECT CONCAT('charset=', collations.CHARACTER_SET_NAME, '/', tables.TABLE_COLLATION)
FROM information_schema.TABLES AS tables
JOIN information_schema.COLLATIONS AS collations
  ON collations.COLLATION_NAME = tables.TABLE_COLLATION
WHERE tables.TABLE_SCHEMA = DATABASE() AND tables.TABLE_NAME = 'items';
"@
    return Normalize-Oracle -Value (
        Invoke-MySqlQuery -Container $Container -Client $Client -Database $Database -Sql $sql
    )
}

function Invoke-MySqlFamilyRoundTrip {
    param(
        [object] $Contract,
        [object] $BuildIdentity,
        [string] $RunId
    )

    $engineName = [string] $Contract.engine
    $isMaria = $engineName -eq "mariadb"
    $client = if ($isMaria) { "mariadb" } else { "mysql" }
    $dumpTool = if ($isMaria) { "mariadb-dump" } else { "mysqldump" }
    $fixturePassword = "dbbackup_g2_ephemeral"
    $sourceContainer = "dbbackup-g2-$engineName-source-$RunId"
    $targetContainer = "dbbackup-g2-$engineName-target-$RunId"
    $containers = [System.Collections.Generic.List[string]]::new()
    $engineOutput = Join-Path $resolvedOutput $engineName
    New-Item -ItemType Directory -Path $engineOutput -Force | Out-Null
    try {
        Write-Host "G2 ${engineName}: pulling pinned image"
        $imageEvidence = Assert-PinnedImage -Image $Contract.image
        $rootPasswordVariable = if ($isMaria) {
            "MARIADB_ROOT_PASSWORD=$fixturePassword"
        }
        else {
            "MYSQL_ROOT_PASSWORD=$fixturePassword"
        }
        $databaseVariable = if ($isMaria) {
            "MARIADB_DATABASE=source_db"
        }
        else {
            "MYSQL_DATABASE=source_db"
        }
        Invoke-DockerQuiet -Arguments @(
            "run", "--detach", "--rm", "--name", $sourceContainer,
            "--env", $rootPasswordVariable,
            "--env", $databaseVariable,
            $Contract.image,
            "--event-scheduler=ON", "--local-infile=0"
        )
        $containers.Add($sourceContainer)
        $passwordEnvironment = if ($isMaria) {
            "MARIADB_ROOT_PASSWORD"
        }
        else {
            "MYSQL_ROOT_PASSWORD"
        }
        Initialize-MySqlClientConfig -Container $sourceContainer `
            -PasswordEnvironment $passwordEnvironment
        Wait-MySqlFamily -Container $sourceContainer -Client $client -Database "source_db"

        $fixture = Join-Path $resolvedRoot $Contract.fixture
        Copy-ToContainer -Source $fixture -Container $sourceContainer -Destination "/tmp/fixture.sql"
        Invoke-MySqlQuery -Container $sourceContainer -Client $client `
            -Database "source_db" -Sql "source /tmp/fixture.sql" | Out-Null

        $serverVersion = Invoke-MySqlQuery -Container $sourceContainer -Client $client `
            -Database "" -Sql "SELECT VERSION()"
        $stableIdSql = if ($isMaria) {
            "SELECT CONCAT(@@hostname, ':', @@server_id)"
        }
        else {
            "SELECT @@server_uuid"
        }
        $stableId = Invoke-MySqlQuery -Container $sourceContainer -Client $client `
            -Database "" -Sql $stableIdSql
        $discovered = Invoke-MySqlQuery -Container $sourceContainer -Client $client `
            -Database "" -Sql "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME"
        if (@($discovered -split "\r?\n") -notcontains "source_db") {
            throw "$engineName discovery did not return source_db"
        }

        $tools = @(
            $dumpTool, $client |
                ForEach-Object {
                    Get-ToolIdentity -Container $sourceContainer -Tool $_ -Image $Contract.image
                }
        )
        if (@($tools | Where-Object { $_.name -eq $Contract.backupTool }).Count -ne 1) {
            throw "$engineName resolved the wrong backup tool"
        }

        $nonTransactionalCase = $null
        if ($isMaria) {
            $nonTxSql = @"
CREATE DATABASE non_tx_probe CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
CREATE TABLE non_tx_probe.legacy_rows(id INT PRIMARY KEY, note VARCHAR(64) NOT NULL) ENGINE=MyISAM;
INSERT INTO non_tx_probe.legacy_rows VALUES (1, 'non-transactional-limit');
SELECT CONCAT(TABLE_SCHEMA, '.', TABLE_NAME, '(', ENGINE, ')')
FROM information_schema.TABLES
WHERE TABLE_SCHEMA = 'non_tx_probe' AND TABLE_NAME = 'legacy_rows';
"@
            $nonTxResult = Normalize-Oracle -Value (
                Invoke-MySqlQuery -Container $sourceContainer -Client $client `
                    -Database "" -Sql $nonTxSql
            )
            if ($nonTxResult -ne "non_tx_probe.legacy_rows(MyISAM)") {
                throw "MariaDB non-transactional limitation probe failed"
            }
            $nonTransactionalCase = [PSCustomObject] @{
                object = $nonTxResult
                consistencyGrade = "crash_consistent"
                supportStatus = "limited"
                limitationCode = "MYSQL_NON_TRANSACTIONAL_TABLE"
            }
        }

        Write-Host "G2 ${engineName}: backup and native restore parser"
        $dumpArguments = @(
            "exec", $sourceContainer, $dumpTool,
            "--defaults-extra-file=/tmp/dbbackup-client.cnf",
            "--single-transaction", "--routines", "--triggers", "--events",
            "--default-character-set=utf8mb4",
            "--result-file=/tmp/database.sql",
            "source_db"
        )
        Invoke-DockerQuiet -Arguments $dumpArguments
        if (-not (Test-DockerCommand -Arguments @(
                    "exec", $sourceContainer, "test", "-s", "/tmp/database.sql"
                ))) {
            throw "$engineName dump is empty"
        }
        $dumpPath = Join-Path $engineOutput "database.sql"
        Copy-FromContainer -Container $sourceContainer -Source "/tmp/database.sql" -Destination $dumpPath
        $sourceOracle = Get-MySqlOracle -Container $sourceContainer -Client $client -Database "source_db"
        $expectedCollation = if ($isMaria) {
            "charset=utf8mb4/utf8mb4_unicode_ci"
        }
        else {
            "charset=utf8mb4/utf8mb4_0900_ai_ci"
        }
        Assert-OracleMarkers -Oracle $sourceOracle -Exact @(
            "rows=2",
            "audit=2",
            "items=1:EC849CEC9AB8:000102FF,2:726573746F72652D70726F6F66:DEADBEEF",
            "view=1:EC849CEC9AB8:000102FF,2:726573746F72652D70726F6F66:DEADBEEF",
            "function=restore-proof",
            "procedure=2",
            "routines=FUNCTION:item_label,PROCEDURE:item_count",
            "trigger=1",
            "event=1",
            $expectedCollation
        ) -Patterns @()

        $targetDatabaseVariable = if ($isMaria) {
            "MARIADB_DATABASE=restored_db"
        }
        else {
            "MYSQL_DATABASE=restored_db"
        }
        Invoke-DockerQuiet -Arguments @(
            "run", "--detach", "--rm", "--name", $targetContainer,
            "--env", $rootPasswordVariable,
            "--env", $targetDatabaseVariable,
            $Contract.image,
            "--event-scheduler=ON", "--local-infile=0"
        )
        $containers.Add($targetContainer)
        Initialize-MySqlClientConfig -Container $targetContainer `
            -PasswordEnvironment $passwordEnvironment
        Wait-MySqlFamily -Container $targetContainer -Client $client -Database "restored_db"
        Copy-ToContainer -Source $dumpPath -Container $targetContainer -Destination "/tmp/database.sql"
        Invoke-MySqlQuery -Container $targetContainer -Client $client `
            -Database "restored_db" -Sql "source /tmp/database.sql" | Out-Null
        $targetOracle = Get-MySqlOracle -Container $targetContainer -Client $client -Database "restored_db"
        if ($sourceOracle -cne $targetOracle) {
            throw "$engineName content oracle mismatch after restore"
        }

        $limitations = if ($isMaria) {
            @(
                "certified support scope requires transactional tables for transaction_consistent grade",
                "MyISAM and other non-transactional tables are reported as limited/crash_consistent"
            )
        }
        else {
            @(
                "certified support scope requires all dumped base tables to use InnoDB",
                "GTID/PITR coordinates are outside this fixture certification"
            )
        }
        return [PSCustomObject] @{
            schemaVersion = 1
            gateId = "G2"
            engine = $engineName
            supportStatus = "supported"
            supportScope = "transactional tables"
            consistencyGrade = "transaction_consistent"
            sourceRevision = $BuildIdentity.sourceRevision
            sourceTreeDigest = $BuildIdentity.sourceTreeDigest
            buildId = $BuildIdentity.buildId
            runnerOs = $BuildIdentity.runnerOs
            runnerArchitecture = $BuildIdentity.runnerArchitecture
            image = $imageEvidence
            server = [PSCustomObject] @{
                version = $serverVersion.Trim()
                stableId = $stableId.Trim()
                sourceDatabase = "source_db"
                restoreDatabase = "restored_db"
            }
            tools = $tools
            backup = [PSCustomObject] @{
                format = "plain"
                sha256 = Get-FileSha256 -Path $dumpPath
                bytes = (Get-Item -LiteralPath $dumpPath).Length
                flags = @(
                    "--single-transaction",
                    "--routines",
                    "--triggers",
                    "--events",
                    "--default-character-set=utf8mb4"
                )
            }
            oracle = [PSCustomObject] @{
                sourceSha256 = Get-Sha256Text -Value $sourceOracle
                restoredSha256 = Get-Sha256Text -Value $targetOracle
                matched = $true
                markerCount = @($targetOracle -split "\n").Count
            }
            nonTransactionalCase = $nonTransactionalCase
            capabilities = New-CapabilityEvidence -EngineName $engineName
            limitations = $limitations
        }
    }
    finally {
        Remove-Containers -Containers $containers
    }
}

Invoke-DockerQuiet -Arguments @("version")
$revisionLines = @(Get-GitLines -Arguments @("rev-parse", "HEAD"))
if ($revisionLines.Count -ne 1) {
    throw "git revision identity is ambiguous"
}
$sourceRevision = $revisionLines[0].Trim()
$sourceTreeDigest = Get-SourceTreeDigest
$runnerOs = [Environment]::OSVersion.VersionString
$rustc = Invoke-VersionLine -Executable "rustc"
$cargo = Invoke-VersionLine -Executable "cargo"
$buildId = Get-Sha256Text -Value (
    "$sourceRevision`n$sourceTreeDigest`n$rustc`n$cargo`n$runnerOs"
)
$buildIdentity = [PSCustomObject] @{
    sourceRevision = $sourceRevision
    sourceTreeDigest = $sourceTreeDigest
    buildId = $buildId
    runnerOs = $runnerOs
    runnerArchitecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
}
$contractChecks = @(
    [PSCustomObject] @{
        id = "provider-contract"
        arguments = @(
            "test", "-p", "dbbackup-core", "--test", "provider_certification"
        )
    },
    [PSCustomObject] @{
        id = "catalog-certification-safety"
        arguments = @(
            "test", "-p", "dbbackup-catalog",
            "--test", "certification_repository",
            "--test", "verification_repository",
            "--test", "ipc_confirmation_repository"
        )
    },
    [PSCustomObject] @{
        id = "restore-verification-safety"
        arguments = @(
            "test", "-p", "dbbackup-verify",
            "--test", "restore_signature_gate",
            "--test", "verify_levels_gated"
        )
    },
    [PSCustomObject] @{
        id = "durable-verification-notification"
        arguments = @(
            "test", "-p", "dbbackup-runtime",
            "--test", "verification_worker",
            "--test", "notification_worker"
        )
    }
)
$contractEvidence = [System.Collections.Generic.List[object]]::new()
$contractFailureCount = 0
foreach ($check in $contractChecks) {
    $evidence = Invoke-CargoEvidence -Id $check.id -Arguments $check.arguments
    $contractEvidence.Add($evidence)
    if ($evidence.exitCode -ne 0) {
        $contractFailureCount++
    }
}
$selectedContracts = if ($Engine -eq "all") {
    $contracts
}
else {
    @($contracts | Where-Object { $_.engine -eq $Engine })
}
$startedAt = [DateTimeOffset]::Now
$results = [System.Collections.Generic.List[object]]::new()
$failureCount = $contractFailureCount
$runId = "{0}-{1}" -f $PID, [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
foreach ($contract in $selectedContracts) {
    try {
        $result = if ($contract.engine -eq "postgresql") {
            Invoke-PostgresqlRoundTrip -Contract $contract -BuildIdentity $buildIdentity -RunId $runId
        }
        else {
            Invoke-MySqlFamilyRoundTrip -Contract $contract -BuildIdentity $buildIdentity -RunId $runId
        }
        Assert-EngineEvidence -Evidence $result -Contract $contract -BuildIdentity $buildIdentity
        $results.Add($result)
        Write-Utf8Json -Path (Join-Path $resolvedOutput "$($contract.engine).json") -Value $result
    }
    catch {
        $failureCount++
        $message = $_.Exception.Message.Replace("dbbackup_g2_ephemeral", "<redacted>")
        [Console]::Error.WriteLine("G2 $($contract.engine) failed: $message")
        $failure = [PSCustomObject] @{
            schemaVersion = 1
            gateId = "G2"
            engine = $contract.engine
            supportStatus = "candidate"
            sourceRevision = $buildIdentity.sourceRevision
            sourceTreeDigest = $buildIdentity.sourceTreeDigest
            buildId = $buildIdentity.buildId
            errorCode = "G2_ENGINE_CERTIFICATION_FAILED"
            error = $message
            capabilities = @()
        }
        $results.Add($failure)
        Write-Utf8Json -Path (Join-Path $resolvedOutput "$($contract.engine).json") -Value $failure
    }
}
$finishedAt = [DateTimeOffset]::Now
$aggregate = [PSCustomObject] @{
    schemaVersion = 1
    gateId = "G2"
    sourceRevision = $buildIdentity.sourceRevision
    sourceTreeDigest = $buildIdentity.sourceTreeDigest
    buildId = $buildIdentity.buildId
    startedAt = $startedAt.ToString("o")
    finishedAt = $finishedAt.ToString("o")
    engineCount = $results.Count
    failureCount = $failureCount
    status = if ($failureCount -eq 0) { "pass" } else { "fail" }
    contractCheckCount = $contractEvidence.Count
    contractChecks = $contractEvidence.ToArray()
    engines = $results.ToArray()
}
Write-Utf8Json -Path (Join-Path $resolvedOutput "existing-engines.json") -Value $aggregate
Write-Output (
    "G2 existing-engine evidence: status=$($aggregate.status), " +
    "engines=$($aggregate.engineCount), failures=$failureCount, buildId=$buildId"
)
if ($failureCount -gt 0) {
    exit 1
}
exit 0
