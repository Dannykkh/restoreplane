[CmdletBinding()]
param(
    [string] $RepositoryRoot,
    [string] $OutputRoot,
    [string] $MatrixPath
)

$collector = Join-Path $PSScriptRoot "collect-g1-evidence.ps1"
$hostPath = (Get-Process -Id $PID).Path
$arguments = @("-NoProfile", "-File", $collector, "-GateId", "S05")
if (-not [string]::IsNullOrWhiteSpace($RepositoryRoot)) {
    $arguments += @("-RepositoryRoot", $RepositoryRoot)
}
if (-not [string]::IsNullOrWhiteSpace($OutputRoot)) {
    $arguments += @("-OutputRoot", $OutputRoot)
}
if (-not [string]::IsNullOrWhiteSpace($MatrixPath)) {
    $arguments += @("-MatrixPath", $MatrixPath)
}

& $hostPath @arguments
exit $LASTEXITCODE
