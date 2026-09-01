[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $EvidencePath,
    [Parameter(Mandatory = $true)] [string] $ExpectedPackageManifestSha256,
    [Parameter(Mandatory = $true)] [string] $ExpectedServiceBinarySha256,
    [Parameter(Mandatory = $true)] [string] $ExpectedCliBinarySha256
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

function Get-TextSha256 {
    param([Parameter(Mandatory = $true)] [string] $Value)

    $sha = [Security.Cryptography.SHA256]::Create()
    try {
        return ([BitConverter]::ToString(
                $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($Value))
            )).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $sha.Dispose()
    }
}

function Assert-Digest {
    param(
        [AllowNull()] [object] $Value,
        [Parameter(Mandatory = $true)] [string] $Code
    )

    if ($Value -isnot [string] -or $Value -cnotmatch "^[0-9a-f]{64}$") {
        throw $Code
    }
}

function Assert-Uuid {
    param(
        [AllowNull()] [object] $Value,
        [Parameter(Mandatory = $true)] [string] $Code
    )

    $parsed = [Guid]::Empty
    if ($Value -isnot [string] -or
        -not [Guid]::TryParseExact([string] $Value, "D", [ref] $parsed) -or
        $parsed -eq [Guid]::Empty -or
        ([string] $Value) -cne $parsed.ToString("D")) {
        throw $Code
    }
}

function Assert-Properties {
    param(
        [AllowNull()] [object] $Document,
        [Parameter(Mandatory = $true)] [string[]] $Names,
        [Parameter(Mandatory = $true)] [string] $Code
    )

    if ($null -eq $Document) {
        throw $Code
    }
    $actual = @($Document.PSObject.Properties.Name | Sort-Object)
    $expected = @($Names | Sort-Object)
    if (($actual -join "`n") -cne ($expected -join "`n")) {
        throw $Code
    }
}

foreach ($digest in @(
        $ExpectedPackageManifestSha256,
        $ExpectedServiceBinarySha256,
        $ExpectedCliBinarySha256
    )) {
    Assert-Digest -Value $digest -Code "ARTIFACT_FUNCTIONAL_EXPECTED_DIGEST_INVALID"
}

$absoluteEvidencePath = [IO.Path]::GetFullPath($EvidencePath)
$evidenceMetadata = Get-Item -LiteralPath $absoluteEvidencePath -Force `
    -ErrorAction SilentlyContinue
if ($null -eq $evidenceMetadata -or
    $evidenceMetadata.PSIsContainer -or
    $evidenceMetadata.Length -le 0 -or
    $evidenceMetadata.Length -gt 4MB -or
    $evidenceMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "ARTIFACT_FUNCTIONAL_EVIDENCE_MISSING"
}
try {
    $evidence = Get-Content -LiteralPath $absoluteEvidencePath -Raw -Encoding UTF8 |
        ConvertFrom-Json
}
catch {
    throw "ARTIFACT_FUNCTIONAL_EVIDENCE_INVALID"
}

$topLevelProperties = @(
    "schemaVersion",
    "status",
    "trustContext",
    "sourceRevision",
    "sourceTreeDigest",
    "buildId",
    "artifactId",
    "packageManifestSha256",
    "serviceBinarySha256",
    "cliBinarySha256",
    "installedServiceBinarySha256",
    "installedCliBinarySha256",
    "executionAuthority",
    "installedCliIpc",
    "fixtureIsolation",
    "cleanup",
    "engines"
)
Assert-Properties -Document $evidence -Names $topLevelProperties `
    -Code "ARTIFACT_FUNCTIONAL_EVIDENCE_INVALID"
if ($evidence.schemaVersion -ne 1 -or
    $evidence.status -ne "pass" -or
    $evidence.trustContext -ne
        "exact_signed_artifact_managed_functional_qualification" -or
    $evidence.sourceRevision -cnotmatch "^(?:[0-9a-f]{40}|[0-9a-f]{64})$" -or
    $evidence.executionAuthority -ne "service" -or
    $evidence.installedCliIpc -ne "pass" -or
    $evidence.fixtureIsolation -ne "managed_ephemeral" -or
    $evidence.cleanup -ne "pass") {
    throw "ARTIFACT_FUNCTIONAL_EVIDENCE_INVALID"
}
foreach ($property in @(
        "sourceTreeDigest",
        "buildId",
        "artifactId",
        "packageManifestSha256",
        "serviceBinarySha256",
        "cliBinarySha256",
        "installedServiceBinarySha256",
        "installedCliBinarySha256"
    )) {
    Assert-Digest -Value $evidence.$property -Code "ARTIFACT_FUNCTIONAL_EVIDENCE_INVALID"
}

