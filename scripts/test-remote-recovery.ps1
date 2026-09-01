[CmdletBinding()]
param(
    [ValidateSet("all", "s3", "ssh")]
    [string]$Target = "all"
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$MinioImage = "minio/minio@sha256:14cea493d9a34af32f524e538b8346cf79f3321eff8e708c1e2960462bd8936e"
$MinioClientImage = "minio/mc@sha256:a7fe349ef4bd8521fb8497f55c6042871b2ae640607cf99d9bede5e9bdf11727"
$SftpImage = "atmoz/sftp@sha256:a6cb3eb29202ca7f57e73bb7e527286e66e0e822fff65609207c7e0ef2d135a3"

$RunId = "{0}-{1}" -f $PID, [DateTimeOffset]::UtcNow.ToUnixTimeSeconds()
$MinioContainer = "dbbackup-minio-drill-$RunId"
$SftpContainer = "dbbackup-sftp-drill-$RunId"
$SftpVolume = "dbbackup-sftp-drill-$RunId"
$StartedContainers = [System.Collections.Generic.List[string]]::new()
$CreatedVolumes = [System.Collections.Generic.List[string]]::new()
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$MinioReaderPolicy = (Resolve-Path (Join-Path $PSScriptRoot "fixtures\minio-reader-policy.json")).Path

function Invoke-Docker {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]]$Arguments)

    & docker @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "docker $($Arguments -join ' ') failed with exit code $LASTEXITCODE"
    }
}

function Get-PublishedPort {
    param(
        [string]$Container,
        [string]$ContainerPort
    )

    $binding = (& docker port $Container $ContainerPort).Trim()
    if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($binding)) {
        throw "published port lookup failed: $Container $ContainerPort"
    }
    return [int]($binding -replace "^.*:", "")
}

function Wait-HttpReady {
    param(
        [string]$Uri,
        [int]$Attempts = 45
    )

    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        try {
            $response = Invoke-WebRequest -Uri $Uri -TimeoutSec 2 -UseBasicParsing
            if ($response.StatusCode -eq 200) {
                return
            }
        }
        catch {
            if ($attempt -eq $Attempts) {
                throw "service did not become ready: $Uri"
            }
        }
        Start-Sleep -Seconds 1
    }
}

function Wait-TcpReady {
    param(
        [string]$HostName,
        [int]$Port,
        [int]$Attempts = 45
    )

    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        $client = [System.Net.Sockets.TcpClient]::new()
        try {
            $pending = $client.ConnectAsync($HostName, $Port)
            if ($pending.Wait([TimeSpan]::FromSeconds(2)) -and $client.Connected) {
                return
            }
        }
        catch {
            if ($attempt -eq $Attempts) {
                throw "service did not become ready: ${HostName}:$Port"
            }
        }
        finally {
            $client.Dispose()
        }
        Start-Sleep -Seconds 1
    }
}

function Get-SshHostFingerprint {
    param(
        [string]$Container,
        [int]$Attempts = 20
    )

    for ($attempt = 1; $attempt -le $Attempts; $attempt++) {
        & docker exec $Container test -f /etc/ssh/ssh_host_ed25519_key.pub
        if ($LASTEXITCODE -eq 0) {
            $line = & docker exec $Container ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub -E sha256
            if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($line)) {
                return (($line.Trim() -split "\s+")[1])
            }
        }
        Start-Sleep -Milliseconds 250
    }
    throw "failed to read SFTP host-key fingerprint"
}

function Invoke-CargoDrill {
    param([string]$TestName)

    & cargo test -p dbbackup-cli --test remote_recovery_live $TestName -- --ignored --exact --nocapture
    if ($LASTEXITCODE -ne 0) {
        throw "remote recovery test failed: $TestName"
    }
}

