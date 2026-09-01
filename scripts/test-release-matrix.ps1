[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $OutputRoot,
    [string] $MatrixPath,
    [switch] $ValidateOnly,
    [switch] $RunManaged,
    [switch] $RequireUpstreamEvidence,
    [switch] $RequirePromotionEvidence,
    [string] $QualificationEvidencePath,
    [switch] $RunLiveSqlServer,
    [string] $SqlServerName,
    [string] $SqlPackagePath,
    [string] $SqlPackageVersion,
    [string] $SqlPackageSha256,
    [string] $SqlPackagePublisher = "Microsoft Corporation",
    [string] $SigningMetadataPath,
    [string] $SignedPackageManifestPath,
    [string] $SignedFixtureEvidencePath,
    [string] $ArtifactFunctionalEvidencePath
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding

if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
$root = [IO.Path]::GetFullPath($RepositoryRoot)
if (-not (Test-Path -LiteralPath $root -PathType Container)) {
    throw "G5_REPOSITORY_ROOT_MISSING"
}
$output = if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    Join-Path $root "artifacts\g5"
}
elseif ([IO.Path]::IsPathRooted($OutputRoot)) {
    [IO.Path]::GetFullPath($OutputRoot)
}
else {
    [IO.Path]::GetFullPath((Join-Path $root $OutputRoot))
}
$matrixFile = if ([string]::IsNullOrWhiteSpace($MatrixPath)) {
    Join-Path $root "release\gates\g5-release-matrix.json"
}
elseif ([IO.Path]::IsPathRooted($MatrixPath)) {
    [IO.Path]::GetFullPath($MatrixPath)
}
else {
    [IO.Path]::GetFullPath((Join-Path $root $MatrixPath))
}
New-Item -ItemType Directory -Path $output -Force | Out-Null

$requiredCommandIds = @(
    "g5.update_fault_matrix",
    "g5.minimum_reader_guard",
    "g5.diagnostics_and_drain",
    "g5.support_bundle_canary",
    "g5.support_bundle_operation",
    "g5.telemetry_archive",
    "g5.installer_contract",
    "g5.release_policy"
)
$requiredUpstreamIds = @(
    "g0.current_build",
    "g1.durable_dispatcher",
    "g2.existing_engines",
    "g2.selective_scopes",
    "g3.remote_recovery",
    "g3.postgresql_restore",
    "g4.wave_one"
)
$externalRequirementIds = @(
    "production_authenticode_certificate",
    "rfc3161_timestamp_service",
    "clean_windows_install_fixture",
    "signed_update_fixture",
    "exact_signed_artifact_functional_qualification",
    "owner_approved_stop_metrics"
)

function Write-Utf8Json {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [object] $Value
    )

    [IO.File]::WriteAllText(
        $Path,
        ($Value | ConvertTo-Json -Depth 32),
        [Text.UTF8Encoding]::new($false)
    )
}