if ($evidence.packageManifestSha256 -cne $ExpectedPackageManifestSha256 -or
    $evidence.serviceBinarySha256 -cne $ExpectedServiceBinarySha256 -or
    $evidence.cliBinarySha256 -cne $ExpectedCliBinarySha256 -or
    $evidence.installedServiceBinarySha256 -cne $ExpectedServiceBinarySha256 -or
    $evidence.installedCliBinarySha256 -cne $ExpectedCliBinarySha256) {
    throw "ARTIFACT_FUNCTIONAL_PACKAGE_DIGEST_MISMATCH"
}
$expectedArtifactId = Get-TextSha256 -Value (
    "dbbackup-exact-artifact-v1`n" +
    "$ExpectedPackageManifestSha256`n" +
    "$ExpectedServiceBinarySha256`n" +
    "$ExpectedCliBinarySha256`n"
)
if ($evidence.artifactId -cne $expectedArtifactId) {
    throw "ARTIFACT_FUNCTIONAL_ARTIFACT_ID_MISMATCH"
}

$engines = @($evidence.engines)
$engineNames = @($engines | ForEach-Object { [string] $_.engine })
if ($engines.Count -ne 3 -or
    (($engineNames | Sort-Object) -join ",") -cne "mariadb,mysql,postgresql" -or
    @($engineNames | Select-Object -Unique).Count -ne 3) {
    throw "ARTIFACT_FUNCTIONAL_ENGINE_SET_INVALID"
}

foreach ($engineEvidence in $engines) {
    Assert-Properties -Document $engineEvidence `
        -Names @("engine", "scope", "backup", "restore", "verification") `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    if ($engineEvidence.engine -notin @("postgresql", "mysql", "mariadb")) {
        throw "ARTIFACT_FUNCTIONAL_ENGINE_SET_INVALID"
    }
    Assert-Properties -Document $engineEvidence.scope `
        -Names @("mode", "scopeIdentity", "includedObjectIds", "excludedObjectId") `
        -Code "ARTIFACT_FUNCTIONAL_SCOPE_INVALID"
    $includedObjectIds = @($engineEvidence.scope.includedObjectIds)
    if ($engineEvidence.scope.mode -ne "explicit_tables" -or
        [string]::IsNullOrWhiteSpace([string] $engineEvidence.scope.scopeIdentity) -or
        ([string] $engineEvidence.scope.scopeIdentity).Length -gt 512 -or
        $includedObjectIds.Count -lt 1 -or
        $includedObjectIds.Count -gt 64 -or
        @($includedObjectIds | Select-Object -Unique).Count -ne $includedObjectIds.Count -or
        @($includedObjectIds | Where-Object {
                $_ -isnot [string] -or
                [string]::IsNullOrWhiteSpace([string] $_) -or
                ([string] $_).Length -gt 512
            }).Count -ne 0 -or
        [string]::IsNullOrWhiteSpace([string] $engineEvidence.scope.excludedObjectId) -or
        ([string] $engineEvidence.scope.excludedObjectId).Length -gt 512 -or
        $engineEvidence.scope.excludedObjectId -in $includedObjectIds) {
        throw "ARTIFACT_FUNCTIONAL_SCOPE_INVALID"
    }

    Assert-Properties -Document $engineEvidence.backup `
        -Names @(
            "status", "workItemId", "recoveryPointId", "signedManifest",
            "requiredReader", "programOwnedWorker"
        ) `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    Assert-Properties -Document $engineEvidence.restore `
        -Names @(
            "status", "mode", "restoreRunId", "contentOracle",
            "selectedContentSha256", "restoredContentSha256",
            "excludedObjectAbsent", "programOwnedWorker"
        ) `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    Assert-Properties -Document $engineEvidence.verification `
        -Names @("status", "level", "verificationRunId", "programOwnedWorker") `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"

    if ($engineEvidence.backup.programOwnedWorker -ne "pass" -or
        $engineEvidence.restore.programOwnedWorker -ne "pass" -or
        $engineEvidence.verification.programOwnedWorker -ne "pass") {
        throw "ARTIFACT_FUNCTIONAL_WORKER_PATH_INVALID"
    }
    if ($engineEvidence.backup.status -ne "pass" -or
        $engineEvidence.backup.signedManifest -ne "pass" -or
        $engineEvidence.backup.requiredReader -ne "pass" -or
        $engineEvidence.restore.status -ne "pass" -or
        $engineEvidence.restore.mode -ne "create_new" -or
        $engineEvidence.restore.contentOracle -ne "pass" -or
        $engineEvidence.restore.excludedObjectAbsent -ne "pass" -or
        $engineEvidence.verification.status -ne "pass" -or
        $engineEvidence.verification.level -ne 1) {
        throw "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    }
    Assert-Uuid -Value $engineEvidence.backup.workItemId `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    Assert-Uuid -Value $engineEvidence.backup.recoveryPointId `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    Assert-Uuid -Value $engineEvidence.restore.restoreRunId `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    Assert-Uuid -Value $engineEvidence.verification.verificationRunId `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    Assert-Digest -Value $engineEvidence.restore.selectedContentSha256 `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    Assert-Digest -Value $engineEvidence.restore.restoredContentSha256 `
        -Code "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
    if ($engineEvidence.restore.selectedContentSha256 -cne
        $engineEvidence.restore.restoredContentSha256) {
        throw "ARTIFACT_FUNCTIONAL_CONTENT_ORACLE_MISMATCH"
    }
}

[ordered] @{
    schemaVersion = 1
    status = "pass"
    artifactId = $expectedArtifactId
    engineCount = $engines.Count
} | ConvertTo-Json -Compress
