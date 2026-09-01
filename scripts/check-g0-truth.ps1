[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $EvidencePath,
    [string] $DurableEvidencePath
)

$ErrorActionPreference = "Stop"
if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
$script:truthErrors = New-Object System.Collections.Generic.List[string]

function Add-TruthError {
    param([Parameter(Mandatory = $true)] [string] $Message)
    $script:truthErrors.Add($Message)
}

function Read-JsonDocument {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $Label
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        Add-TruthError "$Label missing: $Path"
        return $null
    }
    try {
        return Get-Content -LiteralPath $Path -Raw -Encoding UTF8 | ConvertFrom-Json
    }
    catch {
        Add-TruthError "$Label invalid JSON: $($_.Exception.Message)"
        return $null
    }
}

function Test-RequiredProperty {
    param(
        [Parameter(Mandatory = $true)] [object] $Object,
        [Parameter(Mandatory = $true)] [string] $Property,
        [Parameter(Mandatory = $true)] [string] $JsonPath
    )

    $member = $Object.PSObject.Properties[$Property]
    if ($null -eq $member -or
        $null -eq $member.Value -or
        ($member.Value -is [string] -and [string]::IsNullOrWhiteSpace($member.Value))) {
        Add-TruthError "$JsonPath.$Property is required"
        return $false
    }
    return $true
}

