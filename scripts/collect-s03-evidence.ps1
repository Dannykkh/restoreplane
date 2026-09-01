[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $OutputRoot,
    [string] $MatrixPath
)

$ErrorActionPreference = "Stop"
$collector = Join-Path $PSScriptRoot "collect-g1-evidence.ps1"
$hostPath = (Get-Process -Id $PID).Path
$arguments = @("-NoProfile", "-File", $collector, "-GateId", "S03")
if (-not [string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $arguments += @("-RepositoryRoot", $RepositoryRoot)
}
if (-not [string]::IsNullOrWhiteSpace($OutputRoot)) {
    $arguments += @("-OutputRoot", $OutputRoot)
}
if (-not [string]::IsNullOrWhiteSpace($MatrixPath)) {
    $arguments += @("-MatrixPath", $MatrixPath)
}

$previousErrorAction = $ErrorActionPreference
$ErrorActionPreference = "Continue"
try {
    & $hostPath @arguments
    $collectorExitCode = [int] $LASTEXITCODE
}
finally {
    $ErrorActionPreference = $previousErrorAction
}
if ($collectorExitCode -ne 0) {
    exit 1
}
exit 0
