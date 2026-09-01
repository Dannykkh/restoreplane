[CmdletBinding()]
param(
    [string] $ReleaseRoot,
    [string] $OutputRoot,
    [string] $ExpectedSignerThumbprint,
    [string] $ExpectedArtifactId,
    [string] $ExpectedBuildId,
    [string] $ExpectedSourceTreeDigest,
    [string] $RunToken,
    [switch] $ValidateOnly
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$contract = [ordered] @{
    schemaVersion = 1
    mode = "exact_signed_artifact_functional_qualification"
    executionAuthority = "installed_service_ipc"
    backupRestoreAuthority = "program_owned_workers"
    fixtureIsolation = "managed_ephemeral"
    packageInstallRequired = $true
    installedBinaryDigestMatchRequired = $true
    signedManifestRequired = $true
    requiredReaderRequired = $true
    restoreMode = "create_new"
    verificationLevel = 1
    databaseClientUse = "seed_and_content_oracle_only"
    nativeDumpRestoreSubstitutionAllowed = $false
    cleanupRequired = $true
    engines = @("postgresql", "mysql", "mariadb")
    ipcCommands = @(
        "service.health", "workspace.create", "connection.save",
        "connection.probe", "discovery.run", "discovery.result_list",
        "target.select", "backup_scope.discovery_run",
        "backup_scope.object_list", "backup_scope.save",
        "destination.save", "destination.probe", "plan.save", "plan.publish",
        "backup.enqueue", "work.get", "snapshot.list", "snapshot.get",
        "restore.preflight", "restore.enqueue_safe", "verification.enqueue"
    )
}
if ($ValidateOnly) {
    $contract | ConvertTo-Json -Depth 8
    return
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

function Get-FileSha256 {
    param([Parameter(Mandatory = $true)] [string] $Path)
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Assert-RegularLeaf {
    param(
        [Parameter(Mandatory = $true)] [string] $Path,
        [Parameter(Mandatory = $true)] [string] $Code
    )
    $metadata = Get-Item -LiteralPath $Path -Force
    if ($metadata.PSIsContainer -or
        $metadata.Length -le 0 -or
        $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw $Code
    }
    return $metadata
}

function Write-JsonFile {
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

function Invoke-Docker {
    param([Parameter(Mandatory = $true)] [string[]] $Arguments)
    $lines = @(& docker @Arguments 2>&1)
    if ($LASTEXITCODE -ne 0) {
        $tail = @($lines | Select-Object -Last 12) -join "`n"
        throw "ARTIFACT_FUNCTIONAL_DOCKER_FAILED: $tail"
    }
    return @($lines)
}

function Invoke-EngineSql {
    param(
        [Parameter(Mandatory = $true)] [string] $Engine,
        [Parameter(Mandatory = $true)] [string] $Container,
        [Parameter(Mandatory = $true)] [string] $Password,
        [Parameter(Mandatory = $true)] [string] $Database,
        [Parameter(Mandatory = $true)] [string] $Sql
    )
    $lines = if ($Engine -eq "postgresql") {
        Invoke-Docker -Arguments @(
            "exec", "--env", "PGPASSWORD=$Password", $Container,
            "psql", "--username=source_admin", "--dbname=$Database",
            "--no-psqlrc", "--tuples-only", "--no-align", "--set=ON_ERROR_STOP=1",
            "--command=$Sql"
        )
    }
    else {
        Invoke-Docker -Arguments @(
            "exec", "--env", "MYSQL_PWD=$Password", $Container,
            "mysql", "--user=root", "--batch", "--raw", "--skip-column-names",
            "--database=$Database", "--execute=$Sql"
        )
    }
    return (@($lines | ForEach-Object { ([string] $_).TrimEnd() }) -join "`n").Trim()
}

function Wait-EngineReady {
    param(
        [Parameter(Mandatory = $true)] [object] $Fixture,
        [Parameter(Mandatory = $true)] [string] $Password
    )
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds(120)
    do {
        try {
            $value = Invoke-EngineSql `
                -Engine $Fixture.engine `
                -Container $Fixture.container `
                -Password $Password `
                -Database "source_db" `
                -Sql "SELECT 1;"
            if ($value -eq "1") {
                return
            }
        }
        catch {
            # The digest-pinned database image can reject connections during bootstrap.
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "ARTIFACT_FUNCTIONAL_DATABASE_NOT_READY"
}

function Invoke-InstalledIpc {
    param(
        [Parameter(Mandatory = $true)] [string] $Command,
        [Parameter(Mandatory = $true)] [object] $Payload,
        [switch] $Mutation
    )
    if ($Command -notin $contract.ipcCommands) {
        throw "ARTIFACT_FUNCTIONAL_IPC_COMMAND_DENIED"
    }
    $payloadPath = Join-Path $script:FixtureRoot (
        "ipc-" + [Guid]::NewGuid().ToString("N") + ".json"
    )
    Write-JsonFile -Path $payloadPath -Value $Payload
    try {
        # Exact invocation surface: service qualification-request.
        $arguments = @(
            "service", "qualification-request", "--command", $Command,
            "--payload-file", $payloadPath
        )
        if ($Mutation) {
            $arguments += @("--idempotency-key", [Guid]::NewGuid().ToString("D"))
        }
        $lines = @(& $script:InstalledCli @arguments 2>&1)
        if ($LASTEXITCODE -ne 0) {
            throw "ARTIFACT_FUNCTIONAL_INSTALLED_CLI_FAILED: " +
                (@($lines | Select-Object -Last 12) -join "`n")
        }
        try {
            $outcome = $lines -join "`n" | ConvertFrom-Json
        }
        catch {
            throw "ARTIFACT_FUNCTIONAL_INSTALLED_CLI_OUTPUT_INVALID"
        }
        if ($outcome.success -ne $true) {
            throw "ARTIFACT_FUNCTIONAL_IPC_FAILED: $($outcome.code)"
        }
        return $outcome.result
    }
    finally {
        if (Test-Path -LiteralPath $payloadPath -PathType Leaf) {
            Remove-Item -LiteralPath $payloadPath -Force
        }
    }
}

function Wait-WorkSucceeded {
    param(
        [Parameter(Mandatory = $true)] [string] $WorkspaceId,
        [Parameter(Mandatory = $true)] [string] $WorkItemId,
        [int] $TimeoutSeconds = 600
    )
    $deadline = [DateTimeOffset]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        $work = Invoke-InstalledIpc -Command "work.get" -Payload @{
            workspaceId = $WorkspaceId
            workItemId = $WorkItemId
        }
        if ($work.state -eq "succeeded") {
            return $work
        }
        if ($work.state -in @(
                "partially_succeeded", "failed", "failed_terminal", "cancelled"
            )) {
            throw "ARTIFACT_FUNCTIONAL_WORK_FAILED: " +
                "$($work.state):$($work.errorCode)"
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "ARTIFACT_FUNCTIONAL_WORK_TIMEOUT"
}

function Get-ContainerPort {
    param(
        [Parameter(Mandatory = $true)] [string] $Container,
        [Parameter(Mandatory = $true)] [int] $ContainerPort
    )
    $line = (Invoke-Docker -Arguments @(
            "port", $Container, "$ContainerPort/tcp"
        ) | Select-Object -First 1)
    $match = [regex]::Match([string] $line, ":([0-9]+)$")
    if (-not $match.Success) {
        throw "ARTIFACT_FUNCTIONAL_DOCKER_PORT_INVALID"
    }
    return [int] $match.Groups[1].Value
}

function Start-DatabaseFixture {
    param(
        [Parameter(Mandatory = $true)] [object] $Definition,
        [Parameter(Mandatory = $true)] [string] $Password
    )
    $arguments = @(
        "run", "--detach", "--name", $Definition.container,
        "--label", "dbbackup.artifact-functional=$RunToken",
        "--publish", "127.0.0.1::$($Definition.containerPort)"
    )
    if ($Definition.engine -eq "postgresql") {
        $arguments += @(
            "--env", "POSTGRES_USER=source_admin",
            "--env", "POSTGRES_PASSWORD=$Password",
            "--env", "POSTGRES_DB=source_db"
        )
    }
    elseif ($Definition.engine -eq "mysql") {
        $arguments += @(
            "--env", "MYSQL_ROOT_PASSWORD=$Password",
            "--env", "MYSQL_DATABASE=source_db"
        )
    }
    else {
        $arguments += @(
            "--env", "MARIADB_ROOT_PASSWORD=$Password",
            "--env", "MARIADB_DATABASE=source_db"
        )
    }
    $arguments += $Definition.image
    Invoke-Docker -Arguments $arguments | Out-Null
    $script:CreatedContainers.Add([string] $Definition.container)
    $Definition.hostPort = Get-ContainerPort `
        -Container $Definition.container `
        -ContainerPort $Definition.containerPort
    Wait-EngineReady -Fixture $Definition -Password $Password
}

function Invoke-ConnectionOperation {
    param(
        [Parameter(Mandatory = $true)] [string] $Command,
        [Parameter(Mandatory = $true)] [string] $WorkspaceId,
        [Parameter(Mandatory = $true)] [string] $ConnectionId
    )
    $operation = Invoke-InstalledIpc -Command $Command -Mutation -Payload @{
        workspaceId = $WorkspaceId
        connectionId = $ConnectionId
    }
    Wait-WorkSucceeded `
        -WorkspaceId $WorkspaceId `
        -WorkItemId ([string] $operation.workItemId) | Out-Null
    return $operation
}

function Get-ExactSnapshot {
    param(
        [Parameter(Mandatory = $true)] [string] $WorkspaceId,
        [Parameter(Mandatory = $true)] [string] $ConnectionId,
        [Parameter(Mandatory = $true)] [string] $Engine
    )
    $page = Invoke-InstalledIpc -Command "snapshot.list" -Payload @{
        workspaceId = $WorkspaceId
        cursor = $null
        limit = 100
        scope = "complete"
        connectionId = $ConnectionId
        engine = $Engine
        signatureStatus = "valid"
    }
    $items = @($page.items | Where-Object {
            $_.sourceConnectionId -eq $ConnectionId -and $_.engine -eq $Engine
        })
    if ($items.Count -ne 1 -or
        $items[0].state -ne "complete" -or
        $items[0].signatureStatus -ne "valid" -or
        $items[0].artifactCompleteness -ne "complete" -or
        $items[0].restoreEligibility -eq "blocked") {
        throw "ARTIFACT_FUNCTIONAL_SIGNED_SNAPSHOT_INVALID"
    }
    return $items[0]
}

function Get-SnapshotSection {
    param(
        [Parameter(Mandatory = $true)] [string] $WorkspaceId,
        [Parameter(Mandatory = $true)] [string] $SnapshotId,
        [Parameter(Mandatory = $true)] [string] $Section
    )
    $page = Invoke-InstalledIpc -Command "snapshot.get" -Payload @{
        workspaceId = $WorkspaceId
        snapshotId = $SnapshotId
        section = $Section
        cursor = $null
        limit = 100
    }
    if ($page.complete -ne $true -or $null -ne $page.nextCursor) {
        throw "ARTIFACT_FUNCTIONAL_SNAPSHOT_PAGE_INCOMPLETE"
    }
    return $page
}

function Invoke-EngineQualification {
    param(
        [Parameter(Mandatory = $true)] [object] $Fixture,
        [Parameter(Mandatory = $true)] [string] $Password,
        [Parameter(Mandatory = $true)] [string] $WorkspaceId
    )
    $seedSql = if ($Fixture.engine -eq "postgresql") {
        "CREATE TABLE public.selected_records(id integer PRIMARY KEY, value text NOT NULL); " +
        "INSERT INTO public.selected_records VALUES (1,'alpha'),(2,'beta'); " +
        "CREATE TABLE public.excluded_records(id integer PRIMARY KEY, value text NOT NULL); " +
        "INSERT INTO public.excluded_records VALUES (9,'excluded');"
    }
    else {
        "CREATE TABLE selected_records(id integer PRIMARY KEY, value varchar(64) NOT NULL); " +
        "INSERT INTO selected_records VALUES (1,'alpha'),(2,'beta'); " +
        "CREATE TABLE excluded_records(id integer PRIMARY KEY, value varchar(64) NOT NULL); " +
        "INSERT INTO excluded_records VALUES (9,'excluded');"
    }
    Invoke-EngineSql `
        -Engine $Fixture.engine `
        -Container $Fixture.container `
        -Password $Password `
        -Database "source_db" `
        -Sql $seedSql | Out-Null

    $sourceQuery = if ($Fixture.engine -eq "postgresql") {
        "SELECT id::text || ':' || value FROM public.selected_records ORDER BY id;"
    }
    else {
        "SELECT CONCAT(id, ':', value) FROM selected_records ORDER BY id;"
    }
    $sourceContent = Invoke-EngineSql `
        -Engine $Fixture.engine `
        -Container $Fixture.container `
        -Password $Password `
        -Database "source_db" `
        -Sql $sourceQuery
    $sourceContentSha256 = Get-TextSha256 -Value $sourceContent

    $connectionBase = @{
        workspaceId = $WorkspaceId
        engine = $Fixture.engine
        transport = "tcp"
        host = "127.0.0.1"
        port = $Fixture.hostPort
        filePath = ""
        tlsMode = "disabled"
        username = if ($Fixture.engine -eq "postgresql") { "source_admin" } else { "root" }
        password = $Password
    }
    $sourceConnection = Invoke-InstalledIpc `
        -Command "connection.save" `
        -Mutation `
        -Payload ($connectionBase + @{
                displayName = "$($Fixture.engine) qualification source"
                restoreTestTarget = $false
            })
    $targetConnection = Invoke-InstalledIpc `
        -Command "connection.save" `
        -Mutation `
        -Payload ($connectionBase + @{
                displayName = "$($Fixture.engine) qualification restore target"
                restoreTestTarget = $true
            })
    foreach ($connectionId in @(
            [string] $sourceConnection.connectionId,
            [string] $targetConnection.connectionId
        )) {
        Invoke-ConnectionOperation `
            -Command "connection.probe" `
            -WorkspaceId $WorkspaceId `
            -ConnectionId $connectionId | Out-Null
    }
    $sourceDiscovery = Invoke-ConnectionOperation `
        -Command "discovery.run" `
        -WorkspaceId $WorkspaceId `
        -ConnectionId ([string] $sourceConnection.connectionId)
    Invoke-ConnectionOperation `
        -Command "discovery.run" `
        -WorkspaceId $WorkspaceId `
        -ConnectionId ([string] $targetConnection.connectionId) | Out-Null
    $discovered = Invoke-InstalledIpc -Command "discovery.result_list" -Payload @{
        workspaceId = $WorkspaceId
        discoveryRunId = [string] $sourceDiscovery.discoveryRunId
        cursor = $null
        limit = 100
        eligibility = "eligible"
    }
    $database = @($discovered.items | Where-Object {
            $_.databaseName -eq "source_db" -and $_.eligibility -eq "eligible"
        })
    if ($database.Count -ne 1) {
        throw "ARTIFACT_FUNCTIONAL_SOURCE_DISCOVERY_INVALID"
    }
    $selection = Invoke-InstalledIpc `
        -Command "target.select" `
        -Mutation `
        -Payload @{
            workspaceId = $WorkspaceId
            connectionId = [string] $sourceConnection.connectionId
            discoveryRunId = [string] $sourceDiscovery.discoveryRunId
            engineObjectIds = @([string] $database[0].engineObjectId)
        }
    if (@($selection.targets).Count -ne 1) {
        throw "ARTIFACT_FUNCTIONAL_TARGET_SELECTION_INVALID"
    }
    $targetId = [string] $selection.targets[0].id

    $scopeDiscovery = Invoke-InstalledIpc `
        -Command "backup_scope.discovery_run" `
        -Mutation `
        -Payload @{
            workspaceId = $WorkspaceId
            databaseTargetId = $targetId
        }
    Wait-WorkSucceeded `
        -WorkspaceId $WorkspaceId `
        -WorkItemId ([string] $scopeDiscovery.workItemId) | Out-Null
    $objects = Invoke-InstalledIpc -Command "backup_scope.object_list" -Payload @{
        workspaceId = $WorkspaceId
        databaseTargetId = $targetId
        discoveryRunId = [string] $scopeDiscovery.discoveryRunId
        cursor = $null
        limit = 100
        eligibility = "eligible"
    }
    $selectedObject = @($objects.items | Where-Object {
            $_.objectName -eq "selected_records" -and $_.eligibility -eq "eligible"
        })
    $excludedObject = @($objects.items | Where-Object {
            $_.objectName -eq "excluded_records" -and $_.eligibility -eq "eligible"
        })
    if ($selectedObject.Count -ne 1 -or $excludedObject.Count -ne 1) {
        throw "ARTIFACT_FUNCTIONAL_SCOPE_INVENTORY_INVALID"
    }
    $scope = Invoke-InstalledIpc `
        -Command "backup_scope.save" `
        -Mutation `
        -Payload @{
            workspaceId = $WorkspaceId
            databaseTargetId = $targetId
            displayName = "$($Fixture.engine) exact selected table"
            mode = "explicit_tables"
            discoveryRunId = [string] $scopeDiscovery.discoveryRunId
            engineObjectIds = @([string] $selectedObject[0].engineObjectId)
        }

    $destinationPath = Join-Path $script:DataRoot (
        "qualification-destination\$($Fixture.engine)"
    )
    $destination = Invoke-InstalledIpc `
        -Command "destination.save" `
        -Mutation `
        -Payload @{
            workspaceId = $WorkspaceId
            displayName = "$($Fixture.engine) exact local destination"
            kind = "local"
            endpoint = @{ path = $destinationPath }
            credentials = @{}
        }
    $destinationProbe = Invoke-InstalledIpc `
        -Command "destination.probe" `
        -Mutation `
        -Payload @{
            workspaceId = $WorkspaceId
            destinationId = [string] $destination.destinationId
        }
    Wait-WorkSucceeded `
        -WorkspaceId $WorkspaceId `
        -WorkItemId ([string] $destinationProbe.workItemId) | Out-Null

    $plan = Invoke-InstalledIpc -Command "plan.save" -Mutation -Payload @{
        workspaceId = $WorkspaceId
        displayName = "$($Fixture.engine) exact artifact plan"
        scopeIds = @([string] $scope.id)
        destinations = @(@{
                destinationId = [string] $destination.destinationId
                requirement = "required"
            })
        schedule = @{
            kind = "cron"
            expression = "0 0 1 1 *"
            timezone = "UTC"
        }
        rpoMs = 86400000
        retention = @{ daily = 1; weekly = 0; monthly = 0 }
        verification = @{ requestedLevel = 1 }
    }
    Invoke-InstalledIpc -Command "plan.publish" -Mutation -Payload @{
        workspaceId = $WorkspaceId
        planId = [string] $plan.planId
        revisionId = [string] $plan.revisionId
    } | Out-Null
    $backup = Invoke-InstalledIpc -Command "backup.enqueue" -Mutation -Payload @{
        workspaceId = $WorkspaceId
        planId = [string] $plan.planId
    }
    if (@($backup.workItems).Count -ne 1 -or
        $backup.workItems[0].engine -ne $Fixture.engine -or
        $backup.workItems[0].targetId -ne $targetId) {
        throw "ARTIFACT_FUNCTIONAL_BACKUP_DISPATCH_INVALID"
    }
    $backupWorkId = [string] $backup.workItems[0].workItemId
    Wait-WorkSucceeded `
        -WorkspaceId $WorkspaceId `
        -WorkItemId $backupWorkId `
        -TimeoutSeconds 900 | Out-Null
    $snapshot = Get-ExactSnapshot `
        -WorkspaceId $WorkspaceId `
        -ConnectionId ([string] $sourceConnection.connectionId) `
        -Engine $Fixture.engine
    $replicas = Get-SnapshotSection `
        -WorkspaceId $WorkspaceId `
        -SnapshotId ([string] $snapshot.id) `
        -Section "replicas"
    $requiredReader = @($replicas.items | Where-Object {
            $_.requirement -eq "required" -and
            $_.state -eq "committed" -and
            $_.readerStatus -eq "eligible"
        })
    if ($requiredReader.Count -ne 1) {
        throw "ARTIFACT_FUNCTIONAL_REQUIRED_READER_INVALID"
    }

    $restoreTargetName = "dbbackup_restore_$($Fixture.engine)_$($RunToken.Replace('-', '_'))"
    $preflight = Invoke-InstalledIpc -Command "restore.preflight" -Payload @{
        workspaceId = $WorkspaceId
        snapshotId = [string] $snapshot.id
        mode = "create_new"
        purpose = "restore_drill"
        targetConnectionId = [string] $targetConnection.connectionId
        targetName = $restoreTargetName
        sourceReplicaId = [string] $requiredReader[0].id
    }
    if ($preflight.eligible -ne $true -or
        $preflight.mode -ne "create_new" -or
        $preflight.source.replicaId -ne $requiredReader[0].id -or
        $preflight.targetExists -ne $false) {
        throw "ARTIFACT_FUNCTIONAL_RESTORE_PREFLIGHT_INVALID"
    }
    $restore = Invoke-InstalledIpc -Command "restore.enqueue_safe" -Mutation -Payload @{
        workspaceId = $WorkspaceId
        snapshotId = [string] $snapshot.id
        sourceReplicaId = [string] $requiredReader[0].id
        targetConnectionId = [string] $targetConnection.connectionId
        targetName = $restoreTargetName
        preflightVersion = [string] $preflight.preflightVersion
        requiredVerificationLevel = 3
        purpose = "restore_drill"
    }
    Wait-WorkSucceeded `
        -WorkspaceId $WorkspaceId `
        -WorkItemId ([string] $restore.workItemId) `
        -TimeoutSeconds 900 | Out-Null
    $restoreDrills = Get-SnapshotSection `
        -WorkspaceId $WorkspaceId `
        -SnapshotId ([string] $snapshot.id) `
        -Section "restore_drills"
    $restoreEvidence = @($restoreDrills.items | Where-Object {
            $_.restoreRunId -eq $restore.restoreRunId
        })
    if ($restoreEvidence.Count -ne 1 -or
        $restoreEvidence[0].state -ne "succeeded" -or
        $restoreEvidence[0].evidenceAvailable -ne $true) {
        throw "ARTIFACT_FUNCTIONAL_RESTORE_EVIDENCE_INVALID"
    }

    $restoredContent = Invoke-EngineSql `
        -Engine $Fixture.engine `
        -Container $Fixture.container `
        -Password $Password `
        -Database $restoreTargetName `
        -Sql $sourceQuery
    $restoredContentSha256 = Get-TextSha256 -Value $restoredContent
    $excludedQuery = if ($Fixture.engine -eq "postgresql") {
        "SELECT CASE WHEN to_regclass('public.excluded_records') IS NULL THEN 0 ELSE 1 END;"
    }
    else {
        "SELECT COUNT(*) FROM information_schema.tables " +
        "WHERE table_schema = DATABASE() AND table_name = 'excluded_records';"
    }
    $excludedCount = Invoke-EngineSql `
        -Engine $Fixture.engine `
        -Container $Fixture.container `
        -Password $Password `
        -Database $restoreTargetName `
        -Sql $excludedQuery
    if ($sourceContentSha256 -cne $restoredContentSha256 -or $excludedCount -ne "0") {
        throw "ARTIFACT_FUNCTIONAL_CONTENT_ORACLE_MISMATCH"
    }

    $verification = Invoke-InstalledIpc `
        -Command "verification.enqueue" `
        -Mutation `
        -Payload @{
            workspaceId = $WorkspaceId
            snapshotId = [string] $snapshot.id
            sourceReplicaId = [string] $requiredReader[0].id
            level = 1
            purpose = "manual"
        }
    if ($verification.state -ne "queued" -or
        $verification.requestedLevel -ne 1 -or
        [string]::IsNullOrWhiteSpace([string] $verification.workItemId)) {
        throw "ARTIFACT_FUNCTIONAL_L1_DISPATCH_INVALID"
    }
    Wait-WorkSucceeded `
        -WorkspaceId $WorkspaceId `
        -WorkItemId ([string] $verification.workItemId) | Out-Null
    $verificationPage = Get-SnapshotSection `
        -WorkspaceId $WorkspaceId `
        -SnapshotId ([string] $snapshot.id) `
        -Section "verification_evidence"
    $verificationEvidence = @($verificationPage.items | Where-Object {
            $_.id -eq $verification.verificationRunId -and
            $_.level -eq 1 -and $_.result -eq "passed"
        })
    if ($verificationEvidence.Count -ne 1) {
        throw "ARTIFACT_FUNCTIONAL_L1_EVIDENCE_INVALID"
    }

    return [PSCustomObject] [ordered] @{
        engine = $Fixture.engine
        scope = [ordered] @{
            mode = "explicit_tables"
            scopeIdentity = [string] $scope.id
            includedObjectIds = @([string] $selectedObject[0].engineObjectId)
            excludedObjectId = [string] $excludedObject[0].engineObjectId
        }
        backup = [ordered] @{
            status = "pass"
            workItemId = $backupWorkId
            recoveryPointId = [string] $snapshot.id
            signedManifest = "pass"
            requiredReader = "pass"
            programOwnedWorker = "pass"
        }
        restore = [ordered] @{
            status = "pass"
            mode = "create_new"
            restoreRunId = [string] $restore.restoreRunId
            contentOracle = "pass"
            selectedContentSha256 = $sourceContentSha256
            restoredContentSha256 = $restoredContentSha256
            excludedObjectAbsent = "pass"
            programOwnedWorker = "pass"
        }
        verification = [ordered] @{
            status = "pass"
            level = 1
            verificationRunId = [string] $verification.verificationRunId
            programOwnedWorker = "pass"
        }
    }
}

function Remove-DbBackupFixture {
    [CmdletBinding()]
    param()
    $ok = $true
    try {
        $service = Get-Service -Name "DbBackupService" -ErrorAction SilentlyContinue
        if ($null -ne $service) {
            & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy AllSigned `
                -File (Join-Path $script:ResolvedReleaseRoot "scripts\uninstall.ps1") `
                -InstallRoot $script:InstallRoot `
                -DataRoot $script:DataRoot `
                -ServiceName "DbBackupService" `
                -ExpectedSignerThumbprint $script:NormalizedSigner | Out-Null
            if ($LASTEXITCODE -ne 0) {
                throw "ARTIFACT_FUNCTIONAL_UNINSTALL_FAILED"
            }
            $deadline = [DateTimeOffset]::UtcNow.AddSeconds(30)
            do {
                Start-Sleep -Milliseconds 250
                $service = Get-Service -Name "DbBackupService" -ErrorAction SilentlyContinue
            } while ($null -ne $service -and [DateTimeOffset]::UtcNow -lt $deadline)
            if ($null -ne $service) {
                $ok = $false
            }
        }
    }
    catch {
        $ok = $false
    }
    foreach ($container in @($script:CreatedContainers)) {
        try {
            & docker rm --force $container 2>&1 | Out-Null
            if ($LASTEXITCODE -ne 0) {
                $ok = $false
            }
        }
        catch {
            $ok = $false
        }
    }
    $fixturePrefix = $script:EphemeralBase.TrimEnd("\", "/") +
        [IO.Path]::DirectorySeparatorChar
    foreach ($path in @($script:InstallRoot, $script:DataRoot, $script:FixtureRoot)) {
        try {
            $resolved = [IO.Path]::GetFullPath($path)
            if (-not $resolved.StartsWith(
                    $fixturePrefix,
                    [StringComparison]::OrdinalIgnoreCase
                )) {
                $ok = $false
                continue
            }
            if (Test-Path -LiteralPath $resolved) {
                Remove-Item -LiteralPath $resolved -Recurse -Force
            }
        }
        catch {
            $ok = $false
        }
    }
    return $ok
}

foreach ($required in @(
        $ReleaseRoot,
        $OutputRoot,
        $ExpectedSignerThumbprint,
        $ExpectedArtifactId,
        $ExpectedBuildId,
        $ExpectedSourceTreeDigest,
        $RunToken
    )) {
    if ([string]::IsNullOrWhiteSpace($required)) {
        throw "ARTIFACT_FUNCTIONAL_INPUT_INVALID"
    }
}
if ($env:RUNNER_ENVIRONMENT -ne "self-hosted" -or
    $env:DBBACKUP_ARTIFACT_RUNNER_EPHEMERAL -ne "1" -or
    $RunToken -notmatch "^[0-9]+-[0-9]+$") {
    throw "ARTIFACT_FUNCTIONAL_EPHEMERAL_RUNNER_REQUIRED"
}
$principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent()
)
if (-not $principal.IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator
    )) {
    throw "ARTIFACT_FUNCTIONAL_ADMINISTRATOR_REQUIRED"
}
docker version | Out-Null
if ($LASTEXITCODE -ne 0) {
    throw "ARTIFACT_FUNCTIONAL_DOCKER_REQUIRED"
}

$script:ResolvedReleaseRoot = [IO.Path]::GetFullPath($ReleaseRoot)
$resolvedOutputRoot = [IO.Path]::GetFullPath($OutputRoot)
$script:NormalizedSigner = $ExpectedSignerThumbprint.Replace(" ", "").ToUpperInvariant()
if ($script:NormalizedSigner -notmatch "^[0-9A-F]{40}$" -or
    $ExpectedArtifactId -cnotmatch "^[0-9a-f]{64}$" -or
    $ExpectedBuildId -cnotmatch "^[0-9a-f]{64}$" -or
    $ExpectedSourceTreeDigest -cnotmatch "^[0-9a-f]{64}$") {
    throw "ARTIFACT_FUNCTIONAL_IDENTITY_INVALID"
}
$releaseMetadata = Get-Item -LiteralPath $script:ResolvedReleaseRoot -Force
if (-not $releaseMetadata.PSIsContainer -or
    $releaseMetadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
    throw "ARTIFACT_FUNCTIONAL_RELEASE_ROOT_INVALID"
}
$packageRoot = Join-Path $script:ResolvedReleaseRoot "package"
$manifestPath = Join-Path $packageRoot "package-manifest.json"
$signingPath = Join-Path $script:ResolvedReleaseRoot "signing-metadata.json"
Assert-RegularLeaf -Path $manifestPath -Code "ARTIFACT_FUNCTIONAL_MANIFEST_INVALID" |
    Out-Null
Assert-RegularLeaf -Path $signingPath -Code "ARTIFACT_FUNCTIONAL_SIGNING_INVALID" |
    Out-Null
$manifest = Get-Content -LiteralPath $manifestPath -Raw -Encoding UTF8 |
    ConvertFrom-Json
$signing = Get-Content -LiteralPath $signingPath -Raw -Encoding UTF8 |
    ConvertFrom-Json
if ($manifest.channel -ne "production" -or
    $manifest.revision -ne $env:GITHUB_SHA -or
    $manifest.sourceTreeDigest -ne $ExpectedSourceTreeDigest -or
    $manifest.buildId -ne $ExpectedBuildId -or
    $signing.sourceRevision -ne $manifest.revision -or
    $signing.sourceTreeDigest -ne $manifest.sourceTreeDigest -or
    $signing.buildId -ne $manifest.buildId -or
    $signing.signerThumbprint.Replace(" ", "").ToUpperInvariant() -ne
        $script:NormalizedSigner) {
    throw "ARTIFACT_FUNCTIONAL_SIGNED_IDENTITY_MISMATCH"
}
$entryByPath = @{}
foreach ($entry in @($manifest.files)) {
    $relative = [string] $entry.path
    if ($entryByPath.ContainsKey($relative) -or
        [IO.Path]::IsPathRooted($relative) -or
        $relative.Contains("..") -or
        $entry.sha256 -cnotmatch "^[0-9a-f]{64}$" -or
        [long] $entry.sizeBytes -le 0) {
        throw "ARTIFACT_FUNCTIONAL_MANIFEST_ENTRY_INVALID"
    }
    $path = [IO.Path]::GetFullPath((Join-Path $packageRoot $relative))
    $prefix = [IO.Path]::GetFullPath($packageRoot).TrimEnd("\", "/") +
        [IO.Path]::DirectorySeparatorChar
    if (-not $path.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "ARTIFACT_FUNCTIONAL_MANIFEST_ENTRY_INVALID"
    }
    $metadata = Assert-RegularLeaf `
        -Path $path `
        -Code "ARTIFACT_FUNCTIONAL_PACKAGE_FILE_INVALID"
    if ($metadata.Length -ne [long] $entry.sizeBytes -or
        (Get-FileSha256 -Path $path) -cne [string] $entry.sha256) {
        throw "ARTIFACT_FUNCTIONAL_PACKAGE_FILE_MISMATCH"
    }
    $entryByPath[$relative] = $entry
}
foreach ($requiredPath in @(
        "dbbackup-service.exe",
        "dbbackup.exe",
        "provider-certifications.json",
        "tools/native-tools-audit.json"
    )) {
    if (-not $entryByPath.ContainsKey($requiredPath)) {
        throw "ARTIFACT_FUNCTIONAL_PACKAGE_FILE_MISSING"
    }
}
$serviceEntry = $entryByPath["dbbackup-service.exe"]
$cliEntry = $entryByPath["dbbackup.exe"]
$providerEntry = $entryByPath["provider-certifications.json"]
$auditEntry = $entryByPath["tools/native-tools-audit.json"]
if ($serviceEntry.authenticodeRequired -ne $true -or
    $serviceEntry.signerRole -ne "release" -or
    $cliEntry.authenticodeRequired -ne $true -or
    $cliEntry.signerRole -ne "release" -or
    $providerEntry.authenticodeRequired -ne $false -or
    $providerEntry.signerRole -ne "hash_only" -or
    $auditEntry.authenticodeRequired -ne $false -or
    $auditEntry.signerRole -ne "hash_only") {
    throw "ARTIFACT_FUNCTIONAL_PACKAGE_TRUST_ROLE_INVALID"
}
$manifestSha256 = Get-FileSha256 -Path $manifestPath
$serviceSha256 = [string] $entryByPath["dbbackup-service.exe"].sha256
$cliSha256 = [string] $entryByPath["dbbackup.exe"].sha256
$artifactId = Get-TextSha256 -Value (
    "dbbackup-exact-artifact-v1`n" +
    "$manifestSha256`n$serviceSha256`n$cliSha256`n"
)
if ($artifactId -cne $ExpectedArtifactId -or
    $signing.artifactId -cne $artifactId -or
    $signing.packageManifestSha256 -cne $manifestSha256 -or
    $signing.serviceBinarySha256 -cne $serviceSha256 -or
    $signing.cliBinarySha256 -cne $cliSha256) {
    throw "ARTIFACT_FUNCTIONAL_ARTIFACT_ID_MISMATCH"
}
foreach ($binary in @("dbbackup-service.exe", "dbbackup.exe")) {
    $signature = Get-AuthenticodeSignature -LiteralPath (Join-Path $packageRoot $binary)
    if ($signature.Status.ToString() -ne "Valid" -or
        $null -eq $signature.SignerCertificate -or
        $signature.SignerCertificate.Thumbprint.Replace(" ", "").ToUpperInvariant() -ne
            $script:NormalizedSigner) {
        throw "ARTIFACT_FUNCTIONAL_AUTHENTICODE_INVALID"
    }
}
$deploymentFiles = @(
    (Join-Path $script:ResolvedReleaseRoot "scripts\install.ps1"),
    (Join-Path $script:ResolvedReleaseRoot "scripts\uninstall.ps1"),
    (Join-Path $script:ResolvedReleaseRoot "installer\DbBackup.Install.Common.psm1")
)
foreach ($deploymentFile in $deploymentFiles) {
    Assert-RegularLeaf `
        -Path $deploymentFile `
        -Code "ARTIFACT_FUNCTIONAL_DEPLOYMENT_FILE_INVALID" | Out-Null
    $signature = Get-AuthenticodeSignature -LiteralPath $deploymentFile
    if ($signature.Status.ToString() -ne "Valid" -or
        $null -eq $signature.SignerCertificate -or
        $signature.SignerCertificate.Thumbprint.Replace(" ", "").ToUpperInvariant() -ne
            $script:NormalizedSigner) {
        throw "ARTIFACT_FUNCTIONAL_DEPLOYMENT_AUTHENTICODE_INVALID"
    }
}

$script:EphemeralBase = [IO.Path]::GetFullPath((Join-Path (
            [IO.Path]::GetFullPath($env:RUNNER_TEMP)
        ) "DbBackupArtifactQualification"))
$script:FixtureRoot = Join-Path $script:EphemeralBase $RunToken
$script:InstallRoot = Join-Path $script:FixtureRoot "install"
$script:DataRoot = Join-Path $script:FixtureRoot "data"
$script:InstalledCli = Join-Path $script:InstallRoot "dbbackup.exe"
$script:CreatedContainers = [Collections.Generic.List[string]]::new()
if ($null -ne (Get-Service -Name "DbBackupService" -ErrorAction SilentlyContinue) -or
    (Test-Path -LiteralPath $script:FixtureRoot)) {
    throw "ARTIFACT_FUNCTIONAL_RUNNER_NOT_PRISTINE"
}
New-Item -ItemType Directory -Path $script:FixtureRoot -Force | Out-Null

$shortToken = $RunToken.Replace("-", "")
$fixtures = @(
    [PSCustomObject] @{
        engine = "postgresql"
        image = "postgres@sha256:3a82e1f56c8f0f5616a11103ac3d47e632c3938698946a7ad26da0df1334744a"
        containerPort = 5432
        container = "dbbackup-artifact-pg-$shortToken"
        hostPort = 0
    },
    [PSCustomObject] @{
        engine = "mysql"
        image = "mysql@sha256:b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb"
        containerPort = 3306
        container = "dbbackup-artifact-mysql-$shortToken"
        hostPort = 0
    },
    [PSCustomObject] @{
        engine = "mariadb"
        image = "mariadb@sha256:a794d9eb009e20de605858a11f32f63b4075cbd197c650436f0e3b457e4caed7"
        containerPort = 3306
        container = "dbbackup-artifact-maria-$shortToken"
        hostPort = 0
    }
)
$passwords = @{}
$engineResults = [Collections.Generic.List[object]]::new()
$qualificationCompleted = $false
$cleanupSucceeded = $false
try {
    & powershell.exe -NoProfile -NonInteractive -ExecutionPolicy AllSigned `
        -File (Join-Path $script:ResolvedReleaseRoot "scripts\install.ps1") `
        -PackageRoot $packageRoot `
        -ManifestPath $manifestPath `
        -InstallRoot $script:InstallRoot `
        -DataRoot $script:DataRoot `
        -ServiceName "DbBackupService" `
        -ExpectedPackageManifestSha256 $signing.packageManifestSha256 `
        -ExpectedSignerThumbprint $script:NormalizedSigner `
        -ExpectedNativeToolAuditSha256 $signing.nativeToolAuditSha256 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "ARTIFACT_FUNCTIONAL_INSTALL_FAILED"
    }
    $installedServiceSha256 = Get-FileSha256 -Path (
        Join-Path $script:InstallRoot "dbbackup-service.exe"
    )
    $installedCliSha256 = Get-FileSha256 -Path $script:InstalledCli
    if ($installedServiceSha256 -cne $serviceSha256 -or
        $installedCliSha256 -cne $cliSha256) {
        throw "ARTIFACT_FUNCTIONAL_INSTALLED_BINARY_MISMATCH"
    }
    $health = Invoke-InstalledIpc -Command "service.health" -Payload @{}
    if ($health.phase -ne "production_ready" -or
        $health.executionAuthority -ne "service" -or
        $health.claimsEnabled -ne $true) {
        throw "ARTIFACT_FUNCTIONAL_SERVICE_AUTHORITY_INVALID"
    }
    $workspace = Invoke-InstalledIpc `
        -Command "workspace.create" `
        -Mutation `
        -Payload @{ displayName = "Exact artifact $RunToken" }
    $workspaceId = [string] $workspace.id
    if ($workspaceId -notmatch "^[0-9a-f-]{36}$") {
        throw "ARTIFACT_FUNCTIONAL_WORKSPACE_INVALID"
    }
    foreach ($fixture in $fixtures) {
        $passwordBytes = New-Object byte[] 24
        $random = [Security.Cryptography.RandomNumberGenerator]::Create()
        try {
            $random.GetBytes($passwordBytes)
        }
        finally {
            $random.Dispose()
        }
        $password = [Convert]::ToBase64String($passwordBytes).
            Replace("/", "a").Replace("+", "b").TrimEnd("=")
        $passwords[$fixture.engine] = $password
        Start-DatabaseFixture -Definition $fixture -Password $password
        $engineResults.Add((Invoke-EngineQualification `
                    -Fixture $fixture `
                    -Password $password `
                    -WorkspaceId $workspaceId))
    }
    $qualificationCompleted = $true
}
finally {
    $cleanupSucceeded = Remove-DbBackupFixture
}

if (-not $qualificationCompleted -or -not $cleanupSucceeded) {
    throw "ARTIFACT_FUNCTIONAL_QUALIFICATION_OR_CLEANUP_FAILED"
}
$evidence = [ordered] @{
    schemaVersion = 1
    status = "pass"
    trustContext = "exact_signed_artifact_managed_functional_qualification"
    sourceRevision = [string] $manifest.revision
    sourceTreeDigest = [string] $manifest.sourceTreeDigest
    buildId = [string] $manifest.buildId
    artifactId = $artifactId
    packageManifestSha256 = $manifestSha256
    serviceBinarySha256 = $serviceSha256
    cliBinarySha256 = $cliSha256
    installedServiceBinarySha256 = $installedServiceSha256
    installedCliBinarySha256 = $installedCliSha256
    executionAuthority = "service"
    installedCliIpc = "pass"
    fixtureIsolation = "managed_ephemeral"
    cleanup = "pass"
    engines = @($engineResults)
}
New-Item -ItemType Directory -Path $resolvedOutputRoot -Force | Out-Null
$temporaryEvidence = Join-Path $resolvedOutputRoot (
    ".evidence-" + [Guid]::NewGuid().ToString("N") + ".partial"
)
$finalEvidence = Join-Path $resolvedOutputRoot "evidence.json"
try {
    Write-JsonFile -Path $temporaryEvidence -Value $evidence
    & (Join-Path $PSScriptRoot "validate-artifact-functional-evidence.ps1") `
        -EvidencePath $temporaryEvidence `
        -ExpectedPackageManifestSha256 $manifestSha256 `
        -ExpectedServiceBinarySha256 $serviceSha256 `
        -ExpectedCliBinarySha256 $cliSha256 | Out-Null
    Move-Item -LiteralPath $temporaryEvidence -Destination $finalEvidence
}
finally {
    if (Test-Path -LiteralPath $temporaryEvidence -PathType Leaf) {
        Remove-Item -LiteralPath $temporaryEvidence -Force
    }
}
$evidence | ConvertTo-Json -Depth 32
