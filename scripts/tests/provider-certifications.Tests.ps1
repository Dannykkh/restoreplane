$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)

$repoRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot "..\.."))
$generator = Join-Path $repoRoot "scripts\new-provider-certifications.ps1"
$manifestGenerator = Join-Path $repoRoot "scripts\new-package-manifest.ps1"
$schemaPath = Join-Path $repoRoot "release\provider-certifications.schema.json"
$fixtureRoot = Join-Path ([IO.Path]::GetTempPath()) (
    "restoreplane-provider-cert-tests-" + [Guid]::NewGuid().ToString("N")
)

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

function Invoke-ExpectedFailure {
    param(
        [Parameter(Mandatory = $true)] [scriptblock] $Action,
        [Parameter(Mandatory = $true)] [string] $Code
    )
    try {
        & $Action | Out-Null
    }
    catch {
        if ($_.Exception.Message -eq $Code) {
            return
        }
        throw "expected $Code, got $($_.Exception.Message)"
    }
    throw "expected failure code was not observed: $Code"
}

function New-Capabilities {
    param([Parameter(Mandatory = $true)] [string] $Engine)
    @(
        "probe", "discovery", "tool_preflight", "backup", "verify_l1",
        "verify_l2", "restore_create_new", "validation", "restore_l3"
    ) | ForEach-Object {
        [ordered] @{
            capability = $_
            outcome = "passed"
            evidenceRef = "artifacts/g2/$Engine.json#/$($_)"
            limitation = $null
        }
    }
}

function New-ExistingEvidence {
    param(
        [Parameter(Mandatory = $true)] [string] $Revision,
        [Parameter(Mandatory = $true)] [string] $Tree,
        [Parameter(Mandatory = $true)] [string] $Build
    )
    $engines = @(
        @{ engine = "postgresql"; server = "18.4"; tool = "pg_dump"; toolVersion = "18.4"; format = "custom" },
        @{ engine = "mysql"; server = "8.4.11"; tool = "mysqldump"; toolVersion = "8.4.11"; format = "sql" },
        @{ engine = "mariadb"; server = "11.8.5"; tool = "mariadb-dump"; toolVersion = "11.8.5"; format = "sql" }
    ) | ForEach-Object {
        [ordered] @{
            engine = $_.engine
            consistencyGrade = "transaction_consistent"
            server = [ordered] @{ version = $_.server }
            tools = @([ordered] @{ name = $_.tool; version = $_.toolVersion })
            backup = [ordered] @{ format = $_.format }
            capabilities = @(New-Capabilities -Engine $_.engine)
        }
    }
    [ordered] @{
        schemaVersion = 1
        gateId = "G2"
        sourceRevision = $Revision
        sourceTreeDigest = $Tree
        buildId = $Build
        status = "pass"
        engineCount = 3
        failureCount = 0
        engines = @($engines)
    }
}

function New-SelectiveEvidence {
    param(
        [Parameter(Mandatory = $true)] [string] $Revision,
        [Parameter(Mandatory = $true)] [string] $Tree,
        [Parameter(Mandatory = $true)] [string] $Build
    )
    [ordered] @{
        schemaVersion = 1
        gateId = "G2-selective-scope"
        sourceRevision = $Revision
        sourceTreeDigest = $Tree
        buildId = $Build
        failureCount = 0
        engines = @("postgresql", "mysql", "mariadb") | ForEach-Object {
            [ordered] @{
                engine = $_
                status = "pass"
                scope = [ordered] @{
                    mode = "explicit_tables"
                    selectedTables = @("fixture.selected_records")
                    excludedObjects = @("fixture.excluded_records")
                }
                dump = [ordered] @{ nativeProcessCount = 1 }
                oracle = [ordered] @{ matched = $true; excludedObjectsAbsent = $true }
            }
        }
    }
}