function Test-G0Claims {
    param([Parameter(Mandatory = $true)] [string] $Root)

    $claimsPath = Join-Path $Root "release\gates\g0-claims.json"
    $claims = Read-JsonDocument -Path $claimsPath -Label "g0 claims"
    if ($null -eq $claims) {
        return $null
    }

    if ($claims.schemaVersion -ne 1) {
        Add-TruthError "$.schemaVersion must equal 1"
    }
    if ($claims.productVersion -notmatch '^\d+\.\d+\.\d+([-.+][0-9A-Za-z.-]+)?$') {
        Add-TruthError "$.productVersion must be a semantic version"
    }
    if ($claims.executionAuthority -ne "legacy_direct") {
        Add-TruthError "$.executionAuthority must remain legacy_direct before G1"
    }
    if ($claims.cutoverMode -ne "rehearsal_only") {
        Add-TruthError "$.cutoverMode must remain rehearsal_only before G1"
    }

    $allowedClaims = @("supported", "experimental", "configured_only", "unavailable")
    $allowedEvidence = @(
        "automated_pass",
        "managed_pass",
        "not_run",
        "known_gap",
        "not_applicable"
    )
    $requiredIds = @(
        "connection.probe",
        "database.discovery_ui",
        "backup.postgresql",
        "backup.mysql",
        "backup.mariadb",
        "destination.local",
        "destination.s3",
        "destination.ssh",
        "verification.l1",
        "verification.l2",
        "verification.l3",
        "verification.automatic",
        "restore.decrypt_only",
        "restore.database_target",
        "retention.policy",
        "retention.execution",
        "scheduler.legacy_task",
        "dispatcher.durable",
        "concurrency.max_parallel_dumps",
        "dashboard.protection_compliance",
        "notification.durable_delivery",
        "tauri.release_bundle",
        "engine.sqlserver",
        "engine.sqlite"
    )
    $managedRequired = @(
        "backup.postgresql",
        "backup.mysql",
        "backup.mariadb",
        "destination.local",
        "destination.s3",
        "destination.ssh",
        "restore.decrypt_only",
        "restore.database_target",
        "tauri.release_bundle",
        "engine.sqlserver",
        "engine.sqlite"
    )

    $capabilities = @($claims.capabilities)
    if ($capabilities.Count -eq 0) {
        Add-TruthError "$.capabilities must not be empty"
        return $claims
    }

    $seen = @{}
    for ($index = 0; $index -lt $capabilities.Count; $index++) {
        $capability = $capabilities[$index]
        $jsonPath = "$.capabilities[$index]"
        foreach ($property in @(
            "id",
            "claim",
            "automatedEvidence",
            "managedEvidence",
            "publicSummary",
            "ownerSection"
        )) {
            Test-RequiredProperty -Object $capability -Property $property -JsonPath $jsonPath |
                Out-Null
        }
        if ([string]::IsNullOrWhiteSpace($capability.id)) {
            continue
        }
        if ($seen.ContainsKey($capability.id)) {
            Add-TruthError "$jsonPath.id duplicates '$($capability.id)'"
        }
        else {
            $seen[$capability.id] = $true
        }
        if ($allowedClaims -notcontains $capability.claim) {
            Add-TruthError "$jsonPath.claim has unknown value '$($capability.claim)'"
        }
        if ($allowedEvidence -notcontains $capability.automatedEvidence) {
            Add-TruthError (
                "$jsonPath.automatedEvidence has unknown value " +
                "'$($capability.automatedEvidence)'"
            )
        }
        if ($allowedEvidence -notcontains $capability.managedEvidence) {
            Add-TruthError (
                "$jsonPath.managedEvidence has unknown value '$($capability.managedEvidence)'"
            )
        }
        if ($capability.claim -eq "supported" -and
            $capability.automatedEvidence -ne "automated_pass") {
            Add-TruthError (
                "$jsonPath cannot be supported without automated_pass evidence"
            )
        }
        if ($capability.claim -eq "supported" -and
            $managedRequired -contains $capability.id -and
            $capability.managedEvidence -ne "managed_pass") {
            Add-TruthError (
                "$jsonPath cannot be supported without managed_pass evidence"
            )
        }
    }

    foreach ($id in $requiredIds) {
        if (-not $seen.ContainsKey($id)) {
            Add-TruthError "$.capabilities is missing required id '$id'"
        }
    }

    $baselinePath = Join-Path $Root "docs\release\g0-truth-baseline.md"
    if (-not (Test-Path -LiteralPath $baselinePath -PathType Leaf)) {
        Add-TruthError "truth baseline missing: $baselinePath"
    }
    else {
        $baseline = Get-Content -LiteralPath $baselinePath -Raw -Encoding UTF8
        foreach ($capability in $capabilities) {
            $rowPattern = (
                '\|\s*' + [Regex]::Escape([string] $capability.id) +
                '\s*\|\s*' + [Regex]::Escape([string] $capability.claim) +
                '\s*\|\s*' + [Regex]::Escape([string] $capability.automatedEvidence) +
                '\s*\|\s*' + [Regex]::Escape([string] $capability.managedEvidence) +
                '\s*\|'
            )
            if ($baseline -notmatch $rowPattern) {
                Add-TruthError (
                    "truth baseline drift for capability '$($capability.id)'"
                )
            }
        }
    }

    return $claims
}

function Test-G1Requirements {
    param([Parameter(Mandatory = $true)] [string] $Root)

    $path = Join-Path $Root "release\gates\g1-evidence-requirements.json"
    $requirements = Read-JsonDocument -Path $path -Label "g1 requirements"
    if ($null -eq $requirements) {
        return $null
    }
    $expectedIds = @(
        "g1.occurrence_uniqueness",
        "g1.stale_fencing_rejection",
        "g1.process_tree_reaping",
        "g1.checkpoint_crash_reconciliation",
        "g1.spool_reservation",
        "g1.resource_permit_limits",
        "g1.required_destination_truth",
        "g1.single_service_execution_path",
        "g1.operational_store_online_backup"
    )
    if ($requirements.schemaVersion -ne 1 -or $requirements.gateId -ne "G1") {
        Add-TruthError "g1 requirements schemaVersion/gateId is invalid"
    }
    foreach ($flag in @("sameSourceRevision", "sameBuildId", "allRequired")) {
        if ($requirements.$flag -ne $true) {
            Add-TruthError "g1 requirements .$flag must be true"
        }
    }
    $actualIds = @($requirements.requiredEvidenceIds)
    if (@($actualIds | Select-Object -Unique).Count -ne $actualIds.Count) {
        Add-TruthError "g1 requirements contains duplicate evidence IDs"
    }
    foreach ($id in $expectedIds) {
        if ($actualIds -notcontains $id) {
            Add-TruthError "g1 requirements missing '$id'"
        }
    }
    if ($actualIds.Count -ne $expectedIds.Count) {
        Add-TruthError "g1 requirements must contain exactly nine evidence IDs"
    }
    return $requirements
}

