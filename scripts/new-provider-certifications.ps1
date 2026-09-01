[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)] [string] $ExistingEnginesEvidencePath,
    [Parameter(Mandatory = $true)] [string] $SelectiveScopeEvidencePath,
    [Parameter(Mandatory = $true)] [string] $OutputPath,
    [Parameter(Mandatory = $true)] [string] $Blake3ExecutablePath,
    [long] $GeneratedAtMs = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds(),
    [ValidateRange(1, 365)] [int] $ValidityDays = 90
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
Import-Module (
    Join-Path (Split-Path -Parent $PSScriptRoot) "installer\RestorePlane.Install.Common.psm1"
) -Force

$expectedEngines = @("postgresql", "mysql", "mariadb")
$expectedCapabilities = @(
    "probe", "discovery", "tool_preflight", "backup", "verify_l1",
    "verify_l2", "restore_create_new", "validation", "restore_l3"
)
$primaryTools = @{
    postgresql = "pg_dump"
    mysql = "mysqldump"
    mariadb = "mariadb-dump"
}
$versionRanges = @{
    postgresql = ">=18,<19"
    mysql = ">=8,<9"
    mariadb = ">=11,<12"
}
$dumpFormats = @{
    postgresql = "custom"
    mysql = "sql"
    mariadb = "sql"
}

function Get-RequiredProperty {
    param(
        [Parameter(Mandatory = $true)] [object] $Object,
        [Parameter(Mandatory = $true)] [string] $Name
    )
    $property = $Object.PSObject.Properties[$Name]
    if ($null -eq $property) {
        throw "PROVIDER_CERTIFICATION_EVIDENCE_INVALID"
    }
    return $property.Value
}

function Read-BoundedJsonEvidence {
    param([Parameter(Mandatory = $true)] [string] $Path)
    try {
        $resolved = [IO.Path]::GetFullPath($Path)
        $metadata = Get-Item -LiteralPath $resolved -Force
        if ($metadata.PSIsContainer -or
            $metadata.Length -le 0 -or
            $metadata.Length -gt 16MB -or
            $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw "invalid"
        }
        return Get-Content -LiteralPath $resolved -Raw -Encoding UTF8 |
            ConvertFrom-Json
    }
    catch {
        throw "PROVIDER_CERTIFICATION_EVIDENCE_INVALID"
    }
}

function Assert-ExactSet {
    param(
        [Parameter(Mandatory = $true)] [object[]] $Actual,
        [Parameter(Mandatory = $true)] [string[]] $Expected,
        [Parameter(Mandatory = $true)] [string] $ErrorCode
    )
    $seen = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::Ordinal
    )
    foreach ($value in @($Actual)) {
        if ($null -eq $value -or -not $seen.Add([string] $value)) {
            throw $ErrorCode
        }
    }
    if ($seen.Count -ne $Expected.Count) {
        throw $ErrorCode
    }
    foreach ($expected in $Expected) {
        if (-not $seen.Contains($expected)) {
            throw $ErrorCode
        }
    }
}

