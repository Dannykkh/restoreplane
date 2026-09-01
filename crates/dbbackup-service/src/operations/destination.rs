use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dbbackup_catalog::{DestinationProbeBinding, DestinationSecretRoleV2};
use dbbackup_dest_s3::{
    S3Cfg, S3DeleteCfg, S3DeleteOperatorV2, S3Dest, S3ManifestSource, S3Provider, S3ReaderCfg,
};
use dbbackup_dest_ssh::{
    HostKeyPolicy, SshAuth, SshDeleteCfg, SshDeleteOperatorV2, SshDest, SshManifestSource,
    SshReaderCfg,
};
use dbbackup_pipeline::{
    DeleteErrorV2, Destination, DestinationDeleteV2, DestinationReaderV2, ImmutabilityKind,
    LocalDeleteOperatorV2, LocalDest, ReplicaEndpointV2,
};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::secrets::{SecretPurpose, ServiceSecretStore};

use super::ConnectionOperationError;

const DESTINATION_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_BYTES: &[u8] = b"dbbackup destination role probe v1";

#[derive(Clone, Debug)]
pub(super) struct DestinationInspection {
    pub roles: Vec<DestinationRoleInspection>,
    pub aggregate_passed: bool,
}

#[derive(Clone, Debug)]
pub(super) struct DestinationRoleInspection {
    pub role: DestinationSecretRoleV2,
    pub passed: bool,
    pub capabilities: serde_json::Value,
    pub free_bytes: Option<i64>,
    pub latency_ms: i64,
    pub failure_class: Option<&'static str>,
    pub operator_action: Option<&'static str>,
}

pub(super) fn inspect_destination(
    binding: &DestinationProbeBinding,
    secrets: Option<&ServiceSecretStore>,
) -> Result<DestinationInspection, ConnectionOperationError> {
    let resolved = resolve_endpoint(binding, secrets)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| {
            terminal(
                "DESTINATION_RUNTIME_UNAVAILABLE",
                "Restart the service and retry.",
            )
        })?;
    runtime
        .block_on(async {
            tokio::time::timeout(DESTINATION_PROBE_TIMEOUT, run_role_probe(resolved)).await
        })
        .map_err(|_| {
            ConnectionOperationError::retryable(
                "DESTINATION_PROBE_TIMEOUT",
                "network",
                "Check the Destination endpoint and retry.",
            )
        })?
}

struct ResolvedDestination {
    writer: Arc<dyn Destination>,
    reader: Arc<dyn DestinationReaderV2>,
    deleter: Option<Arc<dyn DestinationDeleteV2>>,
    immutability: ImmutabilityKind,
}