function Test-G1Matrix {
    param(
        [Parameter(Mandatory = $true)] [string] $Root,
        [Parameter(Mandatory = $true)] [object] $Requirements
    )

    $path = Join-Path $Root "release\gates\g1-fault-matrix.json"
    $matrix = Read-JsonDocument -Path $path -Label "g1 fault matrix"
    if ($null -eq $matrix) {
        return $null
    }
    if ($matrix.schemaVersion -ne 1 -or $matrix.gateId -ne "G1") {
        Add-TruthError "g1 fault matrix schemaVersion/gateId is invalid"
    }
    $commands = @($matrix.commands)
    $seen = @{}
    foreach ($command in $commands) {
        foreach ($property in @("id", "display", "executable", "arguments", "platform")) {
            Test-RequiredProperty `
                -Object $command `
                -Property $property `
                -JsonPath "$.commands" |
                Out-Null
        }
        if ($seen.ContainsKey([string] $command.id)) {
            Add-TruthError "g1 fault matrix duplicates '$($command.id)'"
        }
        else {
            $seen[[string] $command.id] = $command
        }
        if ($command.platform -ne "windows") {
            Add-TruthError "g1 fault matrix '$($command.id)' must run on windows"
        }
    }
    foreach ($id in @($Requirements.requiredEvidenceIds)) {
        if (-not $seen.ContainsKey([string] $id)) {
            Add-TruthError "g1 fault matrix missing '$id'"
        }
    }
    if ($commands.Count -ne @($Requirements.requiredEvidenceIds).Count) {
        Add-TruthError "g1 fault matrix must contain exactly nine commands"
    }
    return $matrix
}

function Test-RepositoryGuards {
    param([Parameter(Mandatory = $true)] [string] $Root)

    $adrNames = @(
        "ADR-001-execution-authority.md",
        "ADR-002-authoritative-data.md",
        "ADR-003-fenced-delivery.md",
        "ADR-004-recovery-point-and-replicas.md",
        "ADR-005-config-cutover.md",
        "ADR-006-secret-custody.md",
        "ADR-007-local-ipc-authorization.md"
    )
    $adrRoot = Join-Path $Root "docs\plan\connected-backup-platform-v2\adrs"
    foreach ($name in $adrNames) {
        $path = Join-Path $adrRoot $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
            Add-TruthError "approved ADR missing: $name"
            continue
        }
        $text = Get-Content -LiteralPath $path -Raw -Encoding UTF8
        if ($text -notmatch '(?m)^-\s*상태:\s*승인됨\s*$') {
            Add-TruthError "ADR is not approved: $name"
        }
    }

    $inventoryPath = Join-Path $Root "docs\release\g0-direct-execution-inventory.md"
    if (-not (Test-Path -LiteralPath $inventoryPath -PathType Leaf)) {
        Add-TruthError "direct execution inventory missing"
    }
    else {
        $inventory = Get-Content -LiteralPath $inventoryPath -Raw -Encoding UTF8
        $requiredPaths = @(
            "apps/console/src-tauri/src/run.rs::spawn_stream",
            "crates/dbbackup-cli/src/commands/run.rs::run",
            "crates/dbbackup-cli/src/commands/run.rs::execute_job",
            "crates/dbbackup-cli/src/commands/schedule.rs",
            "crates/dbbackup-scheduler/src/xml.rs::render_task_xml",
            "crates/dbbackup-cli/src/commands/verify.rs",
            "crates/dbbackup-cli/src/commands/restore.rs"
        )
        foreach ($requiredPath in $requiredPaths) {
            if (-not $inventory.Contains($requiredPath)) {
                Add-TruthError "direct execution inventory missing '$requiredPath'"
            }
        }
    }

    $publicFiles = New-Object System.Collections.Generic.List[string]
    foreach ($relative in @("README.md", "SECURITY.md")) {
        $candidate = Join-Path $Root $relative
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $publicFiles.Add($candidate)
        }
    }
    $screenRoot = Join-Path $Root "apps\console\src\screens"
    if (Test-Path -LiteralPath $screenRoot -PathType Container) {
        Get-ChildItem -LiteralPath $screenRoot -Filter "*.tsx" -File |
            ForEach-Object { $publicFiles.Add($_.FullName) }
    }
    $bannedPhrases = @(
        "모든 백업이 안전합니다",
        "최근 백업은 서명·검증되어 오프사이트에 보관됩니다",
        "복원 검증 통과"
    )
    foreach ($file in $publicFiles) {
        $text = Get-Content -LiteralPath $file -Raw -Encoding UTF8
        foreach ($phrase in $bannedPhrases) {
            if ($text.Contains($phrase)) {
                $relative = $file.Substring($Root.Length).TrimStart("\", "/")
                Add-TruthError "public copy exceeds G0 claim in ${relative}: '$phrase'"
            }
        }
    }

    $jobsScreen = Join-Path $screenRoot "Jobs.tsx"
    if (Test-Path -LiteralPath $jobsScreen -PathType Leaf) {
        $jobsCopy = Get-Content -LiteralPath $jobsScreen -Raw -Encoding UTF8
        $requiredPolicyCopy = "정책 저장됨, 자동 시행 미배선"
        if (-not $jobsCopy.Contains($requiredPolicyCopy)) {
            Add-TruthError (
                "Jobs screen must disclose configured-only verification/retention: " +
                "'$requiredPolicyCopy'"
            )
        }
    }

    $scheduleScreen = Join-Path $screenRoot "Schedule.tsx"
    if (Test-Path -LiteralPath $scheduleScreen -PathType Leaf) {
        $scheduleCopy = Get-Content -LiteralPath $scheduleScreen -Raw -Encoding UTF8
        $requiredScheduleCopy = "legacy job별 Task Scheduler"
        if (-not $scheduleCopy.Contains($requiredScheduleCopy)) {
            Add-TruthError (
                "Schedule screen must disclose the legacy execution authority: " +
                "'$requiredScheduleCopy'"
            )
        }
    }

    $claimsPath = Join-Path $Root "release\gates\g0-claims.json"
    $currentClaims = if (Test-Path -LiteralPath $claimsPath -PathType Leaf) {
        Get-Content -LiteralPath $claimsPath -Raw -Encoding UTF8 |
            ConvertFrom-Json
    }
    else {
        $null
    }
    $probeClaim = if ($null -eq $currentClaims) {
        @()
    }
    else {
        @(
            $currentClaims.capabilities |
                Where-Object { $_.id -eq "connection.probe" }
        )
    }
    if ($probeClaim.Count -eq 1 -and $probeClaim[0].claim -eq "unavailable") {
        $requiredRegistrationCopy = "live 연결 점검은 실행하지 않았습니다."
        foreach ($screenName in @("Databases.tsx", "Destinations.tsx")) {
            $registrationScreen = Join-Path $screenRoot $screenName
            if (Test-Path -LiteralPath $registrationScreen -PathType Leaf) {
                $registrationCopy =
                    Get-Content -LiteralPath $registrationScreen -Raw -Encoding UTF8
                if (-not $registrationCopy.Contains($requiredRegistrationCopy)) {
                    Add-TruthError (
                        "$screenName must distinguish saved configuration from a live probe: " +
                        "'$requiredRegistrationCopy'"
                    )
                }
            }
        }
    }

    $ciPath = Join-Path $Root ".github\workflows\ci.yml"
    if (-not (Test-Path -LiteralPath $ciPath -PathType Leaf)) {
        Add-TruthError "CI workflow missing: .github/workflows/ci.yml"
    }
    else {
        $ci = Get-Content -LiteralPath $ciPath -Raw -Encoding UTF8
        $requiredCiFragments = @(
            "actions/setup-node@",
            "scripts\collect-g0-evidence.ps1",
            "scripts\check-g0-truth.ps1",
            "-EvidencePath artifacts/g0/evidence.json",
            "if: always()",
            "actions/upload-artifact@",
            "path: artifacts/g0"
        )
        foreach ($fragment in $requiredCiFragments) {
            if (-not $ci.Contains($fragment)) {
                Add-TruthError "CI workflow missing G0 wiring '$fragment'"
            }
        }
    }

    if ($null -ne $currentClaims) {
        foreach ($engine in @("sqlserver", "sqlite")) {
            $relative = "crates\dbbackup-source-$engine"
            $claim = @(
                $currentClaims.capabilities |
                    Where-Object { $_.id -eq "engine.$engine" }
            )
            if (Test-Path -LiteralPath (Join-Path $Root $relative) -PathType Container) {
                if ($claim.Count -ne 1 -or $claim[0].claim -eq "unavailable") {
                    Add-TruthError (
                        "implemented engine '$engine' cannot retain an unavailable claim"
                    )
                }
            }
        }
    }
}

function Get-RepositoryRevision {
    param([Parameter(Mandatory = $true)] [string] $Root)

    $git = Get-Command `
        -Name "git" `
        -CommandType Application `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $git) {
        return $null
    }

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        $lines = @(& $git.Source -C $Root rev-parse HEAD 2>&1)
        $exitCode = $LASTEXITCODE
    }
    catch {
        return $null
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    if ($exitCode -ne 0 -or $lines.Count -eq 0) {
        return $null
    }
    return $lines[0].ToString().Trim()
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

function Get-RepositorySourceTreeDigest {
    param([Parameter(Mandatory = $true)] [string] $Root)

    $git = Get-Command `
        -Name "git" `
        -CommandType Application `
        -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $git) {
        return $null
    }
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $script:LASTEXITCODE = 0
        $paths = @(
            & $git.Source -C $Root ls-files --cached --others --exclude-standard 2>&1
        )
        $exitCode = $LASTEXITCODE
    }
    catch {
        return $null
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
    if ($exitCode -ne 0) {
        return $null
    }
    $manifest = New-Object Text.StringBuilder
    foreach ($rawPath in @($paths | ForEach-Object { $_.ToString() } | Sort-Object -Unique)) {
        if ([string]::IsNullOrWhiteSpace($rawPath)) {
            continue
        }
        $normalized = $rawPath.Replace("\", "/")
        if ($normalized.StartsWith(
                "artifacts/",
                [StringComparison]::OrdinalIgnoreCase
            ) -or
            $normalized.StartsWith(
                "docs/chronos/",
                [StringComparison]::OrdinalIgnoreCase
            )) {
            continue
        }
        $absolute = Join-Path $Root $rawPath
        if (-not (Test-Path -LiteralPath $absolute -PathType Leaf)) {
            continue
        }
        $stream = [IO.File]::OpenRead($absolute)
        $sha = [Security.Cryptography.SHA256]::Create()
        try {
            $fileHash =
                ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace("-", "").ToLowerInvariant()
        }
        finally {
            $sha.Dispose()
            $stream.Dispose()
        }
        [void] $manifest.Append($rawPath.Replace("\", "/"))
        [void] $manifest.Append("`t")
        [void] $manifest.Append($fileHash)
        [void] $manifest.Append("`n")
    }
    return Get-Sha256Text -Value $manifest.ToString()
}

function Test-G0Evidence {
    param(
        [Parameter(Mandatory = $true)] [string] $Root,
        [string] $Path
    )

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return $false
    }
    $resolvedPath = if ([IO.Path]::IsPathRooted($Path)) {
        [IO.Path]::GetFullPath($Path)
    }
    else {
        [IO.Path]::GetFullPath((Join-Path $Root $Path))
    }
    $before = $script:truthErrors.Count
    $evidence = Read-JsonDocument -Path $resolvedPath -Label "g0 evidence"
    if ($null -eq $evidence) {
        return $false
    }

    if ($evidence.schemaVersion -ne 1 -or $evidence.gateId -ne "G0") {
        Add-TruthError "g0 evidence schemaVersion/gateId is invalid"
    }
    foreach ($property in @(
        "sourceRevision",
        "sourceTreeDigest",
        "startedAt",
        "finishedAt",
        "runner",
        "commands",
        "failureCount",
        "status",
        "ignoredTests"
    )) {
        Test-RequiredProperty `
            -Object $evidence `
            -Property $property `
            -JsonPath "$" |
            Out-Null
    }
    if ($evidence.failureCount -ne 0 -or $evidence.status -ne "pass") {
        Add-TruthError "g0 evidence must have status=pass and failureCount=0"
    }

    $currentRevision = Get-RepositoryRevision -Root $Root
    if ($null -ne $currentRevision -and
        $evidence.sourceRevision -ne $currentRevision) {
        Add-TruthError "g0 evidence source revision does not match repository HEAD"
    }
    $currentTreeDigest = Get-RepositorySourceTreeDigest -Root $Root
    if ($null -ne $currentTreeDigest -and
        $evidence.sourceTreeDigest -ne $currentTreeDigest) {
        Add-TruthError "g0 evidence source tree digest is stale"
    }

    foreach ($property in @(
        "os",
        "architecture",
        "powershell",
        "rustc",
        "cargo",
        "node",
        "npm"
    )) {
        Test-RequiredProperty `
            -Object $evidence.runner `
            -Property $property `
            -JsonPath "$.runner" |
            Out-Null
    }

    $requiredCommands = @(
        "cargo-fmt",
        "cargo-clippy",
        "cargo-test",
        "cargo-ignored-list",
        "npm-ci",
        "npm-build"
    )
    $commands = @($evidence.commands)
    $seenCommands = @{}
    for ($index = 0; $index -lt $commands.Count; $index++) {
        $command = $commands[$index]
        $jsonPath = "$.commands[$index]"
        foreach ($property in @(
            "id",
            "command",
            "kind",
            "startedAt",
            "finishedAt",
            "durationMs",
            "exitCode",
            "status",
            "sourceRevision",
            "sourceTreeDigest",
            "runnerOs"
        )) {
            Test-RequiredProperty `
                -Object $command `
                -Property $property `
                -JsonPath $jsonPath |
                Out-Null
        }
        if ($seenCommands.ContainsKey([string] $command.id)) {
            Add-TruthError "$jsonPath.id duplicates '$($command.id)'"
        }
        else {
            $seenCommands[[string] $command.id] = $true
        }
        if ($command.exitCode -ne 0 -or $command.status -ne "pass") {
            Add-TruthError "g0 evidence command '$($command.id)' did not pass"
        }
        if ($command.sourceRevision -ne $evidence.sourceRevision) {
            Add-TruthError "g0 evidence command '$($command.id)' has stale source revision"
        }
        if ($command.sourceTreeDigest -ne $evidence.sourceTreeDigest) {
            Add-TruthError "g0 evidence command '$($command.id)' has stale source tree digest"
        }
        if ($command.runnerOs -ne $evidence.runner.os) {
            Add-TruthError "g0 evidence command '$($command.id)' has mismatched runner OS"
        }
        if ($command.durationMs -lt 0) {
            Add-TruthError "g0 evidence command '$($command.id)' has negative duration"
        }
        $expectedKind = if ($command.id -eq "cargo-ignored-list") {
            "ignored_list"
        }
        else {
            "gate"
        }
        if ($command.kind -ne $expectedKind) {
            Add-TruthError "g0 evidence command '$($command.id)' has invalid kind"
        }
        foreach ($forbiddenProperty in @(
            "output",
            "stdout",
            "stderr",
            "environment",
            "environmentVariables"
        )) {
            if ($null -ne $command.PSObject.Properties[$forbiddenProperty]) {
                Add-TruthError (
                    "g0 evidence command '$($command.id)' stores forbidden " +
                    "property '$forbiddenProperty'"
                )
            }
        }
    }
    foreach ($id in $requiredCommands) {
        if (-not $seenCommands.ContainsKey($id)) {
            Add-TruthError "g0 evidence is missing required command '$id'"
        }
    }
    if ($commands.Count -ne $requiredCommands.Count) {
        Add-TruthError "g0 evidence must contain exactly six commands"
    }

    $seenIgnored = @{}
    foreach ($ignored in @($evidence.ignoredTests)) {
        if ([string]::IsNullOrWhiteSpace([string] $ignored.name)) {
            Add-TruthError "g0 evidence ignored test requires a name"
            continue
        }
        if ($seenIgnored.ContainsKey([string] $ignored.name)) {
            Add-TruthError "g0 evidence duplicates ignored test '$($ignored.name)'"
        }
        else {
            $seenIgnored[[string] $ignored.name] = $true
        }
        if ($ignored.status -ne "not_run") {
            Add-TruthError (
                "g0 evidence ignored test '$($ignored.name)' must remain not_run"
            )
        }
    }

    return $script:truthErrors.Count -eq $before
}

function Test-DurableEvidence {
    param(
        [Parameter(Mandatory = $true)] [string] $Root,
        [Parameter(Mandatory = $true)] [object] $Requirements,
        [Parameter(Mandatory = $true)] [object] $Matrix,
        [string] $EvidencePath
    )

    if ([string]::IsNullOrWhiteSpace($EvidencePath)) {
        return $false
    }
    $resolvedPath = if ([IO.Path]::IsPathRooted($EvidencePath)) {
        $EvidencePath
    }
    else {
        Join-Path $Root $EvidencePath
    }
    $before = $script:truthErrors.Count
    $result = Read-JsonDocument -Path $resolvedPath -Label "durable evidence"
    if ($null -eq $result) {
        return $false
    }
    if ($result.schemaVersion -ne 1 -or $result.gateId -ne "G1") {
        Add-TruthError "durable evidence schemaVersion/gateId is invalid"
    }
    if ($result.status -ne "pass" -or $result.failureCount -ne 0) {
        Add-TruthError "durable evidence must have status=pass and failureCount=0"
    }
    foreach ($property in @(
        "sourceRevision",
        "sourceTreeDigest",
        "buildId",
        "startedAt",
        "finishedAt",
        "runner",
        "evidence"
    )) {
        Test-RequiredProperty `
            -Object $result `
            -Property $property `
            -JsonPath "$" |
            Out-Null
    }
    if ($null -ne $result.runner) {
        Test-RequiredProperty `
            -Object $result.runner `
            -Property "os" `
            -JsonPath "$.runner" |
            Out-Null
    }
    $currentRevision = Get-RepositoryRevision -Root $Root
    if ($null -ne $currentRevision -and $result.sourceRevision -ne $currentRevision) {
        Add-TruthError "durable evidence source revision does not match repository HEAD"
    }
    $currentTreeDigest = Get-RepositorySourceTreeDigest -Root $Root
    if ($null -ne $currentTreeDigest -and
        $result.sourceTreeDigest -ne $currentTreeDigest) {
        Add-TruthError "durable evidence source tree digest is stale"
    }
    $expectedCommands = @{}
    foreach ($command in @($Matrix.commands)) {
        $expectedCommands[[string] $command.id] = [string] $command.display
    }
    $rows = @($result.evidence)
    $seen = @{}
    for ($index = 0; $index -lt $rows.Count; $index++) {
        $row = $rows[$index]
        $jsonPath = "$.evidence[$index]"
        foreach ($property in @(
            "id",
            "command",
            "status",
            "exitCode",
            "startedAt",
            "finishedAt",
            "durationMs",
            "sourceRevision",
            "sourceTreeDigest",
            "buildId",
            "runnerOs"
        )) {
            Test-RequiredProperty `
                -Object $row `
                -Property $property `
                -JsonPath $jsonPath |
                Out-Null
        }
        if ($seen.ContainsKey([string] $row.id)) {
            Add-TruthError "durable evidence duplicates '$($row.id)'"
            continue
        }
        $seen[[string] $row.id] = $row
        if (-not $expectedCommands.ContainsKey([string] $row.id) -or
            $row.command -ne $expectedCommands[[string] $row.id]) {
            Add-TruthError "durable evidence '$($row.id)' command does not match fault matrix"
        }
        if ($row.exitCode -ne 0 -or $row.status -ne "pass") {
            Add-TruthError "durable evidence '$($row.id)' did not pass"
        }
        if ($row.durationMs -lt 0) {
            Add-TruthError "durable evidence '$($row.id)' has negative duration"
        }
        if ($row.sourceRevision -ne $result.sourceRevision) {
            Add-TruthError "durable evidence '$($row.id)' has stale source revision"
        }
        if ($row.sourceTreeDigest -ne $result.sourceTreeDigest) {
            Add-TruthError "durable evidence '$($row.id)' has stale source tree digest"
        }
        if ($row.buildId -ne $result.buildId) {
            Add-TruthError "durable evidence '$($row.id)' has mismatched build ID"
        }
        if ($row.runnerOs -ne $result.runner.os) {
            Add-TruthError "durable evidence '$($row.id)' has mismatched runner OS"
        }
        foreach ($forbiddenProperty in @(
            "output",
            "stdout",
            "stderr",
            "environment",
            "environmentVariables"
        )) {
            if ($null -ne $row.PSObject.Properties[$forbiddenProperty]) {
                Add-TruthError (
                    "durable evidence '$($row.id)' stores forbidden " +
                    "property '$forbiddenProperty'"
                )
            }
        }
    }
    foreach ($id in @($Requirements.requiredEvidenceIds)) {
        if (-not $seen.ContainsKey($id)) {
            Add-TruthError "durable evidence missing '$id'"
            continue
        }
        $row = $seen[$id]
    }
    if ($rows.Count -ne @($Requirements.requiredEvidenceIds).Count) {
        Add-TruthError "durable evidence must contain exactly nine rows"
    }
    return $script:truthErrors.Count -eq $before
}

