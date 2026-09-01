$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$checker = Join-Path $root "scripts\check-release-policy.ps1"
$hostPath = (Get-Process -Id $PID).Path
$failures = [Collections.Generic.List[string]]::new()

function Add-Failure {
    param([Parameter(Mandatory = $true)] [string] $Message)
    $failures.Add($Message)
}

function Invoke-Checker {
    param(
        [Parameter(Mandatory = $true)] [string] $CaseRoot,
        [Parameter(Mandatory = $true)] [string] $OutputRoot
    )

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $lines = @(
            & $hostPath `
                -NoProfile `
                -File $checker `
                -RepositoryRoot $CaseRoot `
                -OutputRoot $OutputRoot 2>&1
        )
        return [PSCustomObject] @{
            exitCode = $LASTEXITCODE
            output = $lines -join "`n"
        }
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
}

$canonical = Invoke-Checker `
    -CaseRoot $root `
    -OutputRoot (Join-Path $root "artifacts\g5")
if ($canonical.exitCode -ne 0) {
    Add-Failure "canonical release policy did not pass"
}
$releaseWorkflowText = Get-Content -LiteralPath (
    Join-Path $root ".github\workflows\release.yml"
) -Raw -Encoding UTF8
foreach ($requiredFailClosedCheck in @(
        '$health.result.updatePending -ne $false',
        '$health.result.phase -ne "production_ready"',
        '$health.result.executionAuthority -ne "service"',
        '$health.result.claimsEnabled -ne $true',
        '${{ vars.AUDITED_NATIVE_TOOL_ROOT }}',
        '${{ vars.AUDITED_NATIVE_TOOL_AUDIT_SHA256 }}',
        'RELEASE_AUDITED_NATIVE_TOOL_INPUT_MISSING',
        '.\scripts\stage-native-tools.ps1',
        'same_artifact_lifecycle_fixture_not_production_trust',
        '-ExpectedNativeToolAuditSha256 $metadata.nativeToolAuditSha256',
        '-ExpectedPackageManifestSha256 $metadata.packageManifestSha256',
        'Prepare protected release staging',
        'Seal downloaded release staging',
        'SetAccessRuleProtection($true, $false)',
        'Assert-DbBackupProtectedAcl',
        '-ExecutionPolicy AllSigned',
        'Set-AuthenticodeSignature',
        '$deploymentFiles',
        'EXPECTED_RELEASE_SIGNER',
        'DEPLOYMENT_SCRIPT_SIGNATURE_INVALID',
        '-ExpectedSignerThumbprint $expectedSigner',
        'final-release-reevaluation:',
        '-RequirePromotionEvidence',
        '-QualificationEvidencePath',
        '-SignedPackageManifestPath',
        '$qualification.buildId',
        '$metadata.sourceTreeDigest',
        '$manifestDocument.sourceTreeDigest',
        '$metadata.packageManifestSha256',
        '$metadata.serviceBinarySha256',
        '$metadata.cliBinarySha256',
        '$metadata.artifactId',
        'dbbackup-exact-artifact-v1',
        'artifact-functional-qualification:',
        'dbbackup-managed-artifact',
        '.\scripts\test-signed-artifact-functional.ps1',
        'artifact-functional-${{ github.run_id }}',
        '-ArtifactFunctionalEvidencePath',
        'promotion-reevaluation-${{ github.run_id }}',
        'dbbackup-signed-candidate-${{ github.run_id }}',
        'publish-signed-package:',
        'needs: [signed-package, final-release-reevaluation]'
    )) {
    if (-not $releaseWorkflowText.Contains($requiredFailClosedCheck)) {
        Add-Failure (
            "clean Windows fixture is missing fail-closed check: " +
            $requiredFailClosedCheck
        )
    }
}
$packageSchema = Get-Content -LiteralPath (
    Join-Path $root "release\package-manifest.schema.json"
) -Raw -Encoding UTF8 | ConvertFrom-Json
if ("sourceTreeDigest" -notin @($packageSchema.required) -or
    $packageSchema.properties.sourceTreeDigest.pattern -ne "^[0-9a-f]{64}$") {
    Add-Failure "package manifest schema does not bind the source tree digest"
}
foreach ($schemaContract in @(
        @(
            "release\signing-metadata.schema.json",
            "DbBackup production signing metadata"
        ),
        @(
            "release\release-fixture-evidence.schema.json",
            "DbBackup pristine signed lifecycle fixture evidence"
        ),
        @(
            "release\artifact-functional-evidence.schema.json",
            "DbBackup exact signed artifact functional qualification evidence"
        )
    )) {
    $schemaPath = Join-Path $root $schemaContract[0]
    if (-not (Test-Path -LiteralPath $schemaPath -PathType Leaf)) {
        Add-Failure "release identity schema is missing: $($schemaContract[0])"
        continue
    }
    $schema = Get-Content -LiteralPath $schemaPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    if ($schema.title -ne $schemaContract[1] -or
        "sourceRevision" -notin @($schema.required) -or
        "sourceTreeDigest" -notin @($schema.required) -or
        "buildId" -notin @($schema.required) -or
        "packageManifestSha256" -notin @($schema.required) -or
        "serviceBinarySha256" -notin @($schema.required) -or
        "cliBinarySha256" -notin @($schema.required)) {
        Add-Failure "release identity schema drifted: $($schemaContract[0])"
    }
}
$functionalSchema = Get-Content -LiteralPath (
    Join-Path $root "release\artifact-functional-evidence.schema.json"
) -Raw -Encoding UTF8 | ConvertFrom-Json
foreach ($required in @(
        "artifactId",
        "packageManifestSha256",
        "serviceBinarySha256",
        "cliBinarySha256",
        "installedServiceBinarySha256",
        "installedCliBinarySha256",
        "engines"
    )) {
    if ($required -notin @($functionalSchema.required)) {
        Add-Failure "artifact functional schema is missing: $required"
    }
}
$nativeToolStageText = Get-Content -LiteralPath (
    Join-Path $root "scripts\stage-native-tools.ps1"
) -Raw -Encoding UTF8
foreach ($requiredNativeToolGate in @(
        "NATIVE_TOOL_VERSION_PROBE_FAILED",
        "NATIVE_TOOL_AUTHENTICODE_INVALID",
        "NATIVE_TOOL_UNAUDITED_FILE"
    )) {
    if (-not $nativeToolStageText.Contains($requiredNativeToolGate)) {
        Add-Failure "native tool stage is missing gate: $requiredNativeToolGate"
    }
}