async fn run_role_probe(
    resolved: ResolvedDestination,
) -> Result<DestinationInspection, ConnectionOperationError> {
    let key = format!(".selftest/probe-{}.bin", uuid::Uuid::new_v4());
    let expected_hash = blake3::hash(PROBE_BYTES).to_hex().to_string();
    let writer_started = Instant::now();
    let write_result = async {
        let mut writer = resolved.writer.writer(&key).await?;
        writer.write_all(PROBE_BYTES).await?;
        writer.commit().await
    }
    .await;
    let writer_latency = elapsed_ms(writer_started);
    let immutability = immutability_name(resolved.immutability);
    let mut roles = Vec::with_capacity(3);
    let receipt = match write_result {
        Ok(receipt) => {
            roles.push(DestinationRoleInspection {
                role: DestinationSecretRoleV2::BackupWriter,
                passed: true,
                capabilities: serde_json::json!({
                    "write": "passed",
                    "commit": "atomic",
                    "immutability": immutability,
                }),
                free_bytes: None,
                latency_ms: writer_latency,
                failure_class: None,
                operator_action: None,
            });
            Some(receipt)
        }
        Err(_) => {
            roles.push(failed_role(
                DestinationSecretRoleV2::BackupWriter,
                writer_latency,
                "writer",
                "Check writer credentials, endpoint permissions, and available capacity.",
            ));
            None
        }
    };

    let reader_started = Instant::now();
    let reader_passed = if receipt.is_some() {
        resolved
            .reader
            .probe_object(&key)
            .await
            .map(|probe| probe.bytes == PROBE_BYTES.len() as u64 && probe.blake3 == expected_hash)
            .unwrap_or(false)
    } else {
        false
    };
    let reader_latency = elapsed_ms(reader_started);
    if reader_passed {
        roles.push(DestinationRoleInspection {
            role: DestinationSecretRoleV2::RestoreReader,
            passed: true,
            capabilities: serde_json::json!({
                "read": "passed",
                "hashVerified": true,
                "immutability": immutability,
            }),
            free_bytes: None,
            latency_ms: reader_latency,
            failure_class: None,
            operator_action: None,
        });
    } else {
        roles.push(failed_role(
            DestinationSecretRoleV2::RestoreReader,
            reader_latency,
            "reader",
            "Check the separate reader credential and object-read permission.",
        ));
    }

    if let Some(deleter) = resolved.deleter {
        let delete_started = Instant::now();
        let version = receipt
            .as_ref()
            .and_then(|receipt| receipt.version.as_deref());
        let delete_result = deleter.delete_object(&key, version).await;
        let delete_passed = matches!(
            delete_result,
            Ok(()) | Err(DeleteErrorV2::RetentionLocked { .. })
        );
        let delete_latency = elapsed_ms(delete_started);
        if delete_passed {
            roles.push(DestinationRoleInspection {
                role: DestinationSecretRoleV2::DeleteOperator,
                passed: true,
                capabilities: serde_json::json!({
                    "delete": if delete_result.is_ok() { "passed" } else { "retention_locked" },
                    "roleSeparated": true,
                    "immutability": immutability,
                }),
                free_bytes: None,
                latency_ms: delete_latency,
                failure_class: None,
                operator_action: None,
            });
        } else {
            roles.push(failed_role(
                DestinationSecretRoleV2::DeleteOperator,
                delete_latency,
                "delete_operator",
                "Check the separate delete credential and retention-lock evidence.",
            ));
        }
    }
    let aggregate_passed = roles
        .iter()
        .filter(|role| {
            matches!(
                role.role,
                DestinationSecretRoleV2::BackupWriter | DestinationSecretRoleV2::RestoreReader
            )
        })
        .all(|role| role.passed);
    Ok(DestinationInspection {
        roles,
        aggregate_passed,
    })
}

fn resolve_endpoint(
    binding: &DestinationProbeBinding,
    secrets: Option<&ServiceSecretStore>,
) -> Result<ResolvedDestination, ConnectionOperationError> {
    match binding.kind() {
        "local" => {
            let endpoint: LocalEndpoint =
                serde_json::from_str(binding.endpoint_json()).map_err(|_| invalid_endpoint())?;
            if !endpoint.path.is_absolute() {
                return Err(invalid_endpoint());
            }
            let writer = Arc::new(LocalDest::new(binding.fingerprint(), endpoint.path.clone()));
            let reader = Arc::new(LocalDest::new(binding.fingerprint(), endpoint.path.clone()));
            let deleter = Arc::new(LocalDeleteOperatorV2::new(endpoint.path));
            Ok(ResolvedDestination {
                writer,
                reader,
                deleter: Some(deleter),
                immutability: ImmutabilityKind::None,
            })
        }
        "s3" => resolve_s3(binding, secrets),
        "ssh_sftp" => resolve_ssh(binding, secrets),
        _ => Err(terminal(
            "DESTINATION_KIND_UNSUPPORTED",
            "Choose a Destination adapter that is supported by this build.",
        )),
    }
}

pub(crate) fn resolve_backup_replica(
    binding: &DestinationProbeBinding,
    secrets: Option<&ServiceSecretStore>,
) -> Result<(ReplicaEndpointV2, &'static str), ConnectionOperationError> {
    let resolved = resolve_endpoint(binding, secrets)?;
    let immutability = immutability_name(resolved.immutability);
    let endpoint = ReplicaEndpointV2::new(binding.fingerprint(), resolved.writer, resolved.reader)
        .map_err(|_| {
            ConnectionOperationError::terminal(
                "DESTINATION_ENDPOINT_INVALID",
                "unknown",
                "Edit the Destination endpoint and retry.",
            )
        })?;
    Ok((endpoint, immutability))
}