function Test-S3Recovery {
    $writerId = "drill-writer"
    $writerSecret = "drill-writer-secret-2026"
    $readerId = "drill-reader"
    $readerSecret = "drill-reader-secret-2026"
    $bucket = "recovery-drill"

    Invoke-Docker run --detach --rm --name $MinioContainer `
        --publish "127.0.0.1::9000" `
        --env "MINIO_ROOT_USER=$writerId" `
        --env "MINIO_ROOT_PASSWORD=$writerSecret" `
        $MinioImage server /data --console-address ":9001"
    $StartedContainers.Add($MinioContainer)

    $port = Get-PublishedPort $MinioContainer "9000/tcp"
    Wait-HttpReady "http://127.0.0.1:$port/minio/health/ready"
    $mcHost = "http://${writerId}:${writerSecret}@127.0.0.1:9000"

    Invoke-Docker run --rm --network "container:$MinioContainer" `
        --env "MC_HOST_local=$mcHost" `
        $MinioClientImage mb --with-lock "local/$bucket"
    Invoke-Docker run --rm --network "container:$MinioContainer" `
        --env "MC_HOST_local=$mcHost" `
        $MinioClientImage admin user add local $readerId $readerSecret
    Invoke-Docker run --rm --network "container:$MinioContainer" `
        --env "MC_HOST_local=$mcHost" `
        --mount "type=bind,source=$MinioReaderPolicy,target=/reader-policy.json,readonly" `
        $MinioClientImage admin policy create local dbbackup-reader /reader-policy.json
    Invoke-Docker run --rm --network "container:$MinioContainer" `
        --env "MC_HOST_local=$mcHost" `
        $MinioClientImage admin policy attach local dbbackup-reader --user $readerId

    $env:S3_ENDPOINT = "http://127.0.0.1:$port"
    $env:S3_REGION = "us-east-1"
    $env:S3_BUCKET = $bucket
    $env:AWS_ACCESS_KEY_ID = $writerId
    $env:AWS_SECRET_ACCESS_KEY = $writerSecret
    $env:S3_READER_ACCESS_KEY_ID = $readerId
    $env:S3_READER_SECRET_ACCESS_KEY = $readerSecret

    Invoke-CargoDrill "s3_reader_rebuilds_verifies_and_restores_without_write_permission"
}

function Test-SshRecovery {
    $writerUser = "drillwriter"
    $writerPassword = "writer-pass-2026"
    $readerUser = "drillreader"
    $readerPassword = "reader-pass-2026"

    Invoke-Docker volume create $SftpVolume
    $CreatedVolumes.Add($SftpVolume)
    Invoke-Docker run --detach --rm --name $SftpContainer `
        --publish "127.0.0.1::22" `
        --mount "source=$SftpVolume,target=/home/$writerUser/backup" `
        --mount "source=$SftpVolume,target=/home/$readerUser/backup" `
        $SftpImage `
        "${writerUser}:${writerPassword}:1001:1001:backup" `
        "${readerUser}:${readerPassword}:1002:1002:backup"
    $StartedContainers.Add($SftpContainer)

    $port = Get-PublishedPort $SftpContainer "22/tcp"
    Wait-TcpReady "127.0.0.1" $port
    Invoke-Docker exec $SftpContainer chown -R "1001:1001" "/home/$writerUser/backup"
    Invoke-Docker exec $SftpContainer chmod 755 "/home/$writerUser/backup"

    $fingerprint = Get-SshHostFingerprint $SftpContainer

    $env:SSH_HOST = "127.0.0.1"
    $env:SSH_PORT = "$port"
    $env:SSH_HOST_FPR = $fingerprint
    $env:SSH_REMOTE_DIR = "/backup"
    $env:SSH_WRITER_USER = $writerUser
    $env:SSH_WRITER_PASSWORD = $writerPassword
    $env:SSH_READER_USER = $readerUser
    $env:SSH_READER_PASSWORD = $readerPassword

    Invoke-CargoDrill "ssh_reader_rebuilds_verifies_and_restores_without_write_permission"
}

Push-Location $RepoRoot
try {
    docker version | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Docker daemon is not available"
    }

    if ($Target -in @("all", "s3")) {
        Test-S3Recovery
    }
    if ($Target -in @("all", "ssh")) {
        Test-SshRecovery
    }
}
finally {
    Pop-Location
    foreach ($container in $StartedContainers) {
        & docker rm --force $container 2>$null | Out-Null
    }
    foreach ($volume in $CreatedVolumes) {
        & docker volume rm --force $volume 2>$null | Out-Null
    }
}