$temporary = Join-Path ([IO.Path]::GetTempPath()) (
    "dbbackup-release-policy-" + [Guid]::NewGuid().ToString("N")
)
New-Item -ItemType Directory -Path $temporary -Force | Out-Null
try {
    foreach ($directory in @(
            ".github\workflows",
            "release\gates",
            "scripts",
            "scripts\tests",
            "installer"
        )) {
        New-Item -ItemType Directory -Path (Join-Path $temporary $directory) `
            -Force | Out-Null
    }
    Copy-Item `
        -LiteralPath (Join-Path $root "release\gates\g0-claims.json") `
        -Destination (Join-Path $temporary "release\gates\g0-claims.json")
    Copy-Item `
        -LiteralPath (Join-Path $root "release\gates\g5-release-matrix.json") `
        -Destination (Join-Path $temporary "release\gates\g5-release-matrix.json")
    foreach ($file in @(
            "release\rollout-policy.json",
            "release\expansion-boundaries.json",
            "release\package-manifest.schema.json",
            "release\signing-metadata.schema.json",
            "release\release-fixture-evidence.schema.json",
            "release\artifact-functional-evidence.schema.json",
            "release\provider-certifications.schema.json",
            "release\native-tool-audit.schema.json",
            "release\provider-promotion.schema.json",
            "scripts\install.ps1",
            "scripts\repair.ps1",
            "scripts\update.ps1",
            "scripts\uninstall.ps1",
            "scripts\stage-native-tools.ps1",
            "scripts\test-release-matrix.ps1",
            "scripts\validate-artifact-functional-evidence.ps1",
            "scripts\new-provider-certifications.ps1",
            "scripts\test-signed-artifact-functional.ps1"
        )) {
        $destination = Join-Path $temporary $file
        New-Item -ItemType Directory -Path (Split-Path -Parent $destination) `
            -Force | Out-Null
        Copy-Item -LiteralPath (Join-Path $root $file) -Destination $destination
    }
    Copy-Item `
        -LiteralPath (Join-Path $root "installer\DbBackup.Install.Common.psm1") `
        -Destination (Join-Path $temporary "installer\DbBackup.Install.Common.psm1")
    $fixtureWorkflow = @"
name: fixture
on: workflow_dispatch
jobs:
  fixture:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262
      - shell: powershell
        run: |
          # Prepare protected release staging
          # Seal downloaded release staging
          `$acl.SetAccessRuleProtection(`$true, `$false)
          Assert-DbBackupProtectedAcl
          '-ExecutionPolicy AllSigned' | Out-Null
          `$deploymentFiles = @('fixture.ps1')
          Set-AuthenticodeSignature -LiteralPath `$deploymentFiles[0]
          `$checksums = Get-ChildItem .
          `$env:EXPECTED_RELEASE_SIGNER | Out-Null
          Get-AuthenticodeSignature -LiteralPath `$deploymentFiles[0]
          'signing-metadata.json' | Out-Null
          'DEPLOYMENT_SCRIPT_SIGNATURE_INVALID' | Out-Null
          '-ExpectedSignerThumbprint `$expectedSigner' | Out-Null
          '-ExpectedPackageManifestSha256 `$metadata.packageManifestSha256' | Out-Null
  final-release-reevaluation:
    needs: [managed-evidence, signed-package, clean-windows-fixture, artifact-functional-qualification]
    runs-on: windows-latest
    steps:
      - shell: powershell
        run: |
          '-RequirePromotionEvidence' | Out-Null
          '-QualificationEvidencePath' | Out-Null
          '-SignedPackageManifestPath' | Out-Null
          '-ArtifactFunctionalEvidencePath' | Out-Null
          `$metadata.packageManifestSha256 | Out-Null
          `$metadata.serviceBinarySha256 | Out-Null
          `$metadata.cliBinarySha256 | Out-Null
          `$metadata.artifactId | Out-Null
          'dbbackup-exact-artifact-v1' | Out-Null
          '.\scripts\new-provider-certifications.ps1' | Out-Null
          'provider-certifications.json' | Out-Null
          'promotion-reevaluation-`${{ github.run_id }}' | Out-Null
          'dbbackup-signed-candidate-`${{ github.run_id }}' | Out-Null
  artifact-functional-qualification:
    needs: signed-package
    runs-on: [self-hosted, windows, x64, dbbackup-managed-artifact, ephemeral]
    steps:
      - shell: powershell
        run: |
          `$env:DBBACKUP_ARTIFACT_RUNNER_EPHEMERAL | Out-Null
          .\scripts\test-signed-artifact-functional.ps1
      - shell: powershell
        run: 'artifact-functional-`${{ github.run_id }}' | Out-Null
  publish-signed-package:
    needs: [signed-package, final-release-reevaluation]
    runs-on: windows-latest
    steps: []
"@
    $workflowPath = Join-Path $temporary ".github\workflows\release.yml"
    [IO.File]::WriteAllText(
        $workflowPath,
        $fixtureWorkflow,
        [Text.UTF8Encoding]::new($false)
    )
    & git -C $temporary init --quiet
    & git -C $temporary config core.autocrlf false
    & git -C $temporary add .
    & git -C $temporary `
        -c user.name=DbBackupFixture `
        -c user.email=fixture@invalid.example `
        commit --quiet -m fixture
    if ($LASTEXITCODE -ne 0) {
        throw "release policy fixture git initialization failed"
    }

    $valid = Invoke-Checker `
        -CaseRoot $temporary `
        -OutputRoot (Join-Path $temporary "artifacts\g5")
    if ($valid.exitCode -ne 0) {
        Add-Failure "SHA-pinned action fixture did not pass: $($valid.output)"
    }

    $directSubstitutionWorkflow = $fixtureWorkflow.Replace(
        ".\scripts\test-signed-artifact-functional.ps1",
        "cargo run --example forbidden-substitute`n" +
        "          .\scripts\test-signed-artifact-functional.ps1"
    )
    [IO.File]::WriteAllText(
        $workflowPath,
        $directSubstitutionWorkflow,
        [Text.UTF8Encoding]::new($false)
    )
    $directSubstitution = Invoke-Checker `
        -CaseRoot $temporary `
        -OutputRoot (Join-Path $temporary "artifacts\g5-direct-substitution")
    if ($directSubstitution.exitCode -eq 0 -or
        -not $directSubstitution.output.Contains(
            "RELEASE_ARTIFACT_FUNCTIONAL_DIRECT_EXECUTION_FORBIDDEN"
        )) {
        Add-Failure "artifact functional direct-execution substitute was not rejected"
    }
    [IO.File]::WriteAllText(
        $workflowPath,
        $fixtureWorkflow,
        [Text.UTF8Encoding]::new($false)
    )

    $rolloutFixturePath = Join-Path $temporary "release\rollout-policy.json"
    $canonicalRolloutFixture = Get-Content -LiteralPath $rolloutFixturePath `
        -Raw -Encoding UTF8
    foreach ($requiredInvariant in @(
            "partialScopesReportedAsWholeDatabase",
            "recoveryPointsWithoutSignedManifestOrRequiredReplicaEvidence",
            "setManifestPayloadDeletionViolations"
        )) {
        $missingInvariantFixture = $canonicalRolloutFixture | ConvertFrom-Json
        $missingInvariantFixture.safetyInvariants.PSObject.Properties.Remove(
            $requiredInvariant
        )
        [IO.File]::WriteAllText(
            $rolloutFixturePath,
            ($missingInvariantFixture | ConvertTo-Json -Depth 16),
            [Text.UTF8Encoding]::new($false)
        )
        $missingInvariant = Invoke-Checker `
            -CaseRoot $temporary `
            -OutputRoot (Join-Path $temporary "artifacts\g5-missing-invariant")
        if ($missingInvariant.exitCode -eq 0 -or
            -not $missingInvariant.output.Contains(
                "RELEASE_SAFETY_INVARIANT_MISSING"
            )) {
            Add-Failure "missing safety invariant was not rejected: $requiredInvariant"
        }
    }
    [IO.File]::WriteAllText(
        $rolloutFixturePath,
        $canonicalRolloutFixture,
        [Text.UTF8Encoding]::new($false)
    )
    foreach ($requiredInvariant in @(
            "partialScopesReportedAsWholeDatabase",
            "recoveryPointsWithoutSignedManifestOrRequiredReplicaEvidence",
            "setManifestPayloadDeletionViolations"
        )) {
        $nonzeroInvariantFixture = $canonicalRolloutFixture | ConvertFrom-Json
        $nonzeroInvariantFixture.safetyInvariants.$requiredInvariant = 1
        [IO.File]::WriteAllText(
            $rolloutFixturePath,
            ($nonzeroInvariantFixture | ConvertTo-Json -Depth 16),
            [Text.UTF8Encoding]::new($false)
        )
        $nonzeroInvariant = Invoke-Checker `
            -CaseRoot $temporary `
            -OutputRoot (Join-Path $temporary "artifacts\g5-nonzero-invariant")
        if ($nonzeroInvariant.exitCode -eq 0 -or
            -not $nonzeroInvariant.output.Contains(
                "RELEASE_SAFETY_INVARIANT_MUST_BE_ZERO"
            )) {
            Add-Failure "nonzero safety invariant was not rejected: $requiredInvariant"
        }
    }
    [IO.File]::WriteAllText(
        $rolloutFixturePath,
        $canonicalRolloutFixture,
        [Text.UTF8Encoding]::new($false)
    )

    [IO.File]::WriteAllText(
        $workflowPath,
        $fixtureWorkflow.Replace(
            "@11d5960a326750d5838078e36cf38b85af677262",
            "@v4"
        ),
        [Text.UTF8Encoding]::new($false)
    )
    $invalid = Invoke-Checker `
        -CaseRoot $temporary `
        -OutputRoot (Join-Path $temporary "artifacts\g5-invalid")
    if ($invalid.exitCode -eq 0 -or
        -not $invalid.output.Contains(
            "RELEASE_WORKFLOW_ACTION_NOT_SHA_PINNED"
        )) {
        Add-Failure "unpinned action fixture was not rejected"
    }
}
finally {
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    $resolvedTemporary = [IO.Path]::GetFullPath($temporary)
    if ($resolvedTemporary.StartsWith(
            $tempRoot,
            [StringComparison]::OrdinalIgnoreCase
        ) -and
        $resolvedTemporary -ne $tempRoot -and
        (Test-Path -LiteralPath $resolvedTemporary)) {
        Remove-Item -LiteralPath $resolvedTemporary -Recurse -Force
    }
}

if ($failures.Count -ne 0) {
    foreach ($failure in $failures) {
        [Console]::Error.WriteLine($failure)
    }
    exit 1
}
Write-Output "release policy contract tests passed"
exit 0
