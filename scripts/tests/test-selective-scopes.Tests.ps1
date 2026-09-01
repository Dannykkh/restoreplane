[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$scriptPath = Join-Path $repoRoot "scripts\test-selective-scopes.ps1"
$powershell = (Get-Process -Id $PID).Path
$previousErrorAction = $ErrorActionPreference
$ErrorActionPreference = "Continue"
try {
    $lines = @(& $powershell -NoProfile -File $scriptPath `
            -RepositoryRoot $repoRoot -ValidateOnly 2>&1)
    $exitCode = $LASTEXITCODE
}
finally {
    $ErrorActionPreference = $previousErrorAction
}
if ($exitCode -ne 0) {
    throw "selective-scope validate-only contract must exit 0: $($lines -join '`n')"
}

$contract = ($lines -join "`n") | ConvertFrom-Json
if ($contract.schemaVersion -ne 1 -or
    $contract.gateId -ne "G2-selective-scope" -or
    $contract.mode -ne "validate_only") {
    throw "selective-scope contract identity is invalid"
}
$engines = @($contract.engines)
if (($engines.engine -join ",") -ne "postgresql,mysql,mariadb") {
    throw "selective-scope engine set is incomplete"
}
foreach ($engine in $engines) {
    if ($engine.image -notmatch "@sha256:[0-9a-f]{64}$") {
        throw "selective-scope image is not digest pinned: $($engine.engine)"
    }
}
$pipeline = $contract.pipeline
if ($pipeline.compressionFormat -ne "zstd" -or
    $pipeline.encryptionFormat -ne "chacha20poly1305_stream" -or
    $pipeline.signedSetManifestRequired -ne $true -or
    $pipeline.dekWrapRoundTripRequired -ne $true -or
    $pipeline.requiredReplicaReaderRoundTrip -ne $true -or
    $pipeline.maxPlaintextBytes -ne 67108864 -or
    $pipeline.payloadExcludedFromEvidence -ne $true) {
    throw "selective-scope pipeline evidence contract is incomplete"
}

$tokens = $null
$errors = $null
[Management.Automation.Language.Parser]::ParseFile(
    $scriptPath,
    [ref] $tokens,
    [ref] $errors
) | Out-Null
if (@($errors).Count -ne 0) {
    throw "selective-scope script has PowerShell parse errors"
}

Write-Output "test-selective-scopes contract tests passed"
