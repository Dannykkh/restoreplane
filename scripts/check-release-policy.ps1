[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $OutputRoot
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

if ([string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $RepositoryRoot = Split-Path -Parent $PSScriptRoot
}
$root = [IO.Path]::GetFullPath($RepositoryRoot)
$output = if ([string]::IsNullOrWhiteSpace($OutputRoot)) {
    Join-Path $root "artifacts\g5"
}
elseif ([IO.Path]::IsPathRooted($OutputRoot)) {
    [IO.Path]::GetFullPath($OutputRoot)
}
else {
    [IO.Path]::GetFullPath((Join-Path $root $OutputRoot))
}
New-Item -ItemType Directory -Path $output -Force | Out-Null

function Read-Json {
    param([Parameter(Mandatory = $true)] [string] $RelativePath)

    $path = Join-Path $root $RelativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "RELEASE_POLICY_FILE_MISSING: $RelativePath"
    }
    return Get-Content -LiteralPath $path -Raw -Encoding UTF8 | ConvertFrom-Json
}

$claims = Read-Json "release\gates\g0-claims.json"
$rollout = Read-Json "release\rollout-policy.json"
$expansion = Read-Json "release\expansion-boundaries.json"
$packageSchema = Read-Json "release\package-manifest.schema.json"
$signingMetadataSchema = Read-Json "release\signing-metadata.schema.json"
$releaseFixtureSchema = Read-Json "release\release-fixture-evidence.schema.json"
$artifactFunctionalSchema = Read-Json "release\artifact-functional-evidence.schema.json"
$providerCertificationSchema = Read-Json "release\provider-certifications.schema.json"
$nativeToolAuditSchema = Read-Json "release\native-tool-audit.schema.json"
$providerSchema = Read-Json "release\provider-promotion.schema.json"
$g5 = Read-Json "release\gates\g5-release-matrix.json"

if ($claims.schemaVersion -ne 1 -or
    $rollout.schemaVersion -ne 1 -or
    $expansion.schemaVersion -ne 1 -or
    $g5.gateId -ne "G5" -or
    $packageSchema.title -ne "DbBackup signed package manifest" -or
    $signingMetadataSchema.title -ne "DbBackup production signing metadata" -or
    $releaseFixtureSchema.title -ne
        "DbBackup pristine signed lifecycle fixture evidence" -or
    $artifactFunctionalSchema.title -ne
        "DbBackup exact signed artifact functional qualification evidence" -or
    $providerCertificationSchema.title -ne
        "DbBackup managed provider certifications" -or
    $nativeToolAuditSchema.title -ne "DbBackup audited native tool bundle" -or
    $providerSchema.title -ne "DbBackup provider promotion evidence") {
    throw "RELEASE_POLICY_SCHEMA_INVALID"
}
if ("sourceTreeDigest" -notin @($packageSchema.required) -or
    $packageSchema.properties.sourceTreeDigest.pattern -ne "^[0-9a-f]{64}$" -or
    @(@(
            "sourceRevision",
            "sourceTreeDigest",
            "buildId",
            "artifactId",
            "packageManifestSha256",
            "serviceBinarySha256",
            "cliBinarySha256"
        ) |
        Where-Object { $_ -notin @($signingMetadataSchema.required) }).Count -ne 0 -or
    @(@(
            "sourceRevision",
            "sourceTreeDigest",
            "buildId",
            "packageManifestSha256",
            "serviceBinarySha256",
            "cliBinarySha256"
        ) |
        Where-Object { $_ -notin @($releaseFixtureSchema.required) }).Count -ne 0) {
    throw "RELEASE_BUILD_IDENTITY_SCHEMA_INVALID"
}
if (@(@(
            "sourceRevision",
            "sourceTreeDigest",
            "buildId",
            "artifactId",
            "packageManifestSha256",
            "serviceBinarySha256",
            "cliBinarySha256",
            "installedServiceBinarySha256",
            "installedCliBinarySha256",
            "engines"
        ) |
        Where-Object { $_ -notin @($artifactFunctionalSchema.required) }).Count -ne 0) {
    throw "RELEASE_ARTIFACT_FUNCTIONAL_SCHEMA_INVALID"
}
if ("sizeBytes" -notin @($packageSchema.properties.files.items.required) -or
    $packageSchema.properties.files.items.properties.sizeBytes.minimum -ne 1 -or
    $packageSchema.allOf[0].then.properties.files.contains.properties.path.const -ne
        "provider-certifications.json") {
    throw "RELEASE_PACKAGE_MANIFEST_BINDING_SCHEMA_INVALID"
}

$claimMap = @{}
foreach ($claim in @($claims.capabilities)) {
    if ($claimMap.ContainsKey([string] $claim.id)) {
        throw "RELEASE_CLAIM_DUPLICATE"
    }
    $claimMap[[string] $claim.id] = $claim
}
foreach ($required in @(
        "engine.sqlite",
        "engine.sqlserver",
        "service.diagnostics",
        "support.bundle",
        "telemetry.archive",
        "windows.install_update",
        "tauri.release_bundle"
    )) {
    if (-not $claimMap.ContainsKey($required)) {
        throw "RELEASE_REQUIRED_CLAIM_MISSING: $required"
    }
}
if ($claimMap["engine.sqlite"].claim -ne "supported" -or
    $claimMap["engine.sqlserver"].claim -ne "experimental" -or
    $claimMap["service.diagnostics"].claim -ne "supported" -or
    $claimMap["support.bundle"].claim -ne "supported" -or
    $claimMap["telemetry.archive"].claim -ne "supported" -or
    $claimMap["windows.install_update"].claim -ne "experimental" -or
    $claimMap["tauri.release_bundle"].claim -ne "experimental") {
    throw "RELEASE_CLAIM_EXCEEDS_EVIDENCE"
}

$requiredSafetyInvariants = @(
    "duplicateRecoveryPointsPerOccurrenceTarget",
    "partialScopesReportedAsWholeDatabase",
    "recoveryPointsWithoutSignedManifestOrRequiredReplicaEvidence",
    "secretCanaryMatches",
    "setManifestPayloadDeletionViolations",
    "staleFenceCommits",
    "unexplainedMissedOccurrences",
    "unsupportedClaimPromotions"
)
$actualSafetyInvariants = @(
    $rollout.safetyInvariants.PSObject.Properties.Name | Sort-Object
)
if (($actualSafetyInvariants -join ",") -ne
    (($requiredSafetyInvariants | Sort-Object) -join ",")) {
    $missing = @(
        $requiredSafetyInvariants |
            Where-Object { $_ -notin $actualSafetyInvariants }
    )
    if ($missing.Count -ne 0) {
        throw "RELEASE_SAFETY_INVARIANT_MISSING: $($missing -join ',')"
    }
    throw "RELEASE_SAFETY_INVARIANT_SET_DRIFT"
}
foreach ($value in $rollout.safetyInvariants.PSObject.Properties.Value) {
    if ([int64] $value -ne 0) {
        throw "RELEASE_SAFETY_INVARIANT_MUST_BE_ZERO"
    }
}
$expectedStages = @(
    "internal_dogfood",
    "controlled_beta",
    "opt_in_v2_cutover",
    "wave_1",
    "general_availability"
)
if ((@($rollout.stages.id) -join ",") -ne ($expectedStages -join ",") -or
    $rollout.ownerApprovedStopMetrics.status -ne "pending_owner_approval") {
    throw "RELEASE_ROLLOUT_POLICY_INVALID"
}
$expectedDeferred = @(
    "public_http_api",
    "organization_rbac",
    "remote_agent",
    "backup_content_browser"
)
if ((@($rollout.deferred) -join ",") -ne ($expectedDeferred -join ",")) {
    throw "RELEASE_DEFERRED_BOUNDARY_DRIFT"
}
foreach ($provider in @($expansion.providers)) {
    if ($provider.status -notmatch "^(candidate|experimental).*(not_implemented)$") {
        throw "RELEASE_UNPROVEN_PROVIDER_PROMOTED"
    }
    if ($claimMap.ContainsKey("engine.$($provider.id)") -and
        $claimMap["engine.$($provider.id)"].claim -eq "supported") {
        throw "RELEASE_UNPROVEN_PROVIDER_CLAIM"
    }
}

foreach ($script in @(
        "scripts\install.ps1",
        "scripts\repair.ps1",
        "scripts\update.ps1",
        "scripts\uninstall.ps1",
        "scripts\stage-native-tools.ps1",
        "scripts\test-release-matrix.ps1",
        "scripts\new-provider-certifications.ps1",
        "scripts\test-signed-artifact-functional.ps1",
        "scripts\validate-artifact-functional-evidence.ps1"
    )) {
    $path = Join-Path $root $script
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        if ($script -eq "scripts\test-release-matrix.ps1") {
            continue
        }
        throw "RELEASE_SCRIPT_MISSING: $script"
    }
    $tokens = $null
    $errors = $null
    [Management.Automation.Language.Parser]::ParseFile(
        $path,
        [ref] $tokens,
        [ref] $errors
    ) | Out-Null
    if (@($errors).Count -ne 0) {
        throw "RELEASE_SCRIPT_PARSE_FAILED: $script"
    }
}

