$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$scriptPath = Join-Path $root "scripts\test-release-matrix.ps1"
$hostPath = (Get-Process -Id $PID).Path
$failures = [Collections.Generic.List[string]]::new()

function Add-Failure {
    param([Parameter(Mandatory = $true)] [string] $Message)
    $failures.Add($Message)
}

function Invoke-ValidateOnly {
    param([string] $MatrixPath)

    $arguments = @(
        "-NoProfile",
        "-File", $scriptPath,
        "-RepositoryRoot", $root,
        "-ValidateOnly"
    )
    if (-not [string]::IsNullOrWhiteSpace($MatrixPath)) {
        $arguments += @("-MatrixPath", $MatrixPath)
    }
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $lines = @(& (Get-Process -Id $PID).Path @arguments 2>&1)
        $exitCode = $LASTEXITCODE
        return [PSCustomObject] @{
            exitCode = $exitCode
            output = $lines -join "`n"
        }
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
}

function Write-Json {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [object] $Value
    )

    New-Item -ItemType Directory -Path (Split-Path -Parent $Path) -Force |
        Out-Null
    [IO.File]::WriteAllText(
        $Path,
        ($Value | ConvertTo-Json -Depth 32),
        [Text.UTF8Encoding]::new($false)
    )
}

function Get-FileSha256 {
    param([Parameter(Mandatory = $true)] [string] $Path)

    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).
        Hash.ToLowerInvariant()
}

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

function Invoke-G5Run {
    param(
        [Parameter(Mandatory = $true)] [string] $CaseRoot,
        [Parameter(Mandatory = $true)] [string] $OutputRoot,
        [string] $QualificationEvidencePath,
        [string] $SigningMetadataPath,
        [string] $SignedPackageManifestPath,
        [string] $SignedFixtureEvidencePath,
        [string] $ArtifactFunctionalEvidencePath,
        [switch] $RequireUpstreamEvidence,
        [switch] $RequirePromotionEvidence
    )

    $arguments = @(
        "-NoProfile",
        "-File", $scriptPath,
        "-RepositoryRoot", $CaseRoot,
        "-OutputRoot", $OutputRoot
    )
    foreach ($pair in @(
            @("QualificationEvidencePath", $QualificationEvidencePath),
            @("SigningMetadataPath", $SigningMetadataPath),
            @("SignedPackageManifestPath", $SignedPackageManifestPath),
            @("SignedFixtureEvidencePath", $SignedFixtureEvidencePath),
            @("ArtifactFunctionalEvidencePath", $ArtifactFunctionalEvidencePath)
        )) {
        if (-not [string]::IsNullOrWhiteSpace([string] $pair[1])) {
            $arguments += @("-$($pair[0])", [string] $pair[1])
        }
    }
    if ($RequireUpstreamEvidence) {
        $arguments += "-RequireUpstreamEvidence"
    }
    if ($RequirePromotionEvidence) {
        $arguments += "-RequirePromotionEvidence"
    }
    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $lines = @(& $hostPath @arguments 2>&1)
        return [PSCustomObject] @{
            exitCode = $LASTEXITCODE
            output = $lines -join "`n"
        }
    }
    finally {
        $ErrorActionPreference = $previousErrorAction
    }
}

$valid = Invoke-ValidateOnly
if ($valid.exitCode -ne 0) {
    Add-Failure "canonical G5 matrix did not validate"
}
else {
    try {
        $contract = $valid.output | ConvertFrom-Json
        $expected = @(
            "g5.update_fault_matrix",
            "g5.minimum_reader_guard",
            "g5.diagnostics_and_drain",
            "g5.support_bundle_canary",
            "g5.support_bundle_operation",
            "g5.telemetry_archive",
            "g5.installer_contract",
            "g5.release_policy"
        )
        $expectedUpstream = @(
            "g0.current_build",
            "g1.durable_dispatcher",
            "g2.existing_engines",
            "g2.selective_scopes",
            "g3.remote_recovery",
            "g3.postgresql_restore",
            "g4.wave_one"
        )
        if ($contract.schemaVersion -ne 1 -or
            $contract.gateId -ne "G5" -or
            $contract.mode -ne "validate_only" -or
            (@($contract.commandIds) -join ",") -ne ($expected -join ",") -or
            (@($contract.upstreamIds) -join ",") -ne ($expectedUpstream -join ",") -or
            (@($contract.externalRequirementIds) -join ",") -ne (
                @(
                    "production_authenticode_certificate",
                    "rfc3161_timestamp_service",
                    "clean_windows_install_fixture",
                    "signed_update_fixture",
                    "exact_signed_artifact_functional_qualification",
                    "owner_approved_stop_metrics"
                ) -join ","
            ) -or
            $contract.promotionStatus -ne "not_evaluated") {
            Add-Failure "canonical G5 validation contract drifted"
        }
    }
    catch {
        Add-Failure "canonical G5 validation output was not JSON"
    }
}