function Get-Sha256Text {
    param([AllowEmptyString()] [string] $Value)

    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [Text.Encoding]::UTF8.GetBytes($Value)
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

function Invoke-GitLines {
    param([Parameter(Mandatory = $true)] [string[]] $Arguments)

    $lines = @(& git -C $root @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "G5_SOURCE_IDENTITY_UNAVAILABLE"
    }
    return @($lines | ForEach-Object { $_.ToString() })
}

function Get-SourceTreeDigest {
    $paths = @(
        Invoke-GitLines -Arguments @(
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard"
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
        $absolute = Join-Path $root $relative
        if (Test-Path -LiteralPath $absolute -PathType Leaf) {
            [void] $manifest.Append($relative.Replace("\", "/"))
            [void] $manifest.Append("`t")
            [void] $manifest.Append((Get-FileSha256 -Path $absolute))
            [void] $manifest.Append("`n")
        }
    }
    return Get-Sha256Text -Value $manifest.ToString()
}

function Get-VersionLine {
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

function Read-G5Matrix {
    if (-not (Test-Path -LiteralPath $matrixFile -PathType Leaf)) {
        throw "G5_MATRIX_MISSING"
    }
    $matrix = Get-Content -LiteralPath $matrixFile -Raw -Encoding UTF8 |
        ConvertFrom-Json
    if ($matrix.schemaVersion -ne 1 -or
        $matrix.gateId -ne "G5" -or
        $matrix.allRequired -ne $true) {
        throw "G5_MATRIX_INVALID"
    }
    $commands = @($matrix.commands)
    if (($commands.id -join ",") -ne ($requiredCommandIds -join ",")) {
        throw "G5_MATRIX_COMMAND_SET_INVALID"
    }
    $seen = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::Ordinal
    )
    foreach ($command in $commands) {
        if ([string]::IsNullOrWhiteSpace([string] $command.executable) -or
            $null -eq $command.arguments -or
            -not $seen.Add([string] $command.id)) {
            throw "G5_MATRIX_COMMAND_INVALID"
        }
    }
    if ((@($matrix.externalPromotionRequirements) -join ",") -ne
        ($externalRequirementIds -join ",")) {
        throw "G5_MATRIX_EXTERNAL_REQUIREMENTS_INVALID"
    }
    return $matrix
}

function Invoke-EvidenceCommand {
    param(
        [Parameter(Mandatory = $true)] [string] $Id,
        [Parameter(Mandatory = $true)] [string] $Executable,
        [string[]] $Arguments = @(),
        [Parameter(Mandatory = $true)] [string] $Kind
    )

    $startedAt = [DateTimeOffset]::Now
    $timer = [Diagnostics.Stopwatch]::StartNew()
    $application = Get-Command -Name $Executable -CommandType Application `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1
    $exitCode = 127
    $lines = @()
    if ($null -ne $application) {
        $previousErrorAction = $ErrorActionPreference
        $ErrorActionPreference = "Continue"
        try {
            $script:LASTEXITCODE = 0
            $lines = @(& $application.Source @Arguments 2>&1)
            $exitCode = [int] $LASTEXITCODE
        }
        catch {
            $exitCode = 126
            $lines = @("G5_COMMAND_INVOCATION_FAILED")
        }
        finally {
            $ErrorActionPreference = $previousErrorAction
        }
    }
    $timer.Stop()
    $outputText = @($lines | ForEach-Object { $_.ToString() }) -join "`n"
    $record = [PSCustomObject] @{
        id = $Id
        kind = $Kind
        executable = $Executable
        arguments = @($Arguments)
        startedAt = $startedAt.ToString("o")
        finishedAt = [DateTimeOffset]::Now.ToString("o")
        durationMs = [Math]::Round($timer.Elapsed.TotalMilliseconds, 3)
        exitCode = $exitCode
        status = if ($exitCode -eq 0) { "pass" } else { "fail" }
        outputSha256 = Get-Sha256Text -Value $outputText
    }
    if ($exitCode -ne 0) {
        [Console]::Error.WriteLine("G5 command failed: $Id (exit $exitCode)")
    }
    return $record
}

function New-ManagedCommandPlan {
    $powershell = (Get-Process -Id $PID).Path
    $plan = [Collections.Generic.List[object]]::new()
    $plan.Add([PSCustomObject] @{
        id = "g0.collect"
        executable = $powershell
        arguments = @(
            "-NoProfile",
            "-ExecutionPolicy", "Bypass",
            "-File", "scripts/collect-g0-evidence.ps1",
            "-RepositoryRoot", ".",
            "-OutputRoot", "artifacts/g0"
        )
    })
    $plan.Add([PSCustomObject] @{
        id = "g1.collect"
        executable = $powershell
        arguments = @(
            "-NoProfile", "-File", "scripts/collect-g1-evidence.ps1"
        )
    })
    $plan.Add([PSCustomObject] @{
        id = "g0_g1.truth"
        executable = $powershell
        arguments = @(
            "-NoProfile",
            "-File", "scripts/check-g0-truth.ps1",
            "-EvidencePath", "artifacts/g0/evidence.json",
            "-DurableEvidencePath", "artifacts/g1/evidence.json"
        )
    })
    $plan.Add([PSCustomObject] @{
        id = "g2.existing_engines"
        executable = $powershell
        arguments = @(
            "-NoProfile", "-File", "scripts/test-existing-engines.ps1"
        )
    })
    $plan.Add([PSCustomObject] @{
        id = "g2.selective_scopes"
        executable = $powershell
        arguments = @(
            "-NoProfile", "-File", "scripts/test-selective-scopes.ps1"
        )
    })
    $plan.Add([PSCustomObject] @{
        id = "g3.remote_recovery"
        executable = $powershell
        arguments = @(
            "-NoProfile", "-File", "scripts/test-remote-recovery.ps1"
        )
    })
    $plan.Add([PSCustomObject] @{
        id = "g3.postgresql_restore"
        executable = "cargo"
        arguments = @(
            "test",
            "-p", "dbbackup-runtime",
            "--test", "restore_worker",
            "live_postgresql_create_new_restore_runs_through_durable_restore_work_and_oracle",
            "--",
            "--ignored",
            "--exact",
            "--nocapture"
        )
    })
    $g4Arguments = [Collections.Generic.List[string]]::new()
    foreach ($argument in @(
            "-NoProfile", "-File", "scripts/test-engine-wave1.ps1"
        )) {
        $g4Arguments.Add($argument)
    }
    if ($RunLiveSqlServer) {
        foreach ($required in @(
                $SqlServerName,
                $SqlPackagePath,
                $SqlPackageVersion,
                $SqlPackageSha256
            )) {
            if ([string]::IsNullOrWhiteSpace($required)) {
                throw "G5_SQLSERVER_LIVE_INPUT_MISSING"
            }
        }
        foreach ($argument in @(
                "-RunLiveSqlServer",
                "-SqlServerName", $SqlServerName,
                "-SqlPackagePath", $SqlPackagePath,
                "-SqlPackageVersion", $SqlPackageVersion,
                "-SqlPackageSha256", $SqlPackageSha256,
                "-SqlPackagePublisher", $SqlPackagePublisher
            )) {
            $g4Arguments.Add($argument)
        }
    }
    $plan.Add([PSCustomObject] @{
        id = "g4.wave_one"
        executable = $powershell
        arguments = $g4Arguments.ToArray()
    })
    return $plan.ToArray()
}

function Read-JsonIfPresent {
    param([Parameter(Mandatory = $true)] [string] $Path)

    $metadata = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue
    if ($null -eq $metadata -or
        $metadata.PSIsContainer -or
        $metadata.Length -le 0 -or
        $metadata.Length -gt 16MB -or
        $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        return $null
    }
    try {
        return Get-Content -LiteralPath $Path -Raw -Encoding UTF8 |
            ConvertFrom-Json
    }
    catch {
        return $null
    }
}

function Test-SelectiveScopeEvidence {
    param([Parameter(Mandatory = $true)] [object] $Document)

    if ($Document.engineCount -ne 3 -or
        $Document.failureCount -ne 0 -or
        $Document.status -ne "pass" -or
        $Document.contract.scopeMode -ne "explicit_tables" -or
        $Document.contract.compressionFormat -ne "zstd" -or
        $Document.contract.encryptionFormat -ne "chacha20poly1305_stream" -or
        $Document.contract.signedSetManifestRequired -ne $true -or
        $Document.contract.dekWrapRoundTripRequired -ne $true -or
        $Document.contract.requiredReplicaReaderRoundTrip -ne $true -or
        $Document.contract.maxPlaintextBytes -ne 67108864 -or
        $Document.contract.payloadExcludedFromEvidence -ne $true) {
        return $false
    }
    $engines = @($Document.engines)
    if (($engines.engine -join ",") -ne "postgresql,mysql,mariadb") {
        return $false
    }
    foreach ($engine in $engines) {
        $selectedTables = @($engine.scope.selectedTables)
        if ($engine.status -ne "pass" -or
            $engine.scope.mode -ne "explicit_tables" -or
            $selectedTables.Count -ne 1 -or
            $engine.dump.nativeProcessCount -ne 1 -or
            $engine.restore.cleanDatabase -ne $true -or
            $engine.pipeline.engine -ne $engine.engine -or
            $engine.pipeline.sourceRevision -ne $Document.sourceRevision -or
            $engine.pipeline.sourceTreeDigest -ne $Document.sourceTreeDigest -or
            $engine.pipeline.buildId -ne $Document.buildId -or
            $engine.pipeline.scopeMode -ne "explicit_tables" -or
            $engine.pipeline.scopeIdentity -ne $selectedTables[0] -or
            $engine.pipeline.compressionFormat -ne "zstd" -or
            $engine.pipeline.encryptionFormat -ne "chacha20poly1305_stream" -or
            $engine.pipeline.dekWrapGeneration -ne 1 -or
            $engine.pipeline.setManifestSchemaVersion -ne 2 -or
            $engine.pipeline.signedSetManifestVerified -ne $true -or
            $engine.pipeline.dekWrapRoundTripVerified -ne $true -or
            $engine.pipeline.requiredReplicaReaderEligible -ne $true -or
            $engine.pipeline.payloadReaderRoundTripVerified -ne $true -or
            $engine.pipeline.manifestReaderRoundTripVerified -ne $true -or
            $engine.pipeline.requiredReplicaRequirement -ne "required" -or
            $engine.pipeline.requiredReplicaReaderStatus -ne "eligible" -or
            $engine.pipeline.toolExecutableSha256 -notmatch "^[0-9a-f]{64}$" -or
            $engine.pipeline.plaintextBlake3 -notmatch "^[0-9a-f]{64}$" -or
            $engine.pipeline.ciphertextBlake3 -notmatch "^[0-9a-f]{64}$" -or
            $engine.pipeline.manifestBlake3 -notmatch "^[0-9a-f]{64}$" -or
            $engine.assertions.selectedContentRestored -ne $true -or
            $engine.assertions.excludedTableAbsent -ne $true) {
            return $false
        }
    }
    return $true
}

function Get-UpstreamEvidence {
    param(
        [Parameter(Mandatory = $true)] [string] $Revision,
        [Parameter(Mandatory = $true)] [string] $TreeDigest,
        [Parameter(Mandatory = $true)] [string] $CurrentBuildId,
        [object[]] $ManagedRecords
    )

    $rows = [Collections.Generic.List[object]]::new()
    $definitions = @(
        [PSCustomObject] @{
            id = "g0.current_build"
            path = Join-Path $root "artifacts\g0\evidence.json"
            gateId = "G0"
            identityShape = "root"
        },
        [PSCustomObject] @{
            id = "g1.durable_dispatcher"
            path = Join-Path $root "artifacts\g1\evidence.json"
            gateId = "G1"
            identityShape = "root"
        },
        [PSCustomObject] @{
            id = "g2.existing_engines"
            path = Join-Path $root "artifacts\g2\existing-engines.json"
            gateId = "G2"
            identityShape = "root"
        },
        [PSCustomObject] @{
            id = "g2.selective_scopes"
            path = Join-Path $root "artifacts\g2\selective-scopes.json"
            gateId = "G2-selective-scope"
            identityShape = "root"
        },
        [PSCustomObject] @{
            id = "g4.wave_one"
            path = Join-Path $root "artifacts\g4\engine-wave1.json"
            gateId = "G4"
            identityShape = "nested"
        }
    )
    foreach ($definition in $definitions) {
        $document = Read-JsonIfPresent -Path $definition.path
        $reasonCode = $null
        if ($null -eq $document) {
            $reasonCode = "EVIDENCE_MISSING_OR_INVALID"
        }
        elseif ($document.schemaVersion -ne 1 -or
            $document.gateId -ne $definition.gateId) {
            $reasonCode = "EVIDENCE_SCHEMA_INVALID"
        }
        elseif ($definition.id -eq "g2.selective_scopes" -and
            -not (Test-SelectiveScopeEvidence -Document $document)) {
            $reasonCode = "EVIDENCE_SCHEMA_INVALID"
        }
        else {
            $identity = if ($definition.identityShape -eq "nested") {
                $identityProperty = $document.PSObject.Properties["identity"]
                if ($null -eq $identityProperty) {
                    $null
                }
                else {
                    $identityProperty.Value
                }
            }
            else {
                $document
            }
            $requiredIdentityProperties = @(
                "sourceRevision",
                "sourceTreeDigest"
            )
            $requiresBuildIdentity = $definition.id -in @(
                "g1.durable_dispatcher",
                "g2.existing_engines",
                "g2.selective_scopes"
            )
            if ($requiresBuildIdentity) {
                $requiredIdentityProperties += "buildId"
            }
            $missingIdentityProperties = @(
                $requiredIdentityProperties |
                    Where-Object {
                        $null -eq $identity -or
                        $null -eq $identity.PSObject.Properties[$_]
                    }
            )
            if ($missingIdentityProperties.Count -ne 0) {
                $reasonCode = "EVIDENCE_SCHEMA_INVALID"
            }
            elseif ($identity.sourceRevision -ne $Revision) {
                $reasonCode = "EVIDENCE_REVISION_MISMATCH"
            }
            elseif ($identity.sourceTreeDigest -ne $TreeDigest) {
                $reasonCode = "EVIDENCE_TREE_MISMATCH"
            }
            elseif ($requiresBuildIdentity -and
                $identity.buildId -ne $CurrentBuildId) {
                $reasonCode = "EVIDENCE_BUILD_MISMATCH"
            }
            elseif ($document.failureCount -ne 0 -or
                ($null -ne $document.PSObject.Properties["status"] -and
                    $document.status -ne "pass")) {
                $reasonCode = "EVIDENCE_GATE_FAILED"
            }
        }
        $rows.Add([PSCustomObject] @{
            id = $definition.id
            status = if ($null -eq $reasonCode) { "pass" } else { "incomplete" }
            reasonCode = $reasonCode
            evidencePath = $definition.path.Substring($root.Length).
                TrimStart("\", "/").
                Replace("\", "/")
        })
    }
    foreach ($id in @("g3.remote_recovery", "g3.postgresql_restore")) {
        $record = @($ManagedRecords | Where-Object { $_.id -eq $id })
        $rows.Add([PSCustomObject] @{
            id = $id
            status = if ($record.Count -eq 1 -and $record[0].status -eq "pass") {
                "pass"
            }
            else {
                "incomplete"
            }
            reasonCode = if ($record.Count -eq 1 -and $record[0].status -eq "pass") {
                $null
            }
            elseif (-not $RunManaged) {
                "MANAGED_FIXTURE_NOT_RUN"
            }
            else {
                "MANAGED_FIXTURE_FAILED"
            }
            evidencePath = "artifacts/g5/release-matrix.json#/managedCommands"
        })
    }
    $ordered = foreach ($id in $requiredUpstreamIds) {
        @($rows | Where-Object { $_.id -eq $id })[0]
    }
    return @($ordered)
}

function Test-HasProperties {
    param(
        [object] $Document,
        [Parameter(Mandatory = $true)] [string[]] $Names
    )

    if ($null -eq $Document) {
        return $false
    }
    foreach ($name in $Names) {
        if ($null -eq $Document.PSObject.Properties[$name]) {
            return $false
        }
    }
    return $true
}

function Get-QualificationEvidence {
    param(
        [Parameter(Mandatory = $true)] [string] $Revision,
        [Parameter(Mandatory = $true)] [string] $TreeDigest
    )

    $document = if ([string]::IsNullOrWhiteSpace($QualificationEvidencePath)) {
        $null
    }
    else {
        Read-JsonIfPresent -Path ([IO.Path]::GetFullPath($QualificationEvidencePath))
    }
    $reasonCode = $null
    if ($null -eq $document) {
        $reasonCode = "QUALIFICATION_EVIDENCE_MISSING_OR_INVALID"
    }
    elseif (-not (Test-HasProperties -Document $document -Names @(
                "schemaVersion",
                "gateId",
                "mode",
                "sourceRevision",
                "sourceTreeDigest",
                "buildId",
                "status",
                "execution",
                "upstreamEvidence",
                "managedCommands",
                "commands"
            )) -or
        $document.schemaVersion -ne 1 -or
        $document.gateId -ne "G5" -or
        $document.mode -ne "qualification") {
        $reasonCode = "QUALIFICATION_EVIDENCE_SCHEMA_INVALID"
    }
    elseif ($document.sourceRevision -ne $Revision) {
        $reasonCode = "QUALIFICATION_EVIDENCE_REVISION_MISMATCH"
    }
    elseif ($document.sourceTreeDigest -ne $TreeDigest) {
        $reasonCode = "QUALIFICATION_EVIDENCE_TREE_MISMATCH"
    }
    elseif ($document.buildId -notmatch "^[0-9a-f]{64}$") {
        $reasonCode = "QUALIFICATION_EVIDENCE_BUILD_INVALID"
    }
    elseif (-not (Test-HasProperties -Document $document.execution -Names @(
                "managedRequested",
                "upstreamRequired",
                "g5FailureCount",
                "managedFailureCount"
            )) -or
        $document.execution.managedRequested -ne $true -or
        $document.execution.upstreamRequired -ne $true) {
        $reasonCode = "QUALIFICATION_EVIDENCE_NOT_MANAGED"
    }
    elseif ($document.status -ne "pass" -or
        $document.execution.g5FailureCount -ne 0 -or
        $document.execution.managedFailureCount -ne 0 -or
        (@($document.commands.id) -join ",") -ne ($requiredCommandIds -join ",") -or
        @($document.commands | Where-Object { $_.status -ne "pass" }).Count -ne 0 -or
        (@($document.upstreamEvidence.id) -join ",") -ne
            ($requiredUpstreamIds -join ",") -or
        @($document.upstreamEvidence |
            Where-Object { $_.status -ne "pass" }).Count -ne 0 -or
        @($document.managedCommands |
            Where-Object { $_.status -ne "pass" }).Count -ne 0) {
        $reasonCode = "QUALIFICATION_EVIDENCE_GATE_FAILED"
    }
    return [PSCustomObject] @{
        document = $document
        row = [PSCustomObject] @{
            status = if ($null -eq $reasonCode) { "pass" } else { "fail" }
            reasonCode = $reasonCode
            evidencePath = if ([string]::IsNullOrWhiteSpace(
                    $QualificationEvidencePath
                )) {
                $null
            }
            else {
                [IO.Path]::GetFullPath($QualificationEvidencePath)
            }
        }
    }
}

function Get-IdentityMismatchReason {
    param(
        [object] $Document,
        [Parameter(Mandatory = $true)] [string] $RevisionProperty,
        [Parameter(Mandatory = $true)] [string] $Prefix,
        [Parameter(Mandatory = $true)] [string] $Revision,
        [Parameter(Mandatory = $true)] [string] $TreeDigest,
        [Parameter(Mandatory = $true)] [string] $BuildId
    )

    if (-not (Test-HasProperties -Document $Document -Names @(
                "schemaVersion",
                $RevisionProperty,
                "sourceTreeDigest",
                "buildId"
            )) -or
        $Document.schemaVersion -ne 1) {
        return "${Prefix}_EVIDENCE_MISSING_OR_INVALID"
    }
    if ($Document.$RevisionProperty -ne $Revision) {
        return "${Prefix}_REVISION_MISMATCH"
    }
    if ($Document.sourceTreeDigest -ne $TreeDigest) {
        return "${Prefix}_TREE_MISMATCH"
    }
    if ($Document.buildId -ne $BuildId) {
        return "${Prefix}_BUILD_MISMATCH"
    }
    return $null
}

function Get-ExternalRequirements {
    param(
        [Parameter(Mandatory = $true)] [string] $Revision,
        [Parameter(Mandatory = $true)] [string] $TreeDigest,
        [Parameter(Mandatory = $true)] [string] $BuildId
    )

    $package = if ([string]::IsNullOrWhiteSpace($SignedPackageManifestPath)) {
        $null
    }
    else {
        Read-JsonIfPresent -Path ([IO.Path]::GetFullPath(
                $SignedPackageManifestPath
            ))
    }
    $signing = if ([string]::IsNullOrWhiteSpace($SigningMetadataPath)) {
        $null
    }
    else {
        Read-JsonIfPresent -Path ([IO.Path]::GetFullPath($SigningMetadataPath))
    }
    $fixture = if ([string]::IsNullOrWhiteSpace($SignedFixtureEvidencePath)) {
        $null
    }
    else {
        Read-JsonIfPresent -Path ([IO.Path]::GetFullPath($SignedFixtureEvidencePath))
    }

    $packageReason = Get-IdentityMismatchReason `
        -Document $package `
        -RevisionProperty "revision" `
        -Prefix "SIGNED_PACKAGE" `
        -Revision $Revision `
        -TreeDigest $TreeDigest `
        -BuildId $BuildId
    if ($null -eq $packageReason -and
        (-not (Test-HasProperties -Document $package -Names @(
                    "channel",
                    "version",
                    "minimumReaderVersion",
                    "files"
                )) -or
            $package.channel -ne "production" -or
            $package.version -notmatch "^\d+\.\d+\.\d+$" -or
            @($package.files).Count -eq 0)) {
        $packageReason = "SIGNED_PACKAGE_MANIFEST_INVALID"
    }
    $manifestSha256 = $null
    $serviceBinarySha256 = $null
    $cliBinarySha256 = $null
    $artifactId = $null
    if ($null -eq $packageReason) {
        $manifestFile = [IO.Path]::GetFullPath($SignedPackageManifestPath)
        $packageRoot = [IO.Path]::GetFullPath((Split-Path -Parent $manifestFile))
        try {
            $items = @(Get-ChildItem -LiteralPath $packageRoot -Recurse -Force)
            if (@($items | Where-Object {
                        $_.Attributes -band [IO.FileAttributes]::ReparsePoint
                    }).Count -ne 0) {
                $packageReason = "SIGNED_PACKAGE_REPARSE_POINT_BLOCKED"
            }
            else {
                $actualPaths = @(
                    $items |
                        Where-Object {
                            -not $_.PSIsContainer -and
                            $_.FullName -ne $manifestFile
                        } |
                        ForEach-Object {
                            $_.FullName.Substring($packageRoot.Length).
                                TrimStart("\", "/").
                                Replace("\", "/")
                        } |
                        Sort-Object
                )
                $seenPaths = [Collections.Generic.HashSet[string]]::new(
                    [StringComparer]::OrdinalIgnoreCase
                )
                $expectedPaths = [Collections.Generic.List[string]]::new()
                $actualHashByPath = @{}
                $entryByPath = @{}
                foreach ($entry in @($package.files)) {
                    if (-not (Test-HasProperties -Document $entry -Names @(
                                "path", "sha256", "sizeBytes",
                                "authenticodeRequired", "signerRole",
                                "signerThumbprint", "publisher"
                            ))) {
                        $packageReason = "SIGNED_PACKAGE_MANIFEST_INVALID"
                        break
                    }
                    $relative = [string] $entry.path
                    $normalized = $relative.Replace("\", "/")
                    $segments = @($normalized.Split("/"))
                    if ([string]::IsNullOrWhiteSpace($relative) -or
                        $relative -cne $normalized -or
                        [IO.Path]::IsPathRooted($relative) -or
                        $relative.Contains(":") -or
                        $segments.Count -eq 0 -or
                        @($segments | Where-Object {
                                [string]::IsNullOrWhiteSpace($_) -or
                                $_ -in @(".", "..")
                            }).Count -ne 0 -or
                        -not $seenPaths.Add($normalized) -or
                        $entry.sha256 -notmatch "^[0-9a-f]{64}$" -or
                        [long] $entry.sizeBytes -le 0 -or
                        [long] $entry.sizeBytes -gt 2GB -or
                        $entry.authenticodeRequired -isnot [bool] -or
                        $entry.signerRole -notin @("release", "vendor", "hash_only") -or
                        $entry.signerThumbprint -notmatch "^(?:|[0-9a-f]{40}|[0-9a-f]{64})$" -or
                        [string]::IsNullOrWhiteSpace([string] $entry.publisher)) {
                        $packageReason = "SIGNED_PACKAGE_PATH_INVALID"
                        break
                    }
                    $absolute = [IO.Path]::GetFullPath(
                        (Join-Path $packageRoot $normalized)
                    )
                    $packagePrefix = $packageRoot.TrimEnd("\", "/") +
                        [IO.Path]::DirectorySeparatorChar
                    if (-not $absolute.StartsWith(
                            $packagePrefix,
                            [StringComparison]::OrdinalIgnoreCase
                        ) -or
                        -not (Test-Path -LiteralPath $absolute -PathType Leaf)) {
                        $packageReason = "SIGNED_PACKAGE_FILE_SET_MISMATCH"
                        break
                    }
                    $actualHash = Get-FileSha256 -Path $absolute
                    $metadata = Get-Item -LiteralPath $absolute -Force
                    if ($metadata.Attributes -band [IO.FileAttributes]::ReparsePoint -or
                        $metadata.Length -ne [long] $entry.sizeBytes -or
                        $actualHash -cne [string] $entry.sha256) {
                        $packageReason = "SIGNED_PACKAGE_FILE_HASH_MISMATCH"
                        break
                    }
                    $expectedPaths.Add($normalized)
                    $actualHashByPath[$normalized.ToLowerInvariant()] = $actualHash
                    $entryByPath[$normalized.ToLowerInvariant()] = $entry
                }
                if ($null -eq $packageReason -and
                    (($expectedPaths.ToArray() | Sort-Object) -join "`n") -cne
                        ($actualPaths -join "`n")) {
                    $packageReason = "SIGNED_PACKAGE_FILE_SET_MISMATCH"
                }
                if ($null -eq $packageReason) {
                    foreach ($requiredBinary in @(
                            "dbbackup-service.exe", "dbbackup.exe",
                            "provider-certifications.json",
                            "tools/native-tools-audit.json"
                        )) {
                        if (-not $actualHashByPath.ContainsKey($requiredBinary)) {
                            $packageReason = "SIGNED_PACKAGE_REQUIRED_BINARY_MISSING"
                            break
                        }
                    }
                }
                if ($null -eq $packageReason) {
                    $serviceEntry = $entryByPath["dbbackup-service.exe"]
                    $cliEntry = $entryByPath["dbbackup.exe"]
                    $providerEntry = $entryByPath["provider-certifications.json"]
                    $auditEntry = $entryByPath["tools/native-tools-audit.json"]
                    if ($serviceEntry.authenticodeRequired -ne $true -or
                        $serviceEntry.signerRole -ne "release" -or
                        $cliEntry.authenticodeRequired -ne $true -or
                        $cliEntry.signerRole -ne "release" -or
                        $providerEntry.authenticodeRequired -ne $false -or
                        $providerEntry.signerRole -ne "hash_only" -or
                        $auditEntry.authenticodeRequired -ne $false -or
                        $auditEntry.signerRole -ne "hash_only" -or
                        -not (Test-HasProperties -Document $package.nativeTools -Names @(
                                "auditManifestPath", "auditManifestSha256", "fileCount"
                            )) -or
                        $package.nativeTools.auditManifestPath -cne
                            "tools/native-tools-audit.json" -or
                        $package.nativeTools.auditManifestSha256 -cne
                            $actualHashByPath["tools/native-tools-audit.json"] -or
                        [int] $package.nativeTools.fileCount -le 0) {
                        $packageReason = "SIGNED_PACKAGE_TRUST_BINDING_INVALID"
                    }
                }
                if ($null -eq $packageReason) {
                    $manifestSha256 = Get-FileSha256 -Path $manifestFile
                    $serviceBinarySha256 = $actualHashByPath[
                        "dbbackup-service.exe"
                    ]
                    $cliBinarySha256 = $actualHashByPath["dbbackup.exe"]
                    $artifactId = Get-Sha256Text -Value (
                        "dbbackup-exact-artifact-v1`n" +
                        "$manifestSha256`n" +
                        "$serviceBinarySha256`n" +
                        "$cliBinarySha256`n"
                    )
                }
            }
        }
        catch {
            $packageReason = "SIGNED_PACKAGE_EVIDENCE_MISSING_OR_INVALID"
        }
    }
    $signingReason = Get-IdentityMismatchReason `
        -Document $signing `
        -RevisionProperty "sourceRevision" `
        -Prefix "SIGNED_PACKAGE" `
        -Revision $Revision `
        -TreeDigest $TreeDigest `
        -BuildId $BuildId
    if ($null -eq $signingReason -and
        (-not (Test-HasProperties -Document $signing -Names @(
                    "version",
                    "artifactId",
                    "signerThumbprint",
                    "signerSubject",
                    "nativeToolAuditSha256",
                    "timestampUrl",
                    "packageManifestSha256",
                    "serviceBinarySha256",
                    "cliBinarySha256"
                )) -or
            $signing.signerThumbprint -notmatch "^[0-9A-Fa-f]{40,64}$" -or
            [string]::IsNullOrWhiteSpace([string] $signing.signerSubject) -or
            $signing.nativeToolAuditSha256 -notmatch "^[0-9a-f]{64}$" -or
            $signing.artifactId -notmatch "^[0-9a-f]{64}$" -or
            $signing.packageManifestSha256 -notmatch "^[0-9a-f]{64}$" -or
            $signing.serviceBinarySha256 -notmatch "^[0-9a-f]{64}$" -or
            $signing.cliBinarySha256 -notmatch "^[0-9a-f]{64}$")) {
        $signingReason = "SIGNING_METADATA_INVALID"
    }
    if ($null -eq $signingReason -and $null -eq $packageReason -and
        $signing.version -ne $package.version) {
        $signingReason = "SIGNED_PACKAGE_VERSION_MISMATCH"
    }
    if ($null -eq $signingReason -and $null -eq $packageReason -and
        ($signing.packageManifestSha256 -ne $manifestSha256 -or
            $signing.serviceBinarySha256 -ne $serviceBinarySha256 -or
            $signing.cliBinarySha256 -ne $cliBinarySha256 -or
            $signing.nativeToolAuditSha256 -ne
                $package.nativeTools.auditManifestSha256)) {
        $signingReason = "SIGNED_PACKAGE_ARTIFACT_DIGEST_MISMATCH"
    }
    if ($null -eq $signingReason -and $null -eq $packageReason -and
        $signing.artifactId -cne $artifactId) {
        $signingReason = "SIGNED_PACKAGE_ARTIFACT_ID_MISMATCH"
    }
    $certificateReason = if ($null -ne $packageReason) {
        $packageReason
    }
    else {
        $signingReason
    }
    $timestampReason = if ($null -ne $certificateReason) {
        $certificateReason
    }
    elseif ($signing.timestampUrl -notmatch "^https://[^\s]+$") {
        "TIMESTAMP_EVIDENCE_REQUIRED"
    }
    else {
        $null
    }

    $fixtureReason = Get-IdentityMismatchReason `
        -Document $fixture `
        -RevisionProperty "sourceRevision" `
        -Prefix "SIGNED_FIXTURE" `
        -Revision $Revision `
        -TreeDigest $TreeDigest `
        -BuildId $BuildId
    if ($null -eq $fixtureReason -and
        (-not (Test-HasProperties -Document $fixture -Names @(
                    "status",
                    "trustContext",
                    "signerThumbprint",
                    "deploymentScriptAuthenticode",
                    "cleanInstall",
                    "repair",
                    "update",
                    "supportBundle",
                    "uninstallPreservation",
                    "packageManifestSha256",
                    "serviceBinarySha256",
                    "cliBinarySha256"
                )) -or
            $fixture.status -ne "pass" -or
            $fixture.trustContext -ne
                "same_artifact_lifecycle_fixture_not_production_trust" -or
            $fixture.deploymentScriptAuthenticode -ne "pass")) {
        $fixtureReason = "SIGNED_FIXTURE_EVIDENCE_INVALID"
    }
    if ($null -eq $fixtureReason -and $null -eq $certificateReason -and
        ($fixture.packageManifestSha256 -ne $manifestSha256 -or
            $fixture.serviceBinarySha256 -ne $serviceBinarySha256 -or
            $fixture.cliBinarySha256 -ne $cliBinarySha256 -or
            $fixture.packageManifestSha256 -ne $signing.packageManifestSha256 -or
            $fixture.serviceBinarySha256 -ne $signing.serviceBinarySha256 -or
            $fixture.cliBinarySha256 -ne $signing.cliBinarySha256)) {
        $fixtureReason = "SIGNED_FIXTURE_PACKAGE_DIGEST_MISMATCH"
    }
    if ($null -eq $fixtureReason -and $null -ne $certificateReason) {
        $fixtureReason = "SIGNED_FIXTURE_PACKAGE_BINDING_INVALID"
    }
    elseif ($null -eq $fixtureReason -and
        ([string] $fixture.signerThumbprint).Replace(" ", "").ToLowerInvariant() -ne
            ([string] $signing.signerThumbprint).Replace(" ", "").ToLowerInvariant()) {
        $fixtureReason = "SIGNED_FIXTURE_SIGNER_MISMATCH"
    }
    $cleanFixtureReason = if ($null -ne $fixtureReason) {
        $fixtureReason
    }
    elseif ($fixture.cleanInstall -ne "pass" -or
        $fixture.repair -ne "pass" -or
        $fixture.supportBundle -ne "pass" -or
        $fixture.uninstallPreservation -ne "pass") {
        "SIGNED_FIXTURE_CLEAN_INSTALL_FAILED"
    }
    else {
        $null
    }
    $updateFixtureReason = if ($null -ne $fixtureReason) {
        $fixtureReason
    }
    elseif ($fixture.update -ne "pass") {
        "SIGNED_FIXTURE_UPDATE_FAILED"
    }
    else {
        $null
    }

    $functional = if ([string]::IsNullOrWhiteSpace(
            $ArtifactFunctionalEvidencePath
        )) {
        $null
    }
    else {
        Read-JsonIfPresent -Path ([IO.Path]::GetFullPath(
                $ArtifactFunctionalEvidencePath
            ))
    }
    $functionalReason = if ([string]::IsNullOrWhiteSpace(
            $ArtifactFunctionalEvidencePath
        ) -or $null -eq $functional) {
        "ARTIFACT_FUNCTIONAL_EVIDENCE_MISSING"
    }
    elseif ($null -ne $certificateReason) {
        "ARTIFACT_FUNCTIONAL_PACKAGE_BINDING_INVALID"
    }
    else {
        Get-IdentityMismatchReason `
            -Document $functional `
            -RevisionProperty "sourceRevision" `
            -Prefix "ARTIFACT_FUNCTIONAL" `
            -Revision $Revision `
            -TreeDigest $TreeDigest `
            -BuildId $BuildId
    }
    if ($null -eq $functionalReason -and
        (-not (Test-HasProperties -Document $functional -Names @(
                    "artifactId",
                    "packageManifestSha256",
                    "serviceBinarySha256",
                    "cliBinarySha256"
                )) -or
            $functional.artifactId -cne $artifactId -or
            $functional.artifactId -cne $signing.artifactId -or
            $functional.packageManifestSha256 -cne $manifestSha256 -or
            $functional.serviceBinarySha256 -cne $serviceBinarySha256 -or
            $functional.cliBinarySha256 -cne $cliBinarySha256)) {
        $functionalReason = "ARTIFACT_FUNCTIONAL_PACKAGE_DIGEST_MISMATCH"
    }
    if ($null -eq $functionalReason) {
        $validatorPath = Join-Path $root `
            "scripts\validate-artifact-functional-evidence.ps1"
        if (-not (Test-Path -LiteralPath $validatorPath -PathType Leaf)) {
            $functionalReason = "ARTIFACT_FUNCTIONAL_VALIDATOR_MISSING"
        }
        else {
            $validatorHost = (Get-Process -Id $PID).Path
            $previousErrorAction = $ErrorActionPreference
            $ErrorActionPreference = "Continue"
            try {
                $validatorLines = @(
                    & $validatorHost -NoProfile -File $validatorPath `
                        -EvidencePath $ArtifactFunctionalEvidencePath `
                        -ExpectedPackageManifestSha256 $manifestSha256 `
                        -ExpectedServiceBinarySha256 $serviceBinarySha256 `
                        -ExpectedCliBinarySha256 $cliBinarySha256 2>&1
                )
                $validatorExitCode = [int] $LASTEXITCODE
            }
            finally {
                $ErrorActionPreference = $previousErrorAction
            }
            if ($validatorExitCode -ne 0) {
                $validatorOutput = @(
                    $validatorLines | ForEach-Object { $_.ToString() }
                ) -join "`n"
                $reasonMatch = [regex]::Match(
                    $validatorOutput,
                    "ARTIFACT_FUNCTIONAL_[A-Z_]+"
                )
                $functionalReason = if ($reasonMatch.Success) {
                    $reasonMatch.Value
                }
                else {
                    "ARTIFACT_FUNCTIONAL_EVIDENCE_INVALID"
                }
            }
        }
    }

    $rollout = Read-JsonIfPresent -Path (
        Join-Path $root "release\rollout-policy.json"
    )
    $ownerApproved = $null -ne $rollout -and
        $null -ne $rollout.PSObject.Properties["ownerApprovedStopMetrics"] -and
        $rollout.ownerApprovedStopMetrics.status -eq "approved"
    $failureStatus = if ($RequirePromotionEvidence) { "fail" } else { "not_run" }
    $requirements = @(
        [PSCustomObject] @{
            id = "production_authenticode_certificate"
            status = if ($null -eq $certificateReason) { "pass" } else { $failureStatus }
            reasonCode = $certificateReason
        },
        [PSCustomObject] @{
            id = "rfc3161_timestamp_service"
            status = if ($null -eq $timestampReason) { "pass" } else { $failureStatus }
            reasonCode = $timestampReason
        },
        [PSCustomObject] @{
            id = "clean_windows_install_fixture"
            status = if ($null -eq $cleanFixtureReason) { "pass" } else { $failureStatus }
            reasonCode = $cleanFixtureReason
        },
        [PSCustomObject] @{
            id = "signed_update_fixture"
            status = if ($null -eq $updateFixtureReason) { "pass" } else { $failureStatus }
            reasonCode = $updateFixtureReason
        },
        [PSCustomObject] @{
            id = "exact_signed_artifact_functional_qualification"
            status = if ($null -eq $functionalReason) { "pass" } else { $failureStatus }
            reasonCode = $functionalReason
        },
        [PSCustomObject] @{
            id = "owner_approved_stop_metrics"
            status = if ($ownerApproved) { "pass" } else { "pending_owner_approval" }
            reasonCode = if ($ownerApproved) { $null } else { "OWNER_DECISION_REQUIRED" }
        }
    )
    return [PSCustomObject] @{
        requirements = $requirements
        signedPackage = [PSCustomObject] @{
            status = if ($null -eq $certificateReason) {
                "pass"
            }
            else {
                $failureStatus
            }
            reasonCode = $certificateReason
            manifestSha256 = $manifestSha256
            serviceBinarySha256 = $serviceBinarySha256
            cliBinarySha256 = $cliBinarySha256
            artifactId = $artifactId
        }
        artifactFunctionalQualification = [PSCustomObject] @{
            status = if ($null -eq $functionalReason) {
                "pass"
            }
            else {
                $failureStatus
            }
            reasonCode = $functionalReason
            artifactId = if ($null -eq $functional) {
                $null
            }
            else {
                $functional.artifactId
            }
        }
    }
}

$matrix = Read-G5Matrix
if ($RequireUpstreamEvidence -and
    -not $RunManaged -and
    -not $RequirePromotionEvidence) {
    throw "G5_REQUIRE_UPSTREAM_REQUIRES_MANAGED_RUN"
}
if ($RequirePromotionEvidence -and $RunManaged) {
    throw "G5_PROMOTION_REEVALUATION_CANNOT_RUN_MANAGED"
}
if ($ValidateOnly) {
    [PSCustomObject] @{
        schemaVersion = 1
        gateId = "G5"
        mode = "validate_only"
        commandIds = @($matrix.commands.id)
        upstreamIds = $requiredUpstreamIds
        externalRequirementIds = @($matrix.externalPromotionRequirements)
        managedExecution = $RunManaged.IsPresent
        promotionStatus = "not_evaluated"
    } | ConvertTo-Json -Depth 8
    exit 0
}

$revisionLines = @(Invoke-GitLines -Arguments @("rev-parse", "HEAD"))
if ($revisionLines.Count -ne 1 -or
    $revisionLines[0].Trim() -notmatch "^[0-9a-f]{40}$") {
    throw "G5_SOURCE_REVISION_INVALID"
}
$sourceRevision = $revisionLines[0].Trim()
$sourceTreeDigest = Get-SourceTreeDigest
$runnerOs = [Environment]::OSVersion.VersionString
$rustc = Get-VersionLine -Executable "rustc"
$cargo = Get-VersionLine -Executable "cargo"
$node = Get-VersionLine -Executable "node"
$defaultBuildId = Get-Sha256Text -Value (
    "$sourceRevision`n$sourceTreeDigest`n$rustc`n$cargo`n$runnerOs"
)
$qualification = if ($RequirePromotionEvidence) {
    Get-QualificationEvidence `
        -Revision $sourceRevision `
        -TreeDigest $sourceTreeDigest
}
else {
    [PSCustomObject] @{
        document = $null
        row = [PSCustomObject] @{
            status = "not_evaluated"
            reasonCode = $null
            evidencePath = $null
        }
    }
}
$buildId = if ($RequirePromotionEvidence -and
    $null -ne $qualification.document -and
    $null -ne $qualification.document.PSObject.Properties["buildId"] -and
    $qualification.document.buildId -match "^[0-9a-f]{64}$") {
    [string] $qualification.document.buildId
}
else {
    $defaultBuildId
}

$startedAt = [DateTimeOffset]::Now
$managedRecords = [Collections.Generic.List[object]]::new()
$g5Records = [Collections.Generic.List[object]]::new()
if ($RequirePromotionEvidence) {
    if ($null -ne $qualification.document) {
        if ($null -ne $qualification.document.PSObject.Properties[
                "managedCommands"
            ]) {
            foreach ($record in @($qualification.document.managedCommands)) {
                $managedRecords.Add($record)
            }
        }
        if ($null -ne $qualification.document.PSObject.Properties["commands"]) {
            foreach ($record in @($qualification.document.commands)) {
                $g5Records.Add($record)
            }
        }
    }
}
else {
    Push-Location $root
    try {
        if ($RunManaged) {
            foreach ($command in @(New-ManagedCommandPlan)) {
                $managedRecords.Add(
                    (Invoke-EvidenceCommand `
                        -Id $command.id `
                        -Executable $command.executable `
                        -Arguments @($command.arguments) `
                        -Kind "upstream")
                )
            }
        }
        foreach ($command in @($matrix.commands)) {
            $g5Records.Add(
                (Invoke-EvidenceCommand `
                    -Id ([string] $command.id) `
                    -Executable ([string] $command.executable) `
                    -Arguments @($command.arguments) `
                    -Kind "g5")
            )
        }
    }
    finally {
        Pop-Location
    }
}

$upstream = if ($RequirePromotionEvidence -and
    $null -ne $qualification.document -and
    $null -ne $qualification.document.PSObject.Properties["upstreamEvidence"]) {
    @($qualification.document.upstreamEvidence)
}
else {
    Get-UpstreamEvidence `
        -Revision $sourceRevision `
        -TreeDigest $sourceTreeDigest `
        -CurrentBuildId $buildId `
        -ManagedRecords $managedRecords.ToArray()
}
$externalEvaluation = Get-ExternalRequirements `
    -Revision $sourceRevision `
    -TreeDigest $sourceTreeDigest `
    -BuildId $buildId
$external = @($externalEvaluation.requirements)
$g5FailureCount = @($g5Records | Where-Object { $_.status -ne "pass" }).Count
$managedFailureCount = @(
    $managedRecords | Where-Object { $_.status -ne "pass" }
).Count
$upstreamComplete = @($upstream | Where-Object { $_.status -ne "pass" }).Count -eq 0
$externalComplete = @($external | Where-Object { $_.status -ne "pass" }).Count -eq 0
$promotionEvidenceFailureCount = @(
    $external | Where-Object {
        $_.id -ne "owner_approved_stop_metrics" -and $_.status -ne "pass"
    }
).Count
$qualificationFailed = $RequirePromotionEvidence -and
    $qualification.row.status -ne "pass"
$gateFailed = $g5FailureCount -ne 0 -or
    $qualificationFailed -or
    ($RequireUpstreamEvidence -and
        ($managedFailureCount -ne 0 -or -not $upstreamComplete)) -or
    ($RequirePromotionEvidence -and $promotionEvidenceFailureCount -ne 0)
$evidence = [ordered] @{
    schemaVersion = 1
    gateId = "G5"
    mode = if ($RequirePromotionEvidence) {
        "promotion_reevaluation"
    }
    else {
        "qualification"
    }
    sourceRevision = $sourceRevision
    sourceTreeDigest = $sourceTreeDigest
    buildId = $buildId
    startedAt = $startedAt.ToString("o")
    finishedAt = [DateTimeOffset]::Now.ToString("o")
    status = if ($gateFailed) { "fail" } else { "pass" }
    promotionStatus = if (-not $gateFailed -and
        $upstreamComplete -and
        $externalComplete) {
        "ready"
    }
    else {
        "parked"
    }
    runner = [ordered] @{
        os = $runnerOs
        architecture = [Runtime.InteropServices.RuntimeInformation]::
            OSArchitecture.ToString()
        powershell = $PSVersionTable.PSVersion.ToString()
        rustc = $rustc
        cargo = $cargo
        node = $node
    }
    execution = [ordered] @{
        managedRequested = $RunManaged.IsPresent
        upstreamRequired = $RequireUpstreamEvidence.IsPresent
        promotionEvidenceRequired = $RequirePromotionEvidence.IsPresent
        liveSqlServerRequested = $RunLiveSqlServer.IsPresent
        g5FailureCount = $g5FailureCount
        managedFailureCount = $managedFailureCount
        promotionEvidenceFailureCount = $promotionEvidenceFailureCount
    }
    qualificationEvidence = $qualification.row
    signedPackage = $externalEvaluation.signedPackage
    artifactFunctionalQualification = `
        $externalEvaluation.artifactFunctionalQualification
    upstreamEvidence = $upstream
    managedCommands = $managedRecords.ToArray()
    commands = $g5Records.ToArray()
    externalPromotionRequirements = $external
}
$evidencePath = Join-Path $output "release-matrix.json"
Write-Utf8Json -Path $evidencePath -Value $evidence
Write-Output (
    "G5 release matrix: status=$($evidence.status), " +
    "promotion=$($evidence.promotionStatus), " +
    "g5Failures=$g5FailureCount, managedFailures=$managedFailureCount, " +
    "output=$evidencePath"
)
if ($gateFailed) {
    exit 1
}
exit 0