$deploymentScripts = @(
    "scripts\install.ps1",
    "scripts\repair.ps1",
    "scripts\update.ps1",
    "scripts\uninstall.ps1"
)
foreach ($script in $deploymentScripts) {
    $text = Get-Content -LiteralPath (Join-Path $root $script) -Raw -Encoding UTF8
    $verificationIndex = $text.LastIndexOf(
        "Assert-DbBackupDeploymentAuthenticode",
        [StringComparison]::Ordinal
    )
    $importIndex = $text.IndexOf("Import-Module", [StringComparison]::Ordinal)
    if ($verificationIndex -lt 0 -or
        $importIndex -lt $verificationIndex -or
        -not $text.Contains("Get-AuthenticodeSignature") -or
        -not $text.Contains("DEPLOYMENT_SCRIPT_AUTHENTICODE_INVALID")) {
        throw "RELEASE_DEPLOYMENT_SCRIPT_TRUST_BOOTSTRAP_MISSING: $script"
    }
}
$installerModulePath = Join-Path $root "installer\DbBackup.Install.Common.psm1"
if (-not (Test-Path -LiteralPath $installerModulePath -PathType Leaf)) {
    throw "RELEASE_INSTALLER_MODULE_MISSING"
}
$moduleTokens = $null
$moduleErrors = $null
[Management.Automation.Language.Parser]::ParseFile(
    $installerModulePath,
    [ref] $moduleTokens,
    [ref] $moduleErrors
) | Out-Null
if (@($moduleErrors).Count -ne 0) {
    throw "RELEASE_INSTALLER_MODULE_PARSE_FAILED"
}
$installerModuleText = Get-Content -LiteralPath $installerModulePath -Raw -Encoding UTF8
foreach ($requiredContract in @(
        "Move-DbBackupUnmanifestedInstallFiles",
        "INSTALL_TREE_REPARSE_POINT_BLOCKED",
        "install-tree-quarantine"
    )) {
    if (-not $installerModuleText.Contains($requiredContract)) {
        throw "RELEASE_EXACT_INSTALL_TREE_CONTRACT_MISSING: $requiredContract"
    }
}

