$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$scriptPath = Join-Path $repoRoot "scripts\test-signed-artifact-functional.ps1"
$failures = [Collections.Generic.List[string]]::new()
if (-not (Test-Path -LiteralPath $scriptPath -PathType Leaf)) {
    $failures.Add("signed artifact functional harness is missing")
}
else {
    try {
        $contract = & $scriptPath -ValidateOnly | ConvertFrom-Json
        $expectedEngines = @("postgresql", "mysql", "mariadb")
        $expectedCommands = @(
            "service.health", "workspace.create", "connection.save",
            "connection.probe", "discovery.run", "discovery.result_list",
            "target.select", "backup_scope.discovery_run",
            "backup_scope.object_list", "backup_scope.save",
            "destination.save", "destination.probe", "plan.save", "plan.publish", "backup.enqueue",
            "work.get", "snapshot.list", "snapshot.get", "restore.preflight",
            "restore.enqueue_safe", "verification.enqueue"
        )
        if ($contract.schemaVersion -ne 1 -or
            $contract.mode -ne "exact_signed_artifact_functional_qualification" -or
            $contract.executionAuthority -ne "installed_service_ipc" -or
            $contract.backupRestoreAuthority -ne "program_owned_workers" -or
            $contract.fixtureIsolation -ne "managed_ephemeral" -or
            $contract.packageInstallRequired -ne $true -or
            $contract.installedBinaryDigestMatchRequired -ne $true -or
            $contract.signedManifestRequired -ne $true -or
            $contract.requiredReaderRequired -ne $true -or
            $contract.restoreMode -ne "create_new" -or
            $contract.verificationLevel -ne 1 -or
            $contract.cleanupRequired -ne $true -or
            ((@($contract.engines | Sort-Object) -join ",") -cne
                (($expectedEngines | Sort-Object) -join ",")) -or
            ((@($contract.ipcCommands | Sort-Object) -join ",") -cne
                (($expectedCommands | Sort-Object) -join ","))) {
            $failures.Add("signed artifact functional harness contract drifted")
        }
        if ($contract.databaseClientUse -ne "seed_and_content_oracle_only" -or
            $contract.nativeDumpRestoreSubstitutionAllowed -ne $false) {
            $failures.Add("native database clients can substitute for service workers")
        }
    }
    catch {
        $failures.Add("ValidateOnly failed: $($_.Exception.Message)")
    }

    $text = Get-Content -LiteralPath $scriptPath -Raw -Encoding UTF8
    foreach ($forbidden in @(
            "cargo run",
            "selective_scope_pipeline",
            "pg_dump",
            "mysqldump",
            "mariadb-dump",
            "pg_restore"
        )) {
        if ($text.IndexOf($forbidden, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
            $failures.Add("harness contains forbidden qualification substitute: $forbidden")
        }
    }
    foreach ($required in @(
            "service qualification-request",
            "scripts\install.ps1",
            "Get-FileHash",
            "signedManifest",
            "requiredReader",
            "restore.enqueue_safe",
            "verification.enqueue",
            "contentOracle",
            "finally",
            "Remove-RestorePlaneFixture"
        )) {
        if ($text.IndexOf($required, [StringComparison]::Ordinal) -lt 0) {
            $failures.Add("harness is missing exact-artifact behavior: $required")
        }
    }
}

if ($failures.Count -ne 0) {
    foreach ($failure in $failures) {
        [Console]::Error.WriteLine($failure)
    }
    exit 1
}
Write-Output "signed artifact functional harness contract tests passed"
exit 0