New-Item -ItemType Directory -Path $fixtureRoot -Force | Out-Null
try {
    if (-not (Test-Path -LiteralPath $generator -PathType Leaf) -or
        -not (Test-Path -LiteralPath $schemaPath -PathType Leaf)) {
        throw "provider certification production files are missing"
    }
    $revision = "a" * 40
    $tree = "b" * 64
    $build = "c" * 64
    $existingPath = Join-Path $fixtureRoot "existing-engines.json"
    $selectivePath = Join-Path $fixtureRoot "selective-scopes.json"
    $outputPath = Join-Path $fixtureRoot "provider-certifications.json"
    Write-Json -Path $existingPath -Value (
        New-ExistingEvidence -Revision $revision -Tree $tree -Build $build
    )
    $existingDocument = Get-Content -LiteralPath $existingPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    foreach ($engineEvidence in @($existingDocument.engines)) {
        Write-Json `
            -Path (Join-Path $fixtureRoot "$($engineEvidence.engine).json") `
            -Value ([ordered] @{
                schemaVersion = 1
                gateId = "G2"
                engine = $engineEvidence.engine
                supportStatus = "supported"
                sourceRevision = $revision
                sourceTreeDigest = $tree
                buildId = $build
            })
    }
    Write-Json -Path $selectivePath -Value (
        New-SelectiveEvidence -Revision $revision -Tree $tree -Build $build
    )

    $fakeBlake3 = Join-Path $fixtureRoot "fake-b3.cmd"
    [IO.File]::WriteAllText(
        $fakeBlake3,
        "@echo off`r`necho $("d" * 64)`r`n",
        [Text.ASCIIEncoding]::new()
    )
    & $generator `
        -ExistingEnginesEvidencePath $existingPath `
        -SelectiveScopeEvidencePath $selectivePath `
        -OutputPath $outputPath `
        -Blake3ExecutablePath $fakeBlake3 `
        -GeneratedAtMs 2000000000000 `
        -ValidityDays 90 | Out-Null

    $document = Get-Content -LiteralPath $outputPath -Raw -Encoding UTF8 |
        ConvertFrom-Json
    $expectedCapabilities = @(
        "probe", "discovery", "tool_preflight", "backup", "verify_l1",
        "verify_l2", "restore_create_new", "validation", "restore_l3"
    )
    if ($document.schemaVersion -ne 1 -or
        $document.sourceRevision -ne $revision -or
        $document.sourceTreeDigest -ne $tree -or
        $document.buildId -ne $build -or
        @($document.certifications).Count -ne 3) {
        throw "provider certification root identity is invalid"
    }
    if ((@($document.certifications.engine | Sort-Object) -join ",") -cne
        "mariadb,mysql,postgresql") {
        throw "provider certification engine set is not exact"
    }
    for ($index = 0; $index -lt @($document.certifications).Count; $index++) {
        $certification = $document.certifications[$index]
        $baseLocator = "package://provider-certifications.json#/certifications/$index"
        $expectedRange = switch ($certification.engine) {
            "postgresql" { ">=18,<19" }
            "mysql" { ">=8,<9" }
            "mariadb" { ">=11,<12" }
        }
        $expectedFormat = if ($certification.engine -eq "postgresql") {
            "custom"
        }
        else {
            "sql"
        }
        if ((@($certification.scopeModes | Sort-Object) -join ",") -cne
                "explicit_tables,whole_database" -or
            (@($certification.capabilities.capability | Sort-Object) -join ",") -cne
                ((@($expectedCapabilities | Sort-Object)) -join ",") -or
            @($certification.capabilities.capability | Select-Object -Unique).Count -ne 9 -or
            $certification.evidenceChecksumBlake3 -ne ("d" * 64) -or
            $certification.expiresAtMs -le $certification.certifiedAtMs -or
            $certification.serverVersionRange -ne $expectedRange -or
            $certification.toolVersionRange -ne $expectedRange -or
            $certification.dumpFormat -ne $expectedFormat -or
            $certification.evidenceLocator -ne $baseLocator -or
            @($certification.capabilities | Where-Object {
                    $_.evidenceLocator -ne (
                        "$baseLocator/capabilities/$($_.capability)"
                    )
                }).Count -ne 0) {
            throw "provider certification contract is incomplete"
        }
        $verifyL2 = @($certification.capabilities | Where-Object {
                $_.capability -eq "verify_l2"
            })
        if ($certification.engine -in @("mysql", "mariadb") -and
            ($verifyL2.Count -ne 1 -or
                $verifyL2[0].supportStatus -ne "limited" -or
                [string]::IsNullOrWhiteSpace($verifyL2[0].limitationText))) {
            throw "MySQL-family verify_l2 must be explicitly limited"
        }
    }

    $tampered = New-SelectiveEvidence `
        -Revision ("e" * 40) `
        -Tree $tree `
        -Build $build
    Write-Json -Path $selectivePath -Value $tampered
    Invoke-ExpectedFailure -Code "PROVIDER_CERTIFICATION_EVIDENCE_IDENTITY_MISMATCH" -Action {
        & $generator `
            -ExistingEnginesEvidencePath $existingPath `
            -SelectiveScopeEvidencePath $selectivePath `
            -OutputPath $outputPath `
            -Blake3ExecutablePath $fakeBlake3 `
            -GeneratedAtMs 2000000000000 `
            -ValidityDays 90
    }
    Write-Json -Path $selectivePath -Value (
        New-SelectiveEvidence -Revision $revision -Tree $tree -Build $build
    )

    $packageRoot = Join-Path $fixtureRoot "package"
    New-Item -ItemType Directory -Path (Join-Path $packageRoot "tools") -Force |
        Out-Null
    foreach ($file in @("restoreplane-service.exe", "restoreplane.exe")) {
        [IO.File]::WriteAllText(
            (Join-Path $packageRoot $file),
            "$file fixture",
            [Text.UTF8Encoding]::new($false)
        )
    }
    Copy-Item -LiteralPath $outputPath -Destination (
        Join-Path $packageRoot "provider-certifications.json"
    )
    $vendorPath = Join-Path $packageRoot "tools\vendor.txt"
    [IO.File]::WriteAllText($vendorPath, "vendor", [Text.UTF8Encoding]::new($false))
    $vendorSha = (Get-FileHash -LiteralPath $vendorPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Json -Path (Join-Path $packageRoot "tools\native-tools-audit.json") -Value (
        [ordered] @{
            schemaVersion = 1
            files = @([ordered] @{
                    path = "vendor.txt"
                    sha256 = $vendorSha
                    authenticodeRequired = $false
                    signerThumbprint = ""
                    publisher = "RestorePlane fixture"
                })
        }
    )
    $manifestPath = Join-Path $packageRoot "package-manifest.json"
    $manifest = (& $manifestGenerator `
        -PackageRoot $packageRoot `
        -OutputPath $manifestPath `
        -Version "0.1.0" `
        -MinimumReaderVersion "0.1.0" `
        -BuildId $build `
        -Revision $revision `
        -SourceTreeDigest $tree `
        -Publisher "RestorePlane fixture" `
        -Channel production) | ConvertFrom-Json
    $providerEntry = @($manifest.files | Where-Object {
            $_.path -eq "provider-certifications.json"
        })
    $providerFile = Get-Item -LiteralPath (
        Join-Path $packageRoot "provider-certifications.json"
    )
    if ($providerEntry.Count -ne 1 -or
        $providerEntry[0].sizeBytes -ne $providerFile.Length -or
        $providerEntry[0].sha256 -ne (
            Get-FileHash -LiteralPath $providerFile.FullName -Algorithm SHA256
        ).Hash.ToLowerInvariant()) {
        throw "package manifest did not exactly bind provider certifications"
    }
}
finally {
    if (Test-Path -LiteralPath $fixtureRoot) {
        Remove-Item -LiteralPath $fixtureRoot -Recurse -Force
    }
}

Write-Output "provider certification contract tests passed"