/// Resolves only the independently provisioned restore-reader role used by
/// startup commit reconciliation. The append-only writer and delete identities
/// are intentionally neither loaded nor required on this path.
pub(crate) fn resolve_recovery_reader(
    binding: &DestinationProbeBinding,
    secrets: Option<&ServiceSecretStore>,
) -> Result<Arc<dyn DestinationReaderV2>, ConnectionOperationError> {
    match binding.kind() {
        "local" => {
            if binding.reader_secret_ref_id().is_some() {
                return Err(invalid_secret());
            }
            let endpoint: LocalEndpoint =
                serde_json::from_str(binding.endpoint_json()).map_err(|_| invalid_endpoint())?;
            if !endpoint.path.is_absolute() {
                return Err(invalid_endpoint());
            }
            Ok(Arc::new(LocalDest::new(
                binding.fingerprint(),
                endpoint.path,
            )))
        }
        "s3" => {
            let endpoint: S3Endpoint =
                serde_json::from_str(binding.endpoint_json()).map_err(|_| invalid_endpoint())?;
            if endpoint.bucket.trim().is_empty()
                || endpoint.region.trim().is_empty()
                || endpoint.retention_days < endpoint.min_dwell_days
            {
                return Err(invalid_endpoint());
            }
            let credentials = s3_credentials(
                secrets,
                binding.reader_secret_ref_id(),
                SecretPurpose::RestoreReader,
            )?;
            Ok(Arc::new(
                S3ManifestSource::new(S3ReaderCfg {
                    endpoint: endpoint.endpoint_url.unwrap_or_default(),
                    region: endpoint.region,
                    bucket: endpoint.bucket,
                    prefix: endpoint.prefix.unwrap_or_default(),
                    access_key_id: credentials.access_key_id,
                    secret_access_key: credentials.secret_access_key,
                })
                .map_err(|_| invalid_endpoint())?,
            ))
        }
        "ssh_sftp" => {
            let endpoint: SshEndpoint =
                serde_json::from_str(binding.endpoint_json()).map_err(|_| invalid_endpoint())?;
            if endpoint.host.trim().is_empty()
                || endpoint.port == 0
                || endpoint.user.trim().is_empty()
                || endpoint.host_key_fingerprint.trim().is_empty()
                || endpoint.path.trim().is_empty()
            {
                return Err(invalid_endpoint());
            }
            let auth = ssh_credentials(
                secrets,
                binding.reader_secret_ref_id(),
                SecretPurpose::RestoreReader,
            )?;
            Ok(Arc::new(SshManifestSource::new(SshReaderCfg {
                host: endpoint.host,
                port: endpoint.port,
                user: endpoint.user,
                host_key_fingerprint: endpoint.host_key_fingerprint,
                auth,
                remote_dir: endpoint.path,
            })))
        }
        _ => Err(terminal(
            "DESTINATION_KIND_UNSUPPORTED",
            "Choose a Destination adapter that is supported by this build.",
        )),
    }
}

fn resolve_s3(
    binding: &DestinationProbeBinding,
    secrets: Option<&ServiceSecretStore>,
) -> Result<ResolvedDestination, ConnectionOperationError> {
    let endpoint: S3Endpoint =
        serde_json::from_str(binding.endpoint_json()).map_err(|_| invalid_endpoint())?;
    if endpoint.bucket.trim().is_empty()
        || endpoint.region.trim().is_empty()
        || endpoint.retention_days < endpoint.min_dwell_days
    {
        return Err(invalid_endpoint());
    }
    let writer_credentials = s3_credentials(
        secrets,
        binding.writer_secret_ref_id(),
        SecretPurpose::BackupWriter,
    )?;
    let reader_credentials = s3_credentials(
        secrets,
        binding.reader_secret_ref_id(),
        SecretPurpose::RestoreReader,
    )?;
    let immutability = match endpoint.object_lock_mode.as_deref().unwrap_or("none") {
        "none" => ImmutabilityKind::None,
        "governance" => ImmutabilityKind::ObjectLockGovernance,
        "compliance" => ImmutabilityKind::ObjectLockCompliance,
        _ => return Err(invalid_endpoint()),
    };
    let provider = match endpoint.provider.as_deref().unwrap_or("aws") {
        "aws" => S3Provider::Aws,
        "minio" => S3Provider::MinIO,
        "backblaze_b2" => S3Provider::BackblazeB2,
        "cloudflare_r2" => S3Provider::CloudflareR2,
        _ => return Err(invalid_endpoint()),
    };
    let endpoint_url = endpoint.endpoint_url.unwrap_or_default();
    let prefix = endpoint.prefix.unwrap_or_default();
    let writer = S3Dest::new(
        binding.fingerprint(),
        S3Cfg {
            endpoint: endpoint_url.clone(),
            region: endpoint.region.clone(),
            bucket: endpoint.bucket.clone(),
            prefix: prefix.clone(),
            access_key_id: writer_credentials.access_key_id,
            secret_access_key: writer_credentials.secret_access_key,
            object_lock_mode: immutability,
            retention_days: endpoint.retention_days,
            min_dwell_days: endpoint.min_dwell_days,
            checksum_algo: endpoint.checksum_algo,
            compliance_opt_in: endpoint.compliance_opt_in,
            provider,
        },
    )
    .map_err(|_| invalid_endpoint())?;
    let reader = S3ManifestSource::new(S3ReaderCfg {
        endpoint: endpoint_url.clone(),
        region: endpoint.region.clone(),
        bucket: endpoint.bucket.clone(),
        prefix: prefix.clone(),
        access_key_id: reader_credentials.access_key_id,
        secret_access_key: reader_credentials.secret_access_key,
    })
    .map_err(|_| invalid_endpoint())?;
    let deleter = match binding.delete_secret_ref_id() {
        Some(secret_ref_id) => {
            let credentials =
                s3_credentials(secrets, Some(secret_ref_id), SecretPurpose::RetentionDelete)?;
            Some(Arc::new(
                S3DeleteOperatorV2::new(S3DeleteCfg {
                    endpoint: endpoint_url,
                    region: endpoint.region,
                    bucket: endpoint.bucket,
                    prefix,
                    access_key_id: credentials.access_key_id,
                    secret_access_key: credentials.secret_access_key,
                })
                .map_err(|_| invalid_endpoint())?,
            ) as Arc<dyn DestinationDeleteV2>)
        }
        None => None,
    };
    Ok(ResolvedDestination {
        writer: Arc::new(writer),
        reader: Arc::new(reader),
        deleter,
        immutability,
    })
}