$temporary = Join-Path ([IO.Path]::GetTempPath()) (
    "restoreplane-g5-contract-" + [Guid]::NewGuid().ToString("N")
)
New-Item -ItemType Directory -Path $temporary -Force | Out-Null
try {
    $matrix = Get-Content `
        -LiteralPath (Join-Path $root "release\gates\g5-release-matrix.json") `
        -Raw -Encoding UTF8 |
        ConvertFrom-Json
    $first = $matrix.commands[0]
    $matrix.commands[0] = $matrix.commands[1]
    $matrix.commands[1] = $first
    $invalidPath = Join-Path $temporary "invalid-order.json"
    [IO.File]::WriteAllText(
        $invalidPath,
        ($matrix | ConvertTo-Json -Depth 16),
        [Text.UTF8Encoding]::new($false)
    )
    $invalid = Invoke-ValidateOnly -MatrixPath $invalidPath
    if ($invalid.exitCode -eq 0 -or
        -not $invalid.output.Contains("G5_MATRIX_COMMAND_SET_INVALID")) {
        Add-Failure "invalid G5 command ordering was not rejected"
    }
}
finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force
}

$bindingRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "restoreplane-g5-build-binding-" + [Guid]::NewGuid().ToString("N")
)
New-Item -ItemType Directory -Path $bindingRoot -Force | Out-Null
try {
    $releaseRoot = Join-Path $bindingRoot "release"
    $gateRoot = Join-Path $releaseRoot "gates"
    New-Item -ItemType Directory -Path $gateRoot -Force | Out-Null
    $scriptRoot = Join-Path $bindingRoot "scripts"
    New-Item -ItemType Directory -Path $scriptRoot -Force | Out-Null
    Copy-Item -LiteralPath (
        Join-Path $root "scripts\validate-artifact-functional-evidence.ps1"
    ) -Destination $scriptRoot
    $matrix = Get-Content `
        -LiteralPath (Join-Path $root "release\gates\g5-release-matrix.json") `
        -Raw -Encoding UTF8 |
        ConvertFrom-Json
    foreach ($command in @($matrix.commands)) {
        $command.executable = $hostPath
        $command.arguments = @("-NoProfile", "-Command", "exit 0")
    }
    Write-Json -Path (Join-Path $gateRoot "g5-release-matrix.json") `
        -Value $matrix
    Write-Json -Path (Join-Path $releaseRoot "rollout-policy.json") -Value (
        [ordered] @{
            schemaVersion = 1
            ownerApprovedStopMetrics = [ordered] @{
                status = "pending_owner_approval"
            }
        }
    )
    & git -C $bindingRoot init --quiet
    & git -C $bindingRoot config user.name "RestorePlane fixture"
    & git -C $bindingRoot config user.email "fixture@invalid.example"
    & git -C $bindingRoot add release
    & git -C $bindingRoot commit --quiet -m fixture
    if ($LASTEXITCODE -ne 0) {
        throw "G5 binding fixture git initialization failed"
    }

    $qualificationRoot = Join-Path $bindingRoot "artifacts\qualification"
    $qualificationRun = Invoke-G5Run `
        -CaseRoot $bindingRoot `
        -OutputRoot $qualificationRoot
    $qualificationPath = Join-Path $qualificationRoot "release-matrix.json"
    if ($qualificationRun.exitCode -ne 0 -or
        -not (Test-Path -LiteralPath $qualificationPath -PathType Leaf)) {
        Add-Failure "qualification fixture did not produce G5 evidence"
    }
    else {
        $qualification = Get-Content -LiteralPath $qualificationPath `
            -Raw -Encoding UTF8 | ConvertFrom-Json
        $qualification.execution.managedRequested = $true
        $qualification.execution.upstreamRequired = $true
        $qualification.execution.managedFailureCount = 0
        foreach ($row in @($qualification.upstreamEvidence)) {
            $row.status = "pass"
            $row.reasonCode = $null
        }
        Write-Json -Path $qualificationPath -Value $qualification

        $signedRoot = Join-Path $bindingRoot "artifacts\signed"
        $packageRoot = Join-Path $signedRoot "package"
        New-Item -ItemType Directory -Path $packageRoot -Force | Out-Null
        $servicePath = Join-Path $packageRoot "restoreplane-service.exe"
        $cliPath = Join-Path $packageRoot "restoreplane.exe"
        [IO.File]::WriteAllText(
            $servicePath,
            "service fixture",
            [Text.UTF8Encoding]::new($false)
        )
        [IO.File]::WriteAllText(
            $cliPath,
            "cli fixture",
            [Text.UTF8Encoding]::new($false)
        )
        $providerPath = Join-Path $packageRoot "provider-certifications.json"
        [IO.File]::WriteAllText(
            $providerPath,
            '{"schemaVersion":1,"certifications":[]}',
            [Text.UTF8Encoding]::new($false)
        )
        $toolRoot = Join-Path $packageRoot "tools\postgresql"
        New-Item -ItemType Directory -Path $toolRoot -Force | Out-Null
        $toolPath = Join-Path $toolRoot "pg_dump.exe"
        [IO.File]::WriteAllText(
            $toolPath,
            "native tool fixture",
            [Text.UTF8Encoding]::new($false)
        )
        $auditPath = Join-Path $packageRoot "tools\native-tools-audit.json"
        [IO.File]::WriteAllText(
            $auditPath,
            '{"schemaVersion":1,"files":[{"path":"postgresql/pg_dump.exe"}]}',
            [Text.UTF8Encoding]::new($false)
        )
        $serviceSha256 = Get-FileSha256 -Path $servicePath
        $cliSha256 = Get-FileSha256 -Path $cliPath
        $providerSha256 = Get-FileSha256 -Path $providerPath
        $auditSha256 = Get-FileSha256 -Path $auditPath
        $toolSha256 = Get-FileSha256 -Path $toolPath
        $manifestPath = Join-Path $packageRoot "package-manifest.json"
        $signingPath = Join-Path $signedRoot "signing-metadata.json"
        $fixturePath = Join-Path $bindingRoot "artifacts\fixture\evidence.json"
        $functionalPath = Join-Path $bindingRoot `
            "artifacts\artifact-functional\evidence.json"
        $signer = "a" * 40
        $manifest = [ordered] @{
            schemaVersion = 1
            channel = "production"
            version = "0.1.0"
            minimumReaderVersion = "0.1.0"
            buildId = $qualification.buildId
            revision = $qualification.sourceRevision
            sourceTreeDigest = $qualification.sourceTreeDigest
            files = @(
                [ordered] @{
                    path = "restoreplane.exe"
                    sha256 = $cliSha256
                    sizeBytes = [long] (Get-Item -LiteralPath $cliPath).Length
                    authenticodeRequired = $true
                    signerRole = "release"
                    signerThumbprint = ""
                    publisher = "RestorePlane fixture"
                },
                [ordered] @{
                    path = "restoreplane-service.exe"
                    sha256 = $serviceSha256
                    sizeBytes = [long] (Get-Item -LiteralPath $servicePath).Length
                    authenticodeRequired = $true
                    signerRole = "release"
                    signerThumbprint = ""
                    publisher = "RestorePlane fixture"
                },
                [ordered] @{
                    path = "provider-certifications.json"
                    sha256 = $providerSha256
                    sizeBytes = [long] (Get-Item -LiteralPath $providerPath).Length
                    authenticodeRequired = $false
                    signerRole = "hash_only"
                    signerThumbprint = ""
                    publisher = "RestorePlane fixture"
                },
                [ordered] @{
                    path = "tools/native-tools-audit.json"
                    sha256 = $auditSha256
                    sizeBytes = [long] (Get-Item -LiteralPath $auditPath).Length
                    authenticodeRequired = $false
                    signerRole = "hash_only"
                    signerThumbprint = ""
                    publisher = "RestorePlane fixture"
                },
                [ordered] @{
                    path = "tools/postgresql/pg_dump.exe"
                    sha256 = $toolSha256
                    sizeBytes = [long] (Get-Item -LiteralPath $toolPath).Length
                    authenticodeRequired = $true
                    signerRole = "vendor"
                    signerThumbprint = "d" * 40
                    publisher = "RestorePlane fixture vendor"
                }
            )
            nativeTools = [ordered] @{
                auditManifestPath = "tools/native-tools-audit.json"
                auditManifestSha256 = $auditSha256
                fileCount = 1
            }
        }
        Write-Json -Path $manifestPath -Value $manifest
        $manifestSha256 = Get-FileSha256 -Path $manifestPath
        $artifactId = Get-TextSha256 -Value (
            "restoreplane-exact-artifact-v1`n" +
            "$manifestSha256`n$serviceSha256`n$cliSha256`n"
        )
        $signing = [ordered] @{
            schemaVersion = 1
            version = "0.1.0"
            sourceRevision = $qualification.sourceRevision
            sourceTreeDigest = $qualification.sourceTreeDigest
            buildId = $qualification.buildId
            artifactId = $artifactId
            signerThumbprint = $signer
            signerSubject = "CN=RestorePlane fixture"
            nativeToolAuditSha256 = $auditSha256
            timestampUrl = "https://timestamp.invalid"
            packageManifestSha256 = $manifestSha256
            serviceBinarySha256 = $serviceSha256
            cliBinarySha256 = $cliSha256
        }
        $fixture = [ordered] @{
            schemaVersion = 1
            status = "pass"
            trustContext = "same_artifact_lifecycle_fixture_not_production_trust"
            sourceRevision = $qualification.sourceRevision
            sourceTreeDigest = $qualification.sourceTreeDigest
            buildId = $qualification.buildId
            signerThumbprint = $signer
            deploymentScriptAuthenticode = "pass"
            cleanInstall = "pass"
            repair = "pass"
            update = "pass"
            supportBundle = "pass"
            uninstallPreservation = "pass"
            packageManifestSha256 = $manifestSha256
            serviceBinarySha256 = $serviceSha256
            cliBinarySha256 = $cliSha256
        }
        $functional = [ordered] @{
            schemaVersion = 1
            status = "pass"
            trustContext = "exact_signed_artifact_managed_functional_qualification"
            sourceRevision = $qualification.sourceRevision
            sourceTreeDigest = $qualification.sourceTreeDigest
            buildId = $qualification.buildId
            artifactId = $artifactId
            packageManifestSha256 = $manifestSha256
            serviceBinarySha256 = $serviceSha256
            cliBinarySha256 = $cliSha256
            installedServiceBinarySha256 = $serviceSha256
            installedCliBinarySha256 = $cliSha256
            executionAuthority = "service"
            installedCliIpc = "pass"
            fixtureIsolation = "managed_ephemeral"
            cleanup = "pass"
            engines = @("postgresql", "mysql", "mariadb") | ForEach-Object {
                [ordered] @{
                    engine = $_
                    scope = [ordered] @{
                        mode = "explicit_tables"
                        scopeIdentity = "$_.release_scope"
                        includedObjectIds = @("release_fixture.included_rows")
                        excludedObjectId = "release_fixture.excluded_rows"
                    }
                    backup = [ordered] @{
                        status = "pass"
                        workItemId = "11111111-1111-4111-8111-111111111111"
                        recoveryPointId = "22222222-2222-4222-8222-222222222222"
                        signedManifest = "pass"
                        requiredReader = "pass"
                        programOwnedWorker = "pass"
                    }
                    restore = [ordered] @{
                        status = "pass"
                        mode = "create_new"
                        restoreRunId = "33333333-3333-4333-8333-333333333333"
                        contentOracle = "pass"
                        selectedContentSha256 = "4" * 64
                        restoredContentSha256 = "4" * 64
                        excludedObjectAbsent = "pass"
                        programOwnedWorker = "pass"
                    }
                    verification = [ordered] @{
                        status = "pass"
                        level = 1
                        verificationRunId = "44444444-4444-4444-8444-444444444444"
                        programOwnedWorker = "pass"
                    }
                }
            }
        }
        Write-Json -Path $signingPath -Value $signing
        Write-Json -Path $fixturePath -Value $fixture
        Write-Json -Path $functionalPath -Value $functional

        $finalRoot = Join-Path $bindingRoot "artifacts\final"
        $finalRun = Invoke-G5Run `
            -CaseRoot $bindingRoot `
            -OutputRoot $finalRoot `
            -QualificationEvidencePath $qualificationPath `
            -SigningMetadataPath $signingPath `
            -SignedPackageManifestPath $manifestPath `
            -SignedFixtureEvidencePath $fixturePath `
            -ArtifactFunctionalEvidencePath $functionalPath `
            -RequireUpstreamEvidence `
            -RequirePromotionEvidence
        $finalPath = Join-Path $finalRoot "release-matrix.json"
        if ($finalRun.exitCode -ne 0 -or
            -not (Test-Path -LiteralPath $finalPath -PathType Leaf)) {
            Add-Failure "exact signed package reevaluation did not pass"
        }
        else {
            $final = Get-Content -LiteralPath $finalPath -Raw -Encoding UTF8 |
                ConvertFrom-Json
            if ($final.status -ne "pass" -or
                $final.mode -ne "promotion_reevaluation" -or
                $final.promotionStatus -ne "parked" -or
                $final.qualificationEvidence.status -ne "pass" -or
                $final.signedPackage.status -ne "pass" -or
                $final.signedPackage.manifestSha256 -ne $manifestSha256 -or
                $final.signedPackage.serviceBinarySha256 -ne $serviceSha256 -or
                $final.signedPackage.cliBinarySha256 -ne $cliSha256 -or
                $final.signedPackage.artifactId -ne $artifactId -or
                $final.artifactFunctionalQualification.status -ne "pass" -or
                $final.artifactFunctionalQualification.artifactId -ne $artifactId -or
                $final.execution.promotionEvidenceRequired -ne $true -or
                $final.execution.promotionEvidenceFailureCount -ne 0 -or
                @($final.externalPromotionRequirements |
                    Where-Object {
                        $_.id -ne "owner_approved_stop_metrics" -and
                        $_.status -ne "pass"
                    }).Count -ne 0) {
                Add-Failure "exact signed package reevaluation contract drifted"
            }
        }

        [IO.File]::AppendAllText(
            $servicePath,
            "tampered",
            [Text.UTF8Encoding]::new($false)
        )
        $tamperedRoot = Join-Path $bindingRoot "artifacts\tampered-package"
        $tamperedRun = Invoke-G5Run `
            -CaseRoot $bindingRoot `
            -OutputRoot $tamperedRoot `
            -QualificationEvidencePath $qualificationPath `
            -SigningMetadataPath $signingPath `
            -SignedPackageManifestPath $manifestPath `
            -SignedFixtureEvidencePath $fixturePath `
            -ArtifactFunctionalEvidencePath $functionalPath `
            -RequireUpstreamEvidence `
            -RequirePromotionEvidence
        $tamperedEvidencePath = Join-Path $tamperedRoot "release-matrix.json"
        if ($tamperedRun.exitCode -eq 0 -or
            -not (Test-Path -LiteralPath $tamperedEvidencePath)) {
            Add-Failure "signed package payload tampering was not rejected"
        }
        else {
            $tamperedEvidence = Get-Content $tamperedEvidencePath `
                -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($tamperedEvidence.status -ne "fail" -or
                $tamperedEvidence.signedPackage.reasonCode -ne
                    "SIGNED_PACKAGE_FILE_HASH_MISMATCH") {
                Add-Failure "package tampering did not emit a stable reason"
            }
        }
        [IO.File]::WriteAllText(
            $servicePath,
            "service fixture",
            [Text.UTF8Encoding]::new($false)
        )

        $mismatchedSigning = $signing | ConvertTo-Json -Depth 16 |
            ConvertFrom-Json
        $mismatchedSigning.buildId = "d" * 64
        $mismatchedSigningPath = Join-Path $signedRoot "mismatched-signing.json"
        Write-Json -Path $mismatchedSigningPath -Value $mismatchedSigning
        $mismatchSigningRoot = Join-Path $bindingRoot "artifacts\mismatch-signing"
        $mismatchSigningRun = Invoke-G5Run `
            -CaseRoot $bindingRoot `
            -OutputRoot $mismatchSigningRoot `
            -QualificationEvidencePath $qualificationPath `
            -SigningMetadataPath $mismatchedSigningPath `
            -SignedPackageManifestPath $manifestPath `
            -SignedFixtureEvidencePath $fixturePath `
            -ArtifactFunctionalEvidencePath $functionalPath `
            -RequireUpstreamEvidence `
            -RequirePromotionEvidence
        $mismatchSigningEvidencePath = Join-Path $mismatchSigningRoot `
            "release-matrix.json"
        if ($mismatchSigningRun.exitCode -eq 0 -or
            -not (Test-Path -LiteralPath $mismatchSigningEvidencePath)) {
            Add-Failure "same-revision different signed build was not rejected"
        }
        else {
            $mismatchEvidence = Get-Content $mismatchSigningEvidencePath `
                -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($mismatchEvidence.status -ne "fail" -or
                "SIGNED_PACKAGE_BUILD_MISMATCH" -notin
                    @($mismatchEvidence.externalPromotionRequirements.reasonCode)) {
                Add-Failure "signed build mismatch did not emit a stable reason"
            }
        }

        $digestMismatchedFixture = $fixture | ConvertTo-Json -Depth 16 |
            ConvertFrom-Json
        $digestMismatchedFixture.packageManifestSha256 = "f" * 64
        $digestMismatchedFixturePath = Join-Path $bindingRoot `
            "artifacts\fixture\digest-mismatched.json"
        Write-Json -Path $digestMismatchedFixturePath `
            -Value $digestMismatchedFixture
        $digestMismatchRoot = Join-Path $bindingRoot `
            "artifacts\digest-mismatch"
        $digestMismatchRun = Invoke-G5Run `
            -CaseRoot $bindingRoot `
            -OutputRoot $digestMismatchRoot `
            -QualificationEvidencePath $qualificationPath `
            -SigningMetadataPath $signingPath `
            -SignedPackageManifestPath $manifestPath `
            -SignedFixtureEvidencePath $digestMismatchedFixturePath `
            -ArtifactFunctionalEvidencePath $functionalPath `
            -RequireUpstreamEvidence `
            -RequirePromotionEvidence
        $digestMismatchEvidencePath = Join-Path $digestMismatchRoot `
            "release-matrix.json"
        if ($digestMismatchRun.exitCode -eq 0 -or
            -not (Test-Path -LiteralPath $digestMismatchEvidencePath)) {
            Add-Failure "fixture package digest mismatch was not rejected"
        }
        else {
            $digestMismatchEvidence = Get-Content $digestMismatchEvidencePath `
                -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($digestMismatchEvidence.status -ne "fail" -or
                "SIGNED_FIXTURE_PACKAGE_DIGEST_MISMATCH" -notin
                    @($digestMismatchEvidence.externalPromotionRequirements.reasonCode)) {
                Add-Failure "fixture package digest mismatch reason drifted"
            }
        }

        $mismatchedFixture = $fixture | ConvertTo-Json -Depth 16 |
            ConvertFrom-Json
        $mismatchedFixture.buildId = "e" * 64
        $mismatchedFixturePath = Join-Path $bindingRoot `
            "artifacts\fixture\mismatched.json"
        Write-Json -Path $mismatchedFixturePath -Value $mismatchedFixture
        $mismatchFixtureRoot = Join-Path $bindingRoot "artifacts\mismatch-fixture"
        $mismatchFixtureRun = Invoke-G5Run `
            -CaseRoot $bindingRoot `
            -OutputRoot $mismatchFixtureRoot `
            -QualificationEvidencePath $qualificationPath `
            -SigningMetadataPath $signingPath `
            -SignedPackageManifestPath $manifestPath `
            -SignedFixtureEvidencePath $mismatchedFixturePath `
            -ArtifactFunctionalEvidencePath $functionalPath `
            -RequireUpstreamEvidence `
            -RequirePromotionEvidence
        $mismatchFixtureEvidencePath = Join-Path $mismatchFixtureRoot `
            "release-matrix.json"
        if ($mismatchFixtureRun.exitCode -eq 0 -or
            -not (Test-Path -LiteralPath $mismatchFixtureEvidencePath)) {
            Add-Failure "same-revision different fixture build was not rejected"
        }
        else {
            $mismatchEvidence = Get-Content $mismatchFixtureEvidencePath `
                -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($mismatchEvidence.status -ne "fail" -or
                "SIGNED_FIXTURE_BUILD_MISMATCH" -notin
                    @($mismatchEvidence.externalPromotionRequirements.reasonCode)) {
                Add-Failure "fixture build mismatch did not emit a stable reason"
            }
        }

        $missingPackageRoot = Join-Path $bindingRoot "artifacts\missing-package"
        $missingPackageRun = Invoke-G5Run `
            -CaseRoot $bindingRoot `
            -OutputRoot $missingPackageRoot `
            -QualificationEvidencePath $qualificationPath `
            -SigningMetadataPath $signingPath `
            -SignedFixtureEvidencePath $fixturePath `
            -ArtifactFunctionalEvidencePath $functionalPath `
            -RequireUpstreamEvidence `
            -RequirePromotionEvidence
        $missingPackageEvidencePath = Join-Path $missingPackageRoot `
            "release-matrix.json"
        if ($missingPackageRun.exitCode -eq 0 -or
            -not (Test-Path -LiteralPath $missingPackageEvidencePath)) {
            Add-Failure "missing signed package evidence was not rejected"
        }
        else {
            $missingPackageEvidence = Get-Content $missingPackageEvidencePath `
                -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($missingPackageEvidence.status -ne "fail" -or
                "SIGNED_PACKAGE_EVIDENCE_MISSING_OR_INVALID" -notin
                    @($missingPackageEvidence.externalPromotionRequirements.reasonCode)) {
                Add-Failure "missing package did not emit a stable reason"
            }
        }

        $missingFixtureRoot = Join-Path $bindingRoot "artifacts\missing-fixture"
        $missingFixtureRun = Invoke-G5Run `
            -CaseRoot $bindingRoot `
            -OutputRoot $missingFixtureRoot `
            -QualificationEvidencePath $qualificationPath `
            -SigningMetadataPath $signingPath `
            -SignedPackageManifestPath $manifestPath `
            -ArtifactFunctionalEvidencePath $functionalPath `
            -RequireUpstreamEvidence `
            -RequirePromotionEvidence
        $missingFixtureEvidencePath = Join-Path $missingFixtureRoot `
            "release-matrix.json"
        if ($missingFixtureRun.exitCode -eq 0 -or
            -not (Test-Path -LiteralPath $missingFixtureEvidencePath)) {
            Add-Failure "missing signed fixture evidence was not rejected"
        }
        else {
            $missingEvidence = Get-Content $missingFixtureEvidencePath `
                -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($missingEvidence.status -ne "fail" -or
                "SIGNED_FIXTURE_EVIDENCE_MISSING_OR_INVALID" -notin
                    @($missingEvidence.externalPromotionRequirements.reasonCode)) {
                Add-Failure "missing fixture did not emit a stable reason"
            }
        }

        $missingFunctionalRoot = Join-Path $bindingRoot `
            "artifacts\missing-artifact-functional"
        $missingFunctionalRun = Invoke-G5Run `
            -CaseRoot $bindingRoot `
            -OutputRoot $missingFunctionalRoot `
            -QualificationEvidencePath $qualificationPath `
            -SigningMetadataPath $signingPath `
            -SignedPackageManifestPath $manifestPath `
            -SignedFixtureEvidencePath $fixturePath `
            -RequireUpstreamEvidence `
            -RequirePromotionEvidence
        $missingFunctionalEvidencePath = Join-Path $missingFunctionalRoot `
            "release-matrix.json"
        if ($missingFunctionalRun.exitCode -eq 0 -or
            -not (Test-Path -LiteralPath $missingFunctionalEvidencePath)) {
            Add-Failure "missing exact artifact functional evidence was not rejected"
        }
        else {
            $missingFunctionalEvidence = Get-Content $missingFunctionalEvidencePath `
                -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($missingFunctionalEvidence.status -ne "fail" -or
                "ARTIFACT_FUNCTIONAL_EVIDENCE_MISSING" -notin
                    @($missingFunctionalEvidence.externalPromotionRequirements.reasonCode)) {
                Add-Failure "missing artifact functional evidence reason drifted"
            }
        }
    }
}
finally {
    Remove-Item -LiteralPath $bindingRoot -Recurse -Force
}

$tokens = $null
$errors = $null
[Management.Automation.Language.Parser]::ParseFile(
    $scriptPath,
    [ref] $tokens,
    [ref] $errors
) | Out-Null
if (@($errors).Count -ne 0) {
    Add-Failure "G5 release matrix script has PowerShell parse errors"
}

if ($failures.Count -ne 0) {
    foreach ($failure in $failures) {
        [Console]::Error.WriteLine($failure)
    }
    exit 1
}
Write-Output "G5 release matrix contract tests passed"
exit 0