$requiredG5 = @(
    "g5.update_fault_matrix",
    "g5.minimum_reader_guard",
    "g5.diagnostics_and_drain",
    "g5.support_bundle_canary",
    "g5.support_bundle_operation",
    "g5.telemetry_archive",
    "g5.installer_contract",
    "g5.release_policy"
)
if ((@($g5.commands.id) -join ",") -ne ($requiredG5 -join ",")) {
    throw "RELEASE_G5_MATRIX_INVALID"
}
$workflowRoot = Join-Path $root ".github\workflows"
$releaseWorkflow = Join-Path $workflowRoot "release.yml"
if (-not (Test-Path -LiteralPath $releaseWorkflow -PathType Leaf)) {
    throw "RELEASE_WORKFLOW_MISSING"
}
$workflowFiles = @(
    Get-ChildItem -LiteralPath $workflowRoot -File |
        Where-Object { $_.Extension -in @(".yml", ".yaml") }
)
if ($workflowFiles.Count -eq 0) {
    throw "RELEASE_WORKFLOW_MISSING"
}
$actionReferenceCount = 0
foreach ($workflow in $workflowFiles) {
    $workflowText = Get-Content -LiteralPath $workflow.FullName -Raw -Encoding UTF8
    $actionReferences = [regex]::Matches(
        $workflowText,
        "(?m)^\s*-\s*uses:\s*(?<reference>[^#\r\n]+)"
    )
    $actionReferenceCount += $actionReferences.Count
    foreach ($match in $actionReferences) {
        $reference = $match.Groups["reference"].Value.Trim()
        if ($reference -notmatch "@[0-9a-f]{40}$") {
            throw "RELEASE_WORKFLOW_ACTION_NOT_SHA_PINNED"
        }
    }
}
if ($actionReferenceCount -eq 0) {
    throw "RELEASE_WORKFLOW_ACTIONS_MISSING"
}
$releaseWorkflowText = Get-Content -LiteralPath $releaseWorkflow -Raw -Encoding UTF8
foreach ($requiredTrustControl in @(
        "Set-AuthenticodeSignature",
        "`$deploymentFiles",
        "EXPECTED_RELEASE_SIGNER",
        "DEPLOYMENT_SCRIPT_SIGNATURE_INVALID",
        "-ExpectedSignerThumbprint `$expectedSigner",
        "-ExpectedPackageManifestSha256 `$metadata.packageManifestSha256",
        "Prepare protected release staging",
        "Seal downloaded release staging",
        "SetAccessRuleProtection(`$true, `$false)",
        "Assert-DbBackupProtectedAcl",
        "-ExecutionPolicy AllSigned",
        "final-release-reevaluation:",
        "needs: [managed-evidence, signed-package, clean-windows-fixture, artifact-functional-qualification]",
        "-RequirePromotionEvidence",
        "-QualificationEvidencePath",
        "-SignedPackageManifestPath",
        "`$metadata.packageManifestSha256",
        "`$metadata.serviceBinarySha256",
        "`$metadata.cliBinarySha256",
        "`$metadata.artifactId",
        "dbbackup-exact-artifact-v1",
        "artifact-functional-qualification:",
        "dbbackup-managed-artifact",
        "ephemeral",
        "DBBACKUP_ARTIFACT_RUNNER_EPHEMERAL",
        ".\scripts\new-provider-certifications.ps1",
        "provider-certifications.json",
        ".\scripts\test-signed-artifact-functional.ps1",
        "artifact-functional-`${{ github.run_id }}",
        "-ArtifactFunctionalEvidencePath",
        "promotion-reevaluation-`${{ github.run_id }}",
        "dbbackup-signed-candidate-`${{ github.run_id }}",
        "publish-signed-package:",
        "needs: [signed-package, final-release-reevaluation]"
    )) {
    if (-not $releaseWorkflowText.Contains($requiredTrustControl)) {
        throw "RELEASE_DEPLOYMENT_SCRIPT_SIGNING_POLICY_MISSING: $requiredTrustControl"
    }
}
$artifactFunctionalJob = [regex]::Match(
    $releaseWorkflowText,
    "(?ms)^  artifact-functional-qualification:\s*\r?\n" +
    "(?<body>.*?)(?=^  [A-Za-z0-9_-]+:\s*(?:#.*)?$|\z)"
)
if (-not $artifactFunctionalJob.Success) {
    throw "RELEASE_ARTIFACT_FUNCTIONAL_JOB_MISSING"
}
$artifactFunctionalJobText = $artifactFunctionalJob.Value
foreach ($forbiddenDirectExecution in @(
        "(?im)\bcargo(?:\.exe)?\s+(?:build|run|test)\b",
        "(?im)\b(?:pg_dump|pg_restore|mysqldump|mariadb-dump)(?:\.exe)?\b",
        "(?im)\bdbbackup-pipeline\b"
    )) {
    if ($artifactFunctionalJobText -match $forbiddenDirectExecution) {
        throw "RELEASE_ARTIFACT_FUNCTIONAL_DIRECT_EXECUTION_FORBIDDEN"
    }
}
$deploymentSigningIndex = $releaseWorkflowText.IndexOf(
    "Set-AuthenticodeSignature",
    [StringComparison]::Ordinal
)
$checksumIndex = $releaseWorkflowText.IndexOf(
    '$checksums = Get-ChildItem',
    [StringComparison]::Ordinal
)
$fixtureSignerIndex = $releaseWorkflowText.IndexOf(
    "EXPECTED_RELEASE_SIGNER",
    [StringComparison]::Ordinal
)
$fixtureVerificationIndex = if ($fixtureSignerIndex -ge 0) {
    $releaseWorkflowText.IndexOf(
        "Get-AuthenticodeSignature",
        $fixtureSignerIndex,
        [StringComparison]::Ordinal
    )
}
else {
    -1
}
$fixtureMetadataIndex = if ($fixtureVerificationIndex -ge 0) {
    $releaseWorkflowText.IndexOf(
        "signing-metadata.json",
        $fixtureVerificationIndex,
        [StringComparison]::Ordinal
    )
}
else {
    -1
}
if ($deploymentSigningIndex -lt 0 -or
    $checksumIndex -lt $deploymentSigningIndex -or
    $fixtureVerificationIndex -lt $fixtureSignerIndex -or
    $fixtureMetadataIndex -lt $fixtureVerificationIndex) {
    throw "RELEASE_DEPLOYMENT_SCRIPT_TRUST_ORDER_INVALID"
}