fn resolve_ssh(
    binding: &DestinationProbeBinding,
    secrets: Option<&ServiceSecretStore>,
) -> Result<ResolvedDestination, ConnectionOperationError> {
    let endpoint: SshEndpoint =
        serde_json::from_str(binding.endpoint_json()).map_err(|_| invalid_endpoint())?;
    if endpoint.host.trim().is_empty()
        || endpoint.port == 0
        || endpoint.user.trim().is_empty()
        || endpoint.host_key_fingerprint.trim().is_empty()
        || endpoint.path.trim().is_empty()
    {
        return Err(invalid_endpoint());
    }
    let writer_auth = ssh_credentials(
        secrets,
        binding.writer_secret_ref_id(),
        SecretPurpose::BackupWriter,
    )?;
    let reader_auth = ssh_credentials(
        secrets,
        binding.reader_secret_ref_id(),
        SecretPurpose::RestoreReader,
    )?;
    let append_only = endpoint.append_only;
    let label = format!("{}:{}", endpoint.host, endpoint.port);
    let writer = SshDest::new(
        binding.fingerprint(),
        endpoint.host.clone(),
        endpoint.port,
        endpoint.user.clone(),
        HostKeyPolicy::new(endpoint.host_key_fingerprint.clone(), label),
        writer_auth,
        endpoint.path.clone(),
        append_only,
    );
    let reader = SshManifestSource::new(SshReaderCfg {
        host: endpoint.host.clone(),
        port: endpoint.port,
        user: endpoint.user.clone(),
        host_key_fingerprint: endpoint.host_key_fingerprint.clone(),
        auth: reader_auth,
        remote_dir: endpoint.path.clone(),
    });
    let deleter = match binding.delete_secret_ref_id() {
        Some(secret_ref_id) => {
            let auth =
                ssh_credentials(secrets, Some(secret_ref_id), SecretPurpose::RetentionDelete)?;
            Some(Arc::new(
                SshDeleteOperatorV2::new(SshDeleteCfg {
                    host: endpoint.host,
                    port: endpoint.port,
                    user: endpoint.user,
                    host_key_fingerprint: endpoint.host_key_fingerprint,
                    auth,
                    remote_dir: endpoint.path,
                })
                .map_err(|_| invalid_endpoint())?,
            ) as Arc<dyn DestinationDeleteV2>)
        }
        None => None,
    };
    Ok(ResolvedDestination {
        writer: Arc::new(writer),
        reader: Arc::new(reader),
        deleter,
        immutability: if append_only {
            ImmutabilityKind::ServerAppendOnly
        } else {
            ImmutabilityKind::None
        },
    })
}

fn s3_credentials(
    secrets: Option<&ServiceSecretStore>,
    secret_ref_id: Option<&dbbackup_domain::SecretRefId>,
    purpose: SecretPurpose,
) -> Result<S3Credentials, ConnectionOperationError> {
    let secret = resolve_secret(secrets, secret_ref_id, purpose)?;
    let credentials: S3Credentials =
        serde_json::from_slice(secret.expose()).map_err(|_| invalid_secret())?;
    if credentials
        .schema_version
        .is_some_and(|version| version != 1)
        || credentials.access_key_id.trim().is_empty()
        || credentials.secret_access_key.is_empty()
    {
        return Err(invalid_secret());
    }
    Ok(credentials)
}

