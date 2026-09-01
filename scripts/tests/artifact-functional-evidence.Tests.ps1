$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$root = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$validator = Join-Path $root "scripts\validate-artifact-functional-evidence.ps1"
$schemaPath = Join-Path $root "release\artifact-functional-evidence.schema.json"
$hostPath = (Get-Process -Id $PID).Path
$failures = [Collections.Generic.List[string]]::new()

function Add-Failure {
    param([Parameter(Mandatory = $true)] [string] $Message)
    $failures.Add($Message)
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

function Write-Json {
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

function Invoke-Validator {
    param(
        [Parameter(Mandatory = $true)] [string] $EvidencePath,
        [Parameter(Mandatory = $true)] [string] $ManifestSha256,
        [Parameter(Mandatory = $true)] [string] $ServiceSha256,
        [Parameter(Mandatory = $true)] [string] $CliSha256
    )

    $previousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "Continue"
    try {
        $lines = @(
            & $hostPath -NoProfile -File $validator `
                -EvidencePath $EvidencePath `
                -ExpectedPackageManifestSha256 $ManifestSha256 `
                -ExpectedServiceBinarySha256 $ServiceSha256 `
                -ExpectedCliBinarySha256 $CliSha256 2>&1
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

if (-not (Test-Path -LiteralPath $validator -PathType Leaf)) {
    Add-Failure "artifact functional evidence validator is missing"
}
if (-not (Test-Path -LiteralPath $schemaPath -PathType Leaf)) {
    Add-Failure "artifact functional evidence schema is missing"
}

if ($failures.Count -eq 0) {
    $schema = Get-Content -LiteralPath $schemaPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    $requiredIdentity = @(
        "artifactId",
        "packageManifestSha256",
        "serviceBinarySha256",
        "cliBinarySha256",
        "installedServiceBinarySha256",
        "installedCliBinarySha256",
        "engines"
    )
    if ($schema.title -ne
        "RestorePlane exact signed artifact functional qualification evidence" -or
        @($requiredIdentity | Where-Object { $_ -notin @($schema.required) }).Count -ne 0) {
        Add-Failure "artifact functional evidence schema identity contract drifted"
    }
    $requiredEngine = @("engine", "scope", "backup", "restore", "verification")
    if (@($requiredEngine |
            Where-Object { $_ -notin @($schema.'$defs'.engineEvidence.required) }).Count -ne 0) {
        Add-Failure "engine result schema is not fail-closed"
    }
}

$temporary = Join-Path ([IO.Path]::GetTempPath()) (
    "restoreplane-artifact-functional-" + [Guid]::NewGuid().ToString("N")
)
New-Item -ItemType Directory -Path $temporary -Force | Out-Null
try {
    if ($failures.Count -eq 0) {
        $manifestSha256 = "1" * 64
        $serviceSha256 = "2" * 64
        $cliSha256 = "3" * 64
        $artifactId = Get-TextSha256 -Value (
            "restoreplane-exact-artifact-v1`n" +
            "$manifestSha256`n$serviceSha256`n$cliSha256`n"
        )
        $engines = @("postgresql", "mysql", "mariadb") | ForEach-Object {
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
        $evidence = [ordered] @{
            schemaVersion = 1
            status = "pass"
            trustContext = "exact_signed_artifact_managed_functional_qualification"
            sourceRevision = "a" * 40
            sourceTreeDigest = "b" * 64
            buildId = "c" * 64
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
            engines = $engines
        }
        $validPath = Join-Path $temporary "valid.json"
        Write-Json -Path $validPath -Value $evidence
        $valid = Invoke-Validator -EvidencePath $validPath `
            -ManifestSha256 $manifestSha256 `
            -ServiceSha256 $serviceSha256 `
            -CliSha256 $cliSha256
        if ($valid.exitCode -ne 0 -or -not $valid.output.Contains($artifactId)) {
            Add-Failure "valid exact signed artifact evidence was rejected: $($valid.output)"
        }

        $missingMaria = $evidence | ConvertTo-Json -Depth 32 | ConvertFrom-Json
        $missingMaria.engines = @($missingMaria.engines | Where-Object { $_.engine -ne "mariadb" })
        $missingMariaPath = Join-Path $temporary "missing-mariadb.json"
        Write-Json -Path $missingMariaPath -Value $missingMaria
        $missingMariaRun = Invoke-Validator -EvidencePath $missingMariaPath `
            -ManifestSha256 $manifestSha256 `
            -ServiceSha256 $serviceSha256 `
            -CliSha256 $cliSha256
        if ($missingMariaRun.exitCode -eq 0 -or
            -not $missingMariaRun.output.Contains("ARTIFACT_FUNCTIONAL_ENGINE_SET_INVALID")) {
            Add-Failure "missing MariaDB qualification was not rejected"
        }

        $directNative = $evidence | ConvertTo-Json -Depth 32 | ConvertFrom-Json
        $directNative.engines[0].backup.programOwnedWorker = "direct_native_tool"
        $directNativePath = Join-Path $temporary "direct-native.json"
        Write-Json -Path $directNativePath -Value $directNative
        $directNativeRun = Invoke-Validator -EvidencePath $directNativePath `
            -ManifestSha256 $manifestSha256 `
            -ServiceSha256 $serviceSha256 `
            -CliSha256 $cliSha256
        if ($directNativeRun.exitCode -eq 0 -or
            -not $directNativeRun.output.Contains("ARTIFACT_FUNCTIONAL_WORKER_PATH_INVALID")) {
            Add-Failure "direct native-tool substitution was not rejected"
        }

        $artifactMismatch = $evidence | ConvertTo-Json -Depth 32 | ConvertFrom-Json
        $artifactMismatch.artifactId = "f" * 64
        $artifactMismatchPath = Join-Path $temporary "artifact-mismatch.json"
        Write-Json -Path $artifactMismatchPath -Value $artifactMismatch
        $artifactMismatchRun = Invoke-Validator -EvidencePath $artifactMismatchPath `
            -ManifestSha256 $manifestSha256 `
            -ServiceSha256 $serviceSha256 `
            -CliSha256 $cliSha256
        if ($artifactMismatchRun.exitCode -eq 0 -or
            -not $artifactMismatchRun.output.Contains("ARTIFACT_FUNCTIONAL_ARTIFACT_ID_MISMATCH")) {
            Add-Failure "artifactId mismatch was not rejected"
        }

        $verificationFailed = $evidence | ConvertTo-Json -Depth 32 | ConvertFrom-Json
        $verificationFailed.engines[1].verification.status = "fail"
        $verificationFailedPath = Join-Path $temporary "verification-failed.json"
        Write-Json -Path $verificationFailedPath -Value $verificationFailed
        $verificationFailedRun = Invoke-Validator -EvidencePath $verificationFailedPath `
            -ManifestSha256 $manifestSha256 `
            -ServiceSha256 $serviceSha256 `
            -CliSha256 $cliSha256
        if ($verificationFailedRun.exitCode -eq 0 -or
            -not $verificationFailedRun.output.Contains(
                "ARTIFACT_FUNCTIONAL_ENGINE_RESULT_INVALID"
            )) {
            Add-Failure "failed L1 verification was not rejected"
        }

        $oversizedPath = Join-Path $temporary "oversized.json"
        $oversizedStream = [IO.File]::Open(
            $oversizedPath,
            [IO.FileMode]::CreateNew,
            [IO.FileAccess]::Write,
            [IO.FileShare]::None
        )
        try {
            $oversizedStream.SetLength(4MB + 1)
        }
        finally {
            $oversizedStream.Dispose()
        }
        $oversizedRun = Invoke-Validator -EvidencePath $oversizedPath `
            -ManifestSha256 $manifestSha256 `
            -ServiceSha256 $serviceSha256 `
            -CliSha256 $cliSha256
        if ($oversizedRun.exitCode -eq 0 -or
            -not $oversizedRun.output.Contains(
                "ARTIFACT_FUNCTIONAL_EVIDENCE_MISSING"
            )) {
            Add-Failure "oversized functional evidence was not rejected before parsing"
        }
    }
}
finally {
    if (Test-Path -LiteralPath $temporary) {
        Remove-Item -LiteralPath $temporary -Recurse -Force
    }
}

if ($failures.Count -ne 0) {
    foreach ($failure in $failures) {
        [Console]::Error.WriteLine($failure)
    }
    exit 1
}
Write-Output "artifact functional evidence contract tests passed"
exit 0