$resolvedRoot = [IO.Path]::GetFullPath($RepositoryRoot)
if (-not (Test-Path -LiteralPath $resolvedRoot -PathType Container)) {
    [Console]::Error.WriteLine("repository root does not exist: $resolvedRoot")
    exit 1
}

$claims = Test-G0Claims -Root $resolvedRoot
$requirements = Test-G1Requirements -Root $resolvedRoot
$matrix = if ($null -ne $requirements) {
    Test-G1Matrix -Root $resolvedRoot -Requirements $requirements
}
else {
    $null
}
Test-RepositoryGuards -Root $resolvedRoot
$evidenceGate = Test-G0Evidence -Root $resolvedRoot -Path $EvidencePath
$durableGate = $false
if ($null -ne $requirements -and $null -ne $matrix) {
    $durableGate = Test-DurableEvidence `
        -Root $resolvedRoot `
        -Requirements $requirements `
        -Matrix $matrix `
        -EvidencePath $DurableEvidencePath
}

if ($script:truthErrors.Count -gt 0) {
    [Console]::Error.WriteLine("G0 truth check failed: $($script:truthErrors.Count) error(s)")
    foreach ($errorMessage in $script:truthErrors) {
        [Console]::Error.WriteLine(" - $errorMessage")
    }
    exit 1
}

Write-Output "G0 truth check passed"
Write-Output "executionAuthority=$($claims.executionAuthority)"
Write-Output "cutoverMode=$($claims.cutoverMode)"
Write-Output "evidenceGate=$($evidenceGate.ToString().ToLowerInvariant())"
Write-Output "durableGate=$($durableGate.ToString().ToLowerInvariant())"