function Get-ExactEngineMap {
    param([Parameter(Mandatory = $true)] [object] $Evidence)
    $engines = @(Get-RequiredProperty -Object $Evidence -Name "engines")
    Assert-ExactSet `
        -Actual @($engines | ForEach-Object {
                [string] (Get-RequiredProperty -Object $_ -Name "engine")
            }) `
        -Expected $expectedEngines `
        -ErrorCode "PROVIDER_CERTIFICATION_ENGINE_SET_INVALID"
    $map = @{}
    foreach ($engine in $engines) {
        $map[[string] (Get-RequiredProperty -Object $engine -Name "engine")] = $engine
    }
    return $map
}

function Get-Blake3 {
    param(
        [Parameter(Mandatory = $true)] [string] $ExecutablePath,
        [Parameter(Mandatory = $true)] [string] $EvidencePath
    )
    $resolvedExecutable = [IO.Path]::GetFullPath($ExecutablePath)
    $metadata = Get-Item -LiteralPath $resolvedExecutable -Force
    if ($metadata.PSIsContainer -or
        $metadata.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw "PROVIDER_CERTIFICATION_BLAKE3_TOOL_INVALID"
    }
    $lines = @(& $resolvedExecutable ([IO.Path]::GetFullPath($EvidencePath)) 2>&1)
    if ($LASTEXITCODE -ne 0 -or $lines.Count -ne 1) {
        throw "PROVIDER_CERTIFICATION_BLAKE3_FAILED"
    }
    $digest = ([string] $lines[0]).Trim()
    if ($digest -cnotmatch "^[0-9a-f]{64}$") {
        throw "PROVIDER_CERTIFICATION_BLAKE3_FAILED"
    }
    return $digest
}

function Test-RelativeEvidenceLocator {
    param([Parameter(Mandatory = $true)] [string] $Locator)
    return -not [string]::IsNullOrWhiteSpace($Locator) -and
        $Locator.Length -le 512 -and
        -not [IO.Path]::IsPathRooted(($Locator -split "#", 2)[0]) -and
        -not $Locator.Contains("\") -and
        -not (($Locator -split "#", 2)[0] -split "/").Contains("..") -and
        $Locator -cmatch "^[A-Za-z0-9._/-]+(?:#[A-Za-z0-9._/~:-]+)?$"
}

$existing = Read-BoundedJsonEvidence -Path $ExistingEnginesEvidencePath
$selective = Read-BoundedJsonEvidence -Path $SelectiveScopeEvidencePath
$existingRevision = [string] (Get-RequiredProperty -Object $existing -Name "sourceRevision")
$existingTree = [string] (Get-RequiredProperty -Object $existing -Name "sourceTreeDigest")
$existingBuild = [string] (Get-RequiredProperty -Object $existing -Name "buildId")
$selectiveRevision = [string] (Get-RequiredProperty -Object $selective -Name "sourceRevision")
$selectiveTree = [string] (Get-RequiredProperty -Object $selective -Name "sourceTreeDigest")
$selectiveBuild = [string] (Get-RequiredProperty -Object $selective -Name "buildId")
if ($existingRevision -cnotmatch "^[0-9a-f]{40,64}$" -or
    $existingTree -cnotmatch "^[0-9a-f]{64}$" -or
    $existingBuild -cnotmatch "^[0-9a-f]{64}$" -or
    $existingRevision -cne $selectiveRevision -or
    $existingTree -cne $selectiveTree -or
    $existingBuild -cne $selectiveBuild) {
    throw "PROVIDER_CERTIFICATION_EVIDENCE_IDENTITY_MISMATCH"
}
if ((Get-RequiredProperty -Object $existing -Name "schemaVersion") -ne 1 -or
    (Get-RequiredProperty -Object $existing -Name "gateId") -cne "G2" -or
    (Get-RequiredProperty -Object $existing -Name "status") -cne "pass" -or
    [int] (Get-RequiredProperty -Object $existing -Name "failureCount") -ne 0 -or
    (Get-RequiredProperty -Object $selective -Name "schemaVersion") -ne 1 -or
    (Get-RequiredProperty -Object $selective -Name "gateId") -cne "G2-selective-scope" -or
    [int] (Get-RequiredProperty -Object $selective -Name "failureCount") -ne 0) {
    throw "PROVIDER_CERTIFICATION_EVIDENCE_INVALID"
}
if ($GeneratedAtMs -le 0) {
    throw "PROVIDER_CERTIFICATION_TIME_INVALID"
}
$expiresAtMs = $GeneratedAtMs + ([long] $ValidityDays * 24L * 60L * 60L * 1000L)
if ($expiresAtMs -le $GeneratedAtMs) {
    throw "PROVIDER_CERTIFICATION_TIME_INVALID"
}

$existingByEngine = Get-ExactEngineMap -Evidence $existing
$selectiveByEngine = Get-ExactEngineMap -Evidence $selective
$certifications = foreach ($engineName in $expectedEngines) {
    $engine = $existingByEngine[$engineName]
    $selectiveEngine = $selectiveByEngine[$engineName]
    $scope = Get-RequiredProperty -Object $selectiveEngine -Name "scope"
    $dump = Get-RequiredProperty -Object $selectiveEngine -Name "dump"
    if ((Get-RequiredProperty -Object $selectiveEngine -Name "status") -cne "pass" -or
        (Get-RequiredProperty -Object $scope -Name "mode") -cne "explicit_tables" -or
        @(Get-RequiredProperty -Object $scope -Name "selectedTables").Count -eq 0 -or
        @(Get-RequiredProperty -Object $scope -Name "excludedObjects").Count -eq 0 -or
        [int] (Get-RequiredProperty -Object $dump -Name "nativeProcessCount") -ne 1) {
        throw "PROVIDER_CERTIFICATION_SELECTIVE_SCOPE_INVALID"
    }

    $capabilityEvidence = @(Get-RequiredProperty -Object $engine -Name "capabilities")
    Assert-ExactSet `
        -Actual @($capabilityEvidence | ForEach-Object {
                [string] (Get-RequiredProperty -Object $_ -Name "capability")
            }) `
        -Expected $expectedCapabilities `
        -ErrorCode "PROVIDER_CERTIFICATION_CAPABILITY_SET_INVALID"
    $capabilities = foreach ($capabilityName in $expectedCapabilities) {
        $sourceCapability = @($capabilityEvidence | Where-Object {
                [string]::Equals(
                    [string] (Get-RequiredProperty -Object $_ -Name "capability"),
                    $capabilityName,
                    [StringComparison]::Ordinal
                )
            })[0]
        $sourceOutcome = [string] (
            Get-RequiredProperty -Object $sourceCapability -Name "outcome"
        )
        $sourceLocator = [string] (
            Get-RequiredProperty -Object $sourceCapability -Name "evidenceRef"
        )
        if (-not (Test-RelativeEvidenceLocator -Locator $sourceLocator)) {
            throw "PROVIDER_CERTIFICATION_EVIDENCE_LOCATOR_INVALID"
        }
        $forcedMySqlFamilyL2 = $engineName -in @("mysql", "mariadb") -and
            $capabilityName -eq "verify_l2"
        $supportStatus = if ($forcedMySqlFamilyL2) {
            "limited"
        }
        elseif ($sourceOutcome -ceq "passed") {
            "passed"
        }
        else {
            "limited"
        }
        $certificationIndex = [Array]::IndexOf($expectedEngines, $engineName)
        $baseLocator = "package://provider-certifications.json#/certifications/$certificationIndex"
        $entry = [ordered] @{
            capability = $capabilityName
            supportStatus = $supportStatus
            evidenceLocator = "$baseLocator/capabilities/$capabilityName"
        }
        if ($supportStatus -eq "limited") {
            $sourceLimitationProperty = $sourceCapability.PSObject.Properties["limitation"]
            $limitation = if ($forcedMySqlFamilyL2) {
                "Native public verification parser is not yet authoritative for this engine family."
            }
            elseif ($null -ne $sourceLimitationProperty -and
                -not [string]::IsNullOrWhiteSpace([string] $sourceLimitationProperty.Value)) {
                [string] $sourceLimitationProperty.Value
            }
            else {
                "Managed G2 evidence did not pass this capability."
            }
            $entry.limitationText = $limitation
        }
        [PSCustomObject] $entry
    }

    $tools = @(Get-RequiredProperty -Object $engine -Name "tools")
    $primaryToolName = [string] $primaryTools[$engineName]
    $primaryTool = @($tools | Where-Object {
            [string]::Equals(
                [string] (Get-RequiredProperty -Object $_ -Name "name"),
                $primaryToolName,
                [StringComparison]::Ordinal
            )
        })
    if ($primaryTool.Count -ne 1) {
        throw "PROVIDER_CERTIFICATION_TOOL_INVALID"
    }
    $engineIndex = [Array]::IndexOf($expectedEngines, $engineName)
    $rootLocator = "package://provider-certifications.json#/certifications/$engineIndex"
    $engineEvidencePath = Join-Path (
        Split-Path -Parent ([IO.Path]::GetFullPath($ExistingEnginesEvidencePath))
    ) "$engineName.json"
    $engineEvidence = Read-BoundedJsonEvidence -Path $engineEvidencePath
    if ((Get-RequiredProperty -Object $engineEvidence -Name "schemaVersion") -ne 1 -or
        (Get-RequiredProperty -Object $engineEvidence -Name "gateId") -cne "G2" -or
        (Get-RequiredProperty -Object $engineEvidence -Name "engine") -cne $engineName -or
        (Get-RequiredProperty -Object $engineEvidence -Name "supportStatus") -cne "supported" -or
        (Get-RequiredProperty -Object $engineEvidence -Name "sourceRevision") -cne
            $existingRevision -or
        (Get-RequiredProperty -Object $engineEvidence -Name "sourceTreeDigest") -cne
            $existingTree -or
        (Get-RequiredProperty -Object $engineEvidence -Name "buildId") -cne
            $existingBuild) {
        throw "PROVIDER_CERTIFICATION_EVIDENCE_IDENTITY_MISMATCH"
    }
    $evidenceChecksum = Get-Blake3 `
        -ExecutablePath $Blake3ExecutablePath `
        -EvidencePath $engineEvidencePath
    [PSCustomObject] [ordered] @{
        id = "g2-$engineName-$($existingBuild.Substring(0, 16))"
        engine = $engineName
        serverVersionRange = [string] $versionRanges[$engineName]
        toolName = $primaryToolName
        toolVersionRange = [string] $versionRanges[$engineName]
        operatingSystem = "windows"
        architecture = "x86_64"
        dumpFormat = [string] $dumpFormats[$engineName]
        consistencyGrade = [string] (
            Get-RequiredProperty -Object $engine -Name "consistencyGrade"
        )
        evidenceLocator = $rootLocator
        evidenceChecksumBlake3 = $evidenceChecksum
        certifiedAtMs = $GeneratedAtMs
        expiresAtMs = $expiresAtMs
        scopeModes = @("whole_database", "explicit_tables")
        capabilities = @($capabilities)
    }
}

$document = [ordered] @{
    schemaVersion = 1
    sourceRevision = $existingRevision
    sourceTreeDigest = $existingTree
    buildId = $existingBuild
    generatedAtMs = $GeneratedAtMs
    certifications = @($certifications)
}
$resolvedOutput = [IO.Path]::GetFullPath($OutputPath)
Write-RestorePlaneJsonAtomic -Path $resolvedOutput -Value $document
$document | ConvertTo-Json -Depth 32