$revision = (& git -C $root rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $revision -notmatch "^[0-9a-f]{40}$") {
    throw "RELEASE_SOURCE_REVISION_UNAVAILABLE"
}
$evidence = [ordered] @{
    schemaVersion = 1
    gateId = "G5-POLICY"
    sourceRevision = $revision
    checkedAt = [DateTimeOffset]::Now.ToString("o")
    status = "pass"
    safetyInvariantCount = @($rollout.safetyInvariants.PSObject.Properties).Count
    rolloutStages = @($rollout.stages.id)
    deferredBoundaries = @($rollout.deferred)
    shaPinnedActionReferences = $actionReferenceCount
    claims = [ordered] @{
        sqlite = $claimMap["engine.sqlite"].claim
        sqlserver = $claimMap["engine.sqlserver"].claim
        diagnostics = $claimMap["service.diagnostics"].claim
        supportBundle = $claimMap["support.bundle"].claim
        telemetryArchive = $claimMap["telemetry.archive"].claim
        windowsInstallUpdate = $claimMap["windows.install_update"].claim
        tauriBundle = $claimMap["tauri.release_bundle"].claim
    }
}
$evidencePath = Join-Path $output "release-policy.json"
[IO.File]::WriteAllText(
    $evidencePath,
    ($evidence | ConvertTo-Json -Depth 16),
    [Text.UTF8Encoding]::new($false)
)
$evidence | ConvertTo-Json -Depth 16