fn ssh_credentials(
    secrets: Option<&ServiceSecretStore>,
    secret_ref_id: Option<&dbbackup_domain::SecretRefId>,
    purpose: SecretPurpose,
) -> Result<SshAuth, ConnectionOperationError> {
    let secret = resolve_secret(secrets, secret_ref_id, purpose)?;
    let credential: SshCredentials =
        serde_json::from_slice(secret.expose()).map_err(|_| invalid_secret())?;
    if credential.schema_version != 1 {
        return Err(invalid_secret());
    }
    match credential.auth {
        SshCredentialAuth::Password { password } if !password.is_empty() => Ok(SshAuth::Password {
            password: Zeroizing::new(password),
        }),
        SshCredentialAuth::PublicKey {
            identity_pem,
            passphrase,
        } if !identity_pem.is_empty() => Ok(SshAuth::PublicKey {
            identity_pem: Zeroizing::new(identity_pem),
            passphrase: passphrase.map(Zeroizing::new),
        }),
        _ => Err(invalid_secret()),
    }
}

fn resolve_secret(
    secrets: Option<&ServiceSecretStore>,
    secret_ref_id: Option<&dbbackup_domain::SecretRefId>,
    purpose: SecretPurpose,
) -> Result<crate::secrets::SecretBuffer, ConnectionOperationError> {
    let secrets = secrets.ok_or_else(invalid_secret)?;
    let secret_ref_id = secret_ref_id.ok_or_else(invalid_secret)?;
    secrets
        .resolve(secret_ref_id, purpose)
        .map_err(|_| invalid_secret())
}

fn failed_role(
    role: DestinationSecretRoleV2,
    latency_ms: i64,
    failure_class: &'static str,
    operator_action: &'static str,
) -> DestinationRoleInspection {
    DestinationRoleInspection {
        role,
        passed: false,
        capabilities: serde_json::json!({
            "probe": "failed",
            "immutability": "none",
        }),
        free_bytes: None,
        latency_ms,
        failure_class: Some(failure_class),
        operator_action: Some(operator_action),
    }
}

fn elapsed_ms(started: Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

const fn immutability_name(value: ImmutabilityKind) -> &'static str {
    match value {
        ImmutabilityKind::None => "none",
        ImmutabilityKind::ObjectLockGovernance => "governance",
        ImmutabilityKind::ObjectLockCompliance => "compliance",
        ImmutabilityKind::ServerAppendOnly => "server_append_only",
    }
}

fn invalid_endpoint() -> ConnectionOperationError {
    terminal(
        "DESTINATION_ENDPOINT_INVALID",
        "Edit the Destination endpoint and retry.",
    )
}

fn invalid_secret() -> ConnectionOperationError {
    ConnectionOperationError::terminal(
        "DESTINATION_SECRET_INVALID",
        "authentication",
        "Re-enter the role-specific credential and retry.",
    )
}

fn terminal(code: &str, action: &str) -> ConnectionOperationError {
    ConnectionOperationError::terminal(code, "unknown", action)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalEndpoint {
    path: PathBuf,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct S3Endpoint {
    bucket: String,
    #[serde(default = "default_s3_region")]
    region: String,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default, alias = "endpoint")]
    endpoint_url: Option<String>,
    #[serde(default, rename = "forcePathStyle")]
    _force_path_style: bool,
    #[serde(default)]
    object_lock_mode: Option<String>,
    #[serde(default)]
    retention_days: u32,
    #[serde(default)]
    min_dwell_days: u32,
    #[serde(default)]
    checksum_algo: Option<String>,
    #[serde(default)]
    compliance_opt_in: bool,
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct S3Credentials {
    #[serde(default)]
    schema_version: Option<u32>,
    access_key_id: String,
    secret_access_key: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SshEndpoint {
    host: String,
    port: u16,
    #[serde(alias = "username")]
    user: String,
    #[serde(alias = "hostKeySha256")]
    host_key_fingerprint: String,
    path: String,
    #[serde(default)]
    append_only: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SshCredentials {
    schema_version: u32,
    auth: SshCredentialAuth,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SshCredentialAuth {
    Password {
        password: String,
    },
    PublicKey {
        identity_pem: String,
        passphrase: Option<String>,
    },
}

fn default_s3_region() -> String {
    "us-east-1".to_owned()
}
