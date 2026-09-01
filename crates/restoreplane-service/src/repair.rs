use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ed25519_dalek::VerifyingKey;
use restoreplane_catalog::{
    prepare_catalog_rebuild_v2, CatalogRebuildDispositionV2, CatalogRebuildRejectReasonV2,
    ClaimedReplicaRepairV2, ConfigurationRepository, DestinationProbeBinding,
    DestinationRepairBindingV2, LocalManifestSource, ManifestSource, RepositoryError,
};
use restoreplane_dest_s3::{S3Cfg, S3Dest, S3ManifestSource, S3Provider, S3ReaderCfg};
use restoreplane_dest_ssh::{HostKeyPolicy, SshAuth, SshDest, SshManifestSource, SshReaderCfg};
use restoreplane_domain::DestinationId;
use restoreplane_pipeline::{
    Destination, DestinationReaderV2, ImmutabilityKind, LocalDest, PipelineCfg, RepairSourceV2,
    ReplicaEndpointV2, SignedSetManifestV2,
};
use restoreplane_runtime::{
    ReplicaRepairResolutionErrorV2, ReplicaRepairResolverV2, ReplicaRepairWorkerErrorV2,
    ResolvedReplicaRepairV2,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::secrets::{SecretBuffer, SecretPurpose, SecretStoreError, ServiceSecretStore};

mod notification_runtime;
mod queue_runtime;
mod replica_repair_runtime;

pub use notification_runtime::{
    NotificationRuntimeStartError, ServiceNotificationDeliveryRuntimeHandle,
};
pub use replica_repair_runtime::{
    ReplicaRepairRuntimeStartError, ServiceReplicaRepairRuntimeHandle,
};

const BINDING_MISSING: &str = "REPLICA_REPAIR_SECRET_BINDING_MISSING";
const DESTINATION_INVALID: &str = "REPLICA_REPAIR_DESTINATION_INVALID";
const DESTINATION_UNSUPPORTED: &str = "REPLICA_REPAIR_DESTINATION_UNSUPPORTED";
const MANIFEST_INVALID: &str = "REPLICA_REPAIR_MANIFEST_INVALID";
const MANIFEST_READ_FAILED: &str = "REPLICA_REPAIR_MANIFEST_READ_FAILED";
const SECRET_INVALID: &str = "REPLICA_REPAIR_SECRET_INVALID";
const SECRET_UNEXPECTED: &str = "REPLICA_REPAIR_SECRET_UNEXPECTED";
const MAX_SET_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRebuildPreview {
    schema_version: u32,
    destination_id: String,
    destination_fingerprint: String,
    observed_at_ms: i64,
    total_count: usize,
    accepted_count: usize,
    rejected_count: usize,
    duplicate_count: usize,
    unknown_key_count: usize,
    candidates: Vec<CatalogRebuildPreviewCandidate>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogRebuildPreviewCandidate {
    object_key: String,
    disposition: String,
    reason_code: Option<String>,
    signing_key_id: Option<String>,
    recovery_point_id: Option<String>,
    manifest_checksum_blake3: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CatalogRebuildPreviewError {
    #[error("CATALOG_REBUILD_PREVIEW_INPUT_INVALID")]
    InputInvalid,
    #[error("CATALOG_REBUILD_PREVIEW_SOURCE_UNAVAILABLE")]
    SourceUnavailable,
    #[error("CATALOG_REBUILD_PREVIEW_SCAN_FAILED")]
    ScanFailed,
}

impl CatalogRebuildPreviewError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "CATALOG_REBUILD_PREVIEW_INPUT_INVALID",
            Self::SourceUnavailable => "CATALOG_REBUILD_PREVIEW_SOURCE_UNAVAILABLE",
            Self::ScanFailed => "CATALOG_REBUILD_PREVIEW_SCAN_FAILED",
        }
    }
}

struct CatalogRebuildSource {
    manifests: Arc<dyn ManifestSource>,
    reader: Arc<dyn DestinationReaderV2>,
}

pub struct ServiceCatalogRebuildPreviewer {
    configuration: ConfigurationRepository,
    secrets: Arc<ServiceSecretStore>,
    trusted_signing_keys: BTreeMap<String, VerifyingKey>,
}

impl ServiceCatalogRebuildPreviewer {
    pub fn new(
        configuration: ConfigurationRepository,
        secrets: Arc<ServiceSecretStore>,
        trusted_signing_keys: BTreeMap<String, VerifyingKey>,
    ) -> Self {
        Self {
            configuration,
            secrets,
            trusted_signing_keys,
        }
    }

    pub fn preview(
        &self,
        destination_id: &DestinationId,
        job_filter: Option<&str>,
        observed_at_ms: i64,
    ) -> Result<CatalogRebuildPreview, CatalogRebuildPreviewError> {
        if observed_at_ms < 0
            || job_filter.is_some_and(|value| {
                value.is_empty()
                    || value.len() > 256
                    || value.starts_with('/')
                    || value.contains('\\')
                    || value
                        .split('/')
                        .any(|part| part.is_empty() || part == "." || part == "..")
            })
        {
            return Err(CatalogRebuildPreviewError::InputInvalid);
        }
        let binding = self
            .configuration
            .destination_probe_binding(destination_id)
            .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
        let source = self.catalog_source(&binding)?;
        let destination_id = destination_id.clone();
        let destination_fingerprint = binding.fingerprint();
        let job_filter = job_filter.map(str::to_owned);
        let trusted_signing_keys = self.trusted_signing_keys.clone();
        let thread = std::thread::Builder::new()
            .name("catalog-rebuild-preview".to_owned())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| CatalogRebuildPreviewError::ScanFailed)?;
                runtime.block_on(async move {
                    let manifest_keys = source
                        .manifests
                        .list_manifests(job_filter.as_deref())
                        .await
                        .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
                    let plan = prepare_catalog_rebuild_v2(
                        source.reader.as_ref(),
                        &destination_fingerprint,
                        manifest_keys,
                        &trusted_signing_keys,
                        observed_at_ms,
                    )
                    .await
                    .map_err(|_| CatalogRebuildPreviewError::ScanFailed)?;
                    let candidates = plan
                        .candidates()
                        .iter()
                        .map(|candidate| {
                            let duplicate = candidate.reject_reason()
                                == Some(CatalogRebuildRejectReasonV2::DuplicateRecoveryPoint);
                            CatalogRebuildPreviewCandidate {
                                object_key: candidate.key().to_owned(),
                                disposition: if duplicate {
                                    "duplicate".to_owned()
                                } else {
                                    match candidate.disposition() {
                                        CatalogRebuildDispositionV2::Accepted => {
                                            "accepted".to_owned()
                                        }
                                        CatalogRebuildDispositionV2::Rejected => {
                                            "rejected".to_owned()
                                        }
                                        CatalogRebuildDispositionV2::UnknownKey => {
                                            "unknown_key".to_owned()
                                        }
                                    }
                                },
                                reason_code: candidate
                                    .reject_reason()
                                    .map(catalog_rebuild_reason_code)
                                    .map(str::to_owned),
                                signing_key_id: candidate.signing_key_id().map(str::to_owned),
                                recovery_point_id: candidate.recovery_point_id().map(str::to_owned),
                                manifest_checksum_blake3: candidate
                                    .manifest_checksum_blake3()
                                    .map(str::to_owned),
                            }
                        })
                        .collect::<Vec<_>>();
                    let duplicate_count = candidates
                        .iter()
                        .filter(|candidate| candidate.disposition == "duplicate")
                        .count();
                    let rejected_count = candidates
                        .iter()
                        .filter(|candidate| candidate.disposition == "rejected")
                        .count();
                    Ok(CatalogRebuildPreview {
                        schema_version: 1,
                        destination_id: destination_id.as_str().to_owned(),
                        destination_fingerprint,
                        observed_at_ms,
                        total_count: candidates.len(),
                        accepted_count: plan.accepted_count(),
                        rejected_count,
                        duplicate_count,
                        unknown_key_count: plan.unknown_key_count(),
                        candidates,
                    })
                })
            })
            .map_err(|_| CatalogRebuildPreviewError::ScanFailed)?;
        thread
            .join()
            .map_err(|_| CatalogRebuildPreviewError::ScanFailed)?
    }

    fn catalog_source(
        &self,
        binding: &DestinationProbeBinding,
    ) -> Result<CatalogRebuildSource, CatalogRebuildPreviewError> {
        match binding.kind() {
            "local" => {
                if binding.reader_secret_ref_id().is_some() {
                    return Err(CatalogRebuildPreviewError::SourceUnavailable);
                }
                let root = local_root(binding.endpoint_json(), true)
                    .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
                let manifests: Arc<dyn ManifestSource> =
                    Arc::new(LocalManifestSource::new(root.clone()));
                let reader: Arc<dyn DestinationReaderV2> =
                    Arc::new(LocalDest::new(binding.fingerprint(), root));
                Ok(CatalogRebuildSource { manifests, reader })
            }
            "s3" => {
                let secret = self
                    .secrets
                    .resolve(
                        binding
                            .reader_secret_ref_id()
                            .ok_or(CatalogRebuildPreviewError::SourceUnavailable)?,
                        SecretPurpose::RestoreReader,
                    )
                    .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
                let endpoint = s3_endpoint(binding.endpoint_json())
                    .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
                let credentials = s3_credentials(secret.expose())
                    .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
                let source = Arc::new(
                    S3ManifestSource::new(S3ReaderCfg {
                        endpoint: endpoint.endpoint.unwrap_or_default(),
                        region: endpoint.region,
                        bucket: endpoint.bucket,
                        prefix: endpoint.prefix.unwrap_or_default(),
                        access_key_id: credentials.access_key_id,
                        secret_access_key: credentials.secret_access_key,
                    })
                    .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?,
                );
                let manifests: Arc<dyn ManifestSource> = source.clone();
                let reader: Arc<dyn DestinationReaderV2> = source;
                Ok(CatalogRebuildSource { manifests, reader })
            }
            "ssh_sftp" => {
                let secret = self
                    .secrets
                    .resolve(
                        binding
                            .reader_secret_ref_id()
                            .ok_or(CatalogRebuildPreviewError::SourceUnavailable)?,
                        SecretPurpose::RestoreReader,
                    )
                    .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
                let endpoint = ssh_endpoint(binding.endpoint_json())
                    .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
                let auth = ssh_auth(secret.expose())
                    .map_err(|_| CatalogRebuildPreviewError::SourceUnavailable)?;
                let source = Arc::new(SshManifestSource::new(SshReaderCfg {
                    host: endpoint.host,
                    port: endpoint.port,
                    user: endpoint.user,
                    host_key_fingerprint: endpoint.host_key_fingerprint,
                    auth,
                    remote_dir: endpoint.path,
                }));
                let manifests: Arc<dyn ManifestSource> = source.clone();
                let reader: Arc<dyn DestinationReaderV2> = source;
                Ok(CatalogRebuildSource { manifests, reader })
            }
            _ => Err(CatalogRebuildPreviewError::SourceUnavailable),
        }
    }
}

const fn catalog_rebuild_reason_code(reason: CatalogRebuildRejectReasonV2) -> &'static str {
    match reason {
        CatalogRebuildRejectReasonV2::ReadFailed => "READ_FAILED",
        CatalogRebuildRejectReasonV2::MalformedEnvelope => "MALFORMED_ENVELOPE",
        CatalogRebuildRejectReasonV2::NonCanonicalEnvelope => "NON_CANONICAL_ENVELOPE",
        CatalogRebuildRejectReasonV2::SignatureInvalid => "SIGNATURE_INVALID",
        CatalogRebuildRejectReasonV2::DestinationNotDeclared => "DESTINATION_NOT_DECLARED",
        CatalogRebuildRejectReasonV2::ManifestKeyMismatch => "MANIFEST_KEY_MISMATCH",
        CatalogRebuildRejectReasonV2::PayloadUnavailable => "PAYLOAD_UNAVAILABLE",
        CatalogRebuildRejectReasonV2::PayloadMismatch => "PAYLOAD_MISMATCH",
        CatalogRebuildRejectReasonV2::ProjectionInvalid => "PROJECTION_INVALID",
        CatalogRebuildRejectReasonV2::DuplicateRecoveryPoint => "DUPLICATE_RECOVERY_POINT",
    }
}

pub struct ServiceReplicaRepairResolverV2 {
    configuration: ConfigurationRepository,
    secrets: Arc<ServiceSecretStore>,
    trusted_signing_keys: BTreeMap<String, VerifyingKey>,
    pipeline_config: PipelineCfg,
}

impl ServiceReplicaRepairResolverV2 {
    pub fn new(
        configuration: ConfigurationRepository,
        secrets: Arc<ServiceSecretStore>,
        trusted_signing_keys: BTreeMap<String, VerifyingKey>,
        pipeline_config: PipelineCfg,
    ) -> Result<Self, ReplicaRepairWorkerErrorV2> {
        if pipeline_config.max_parallel_destinations == 0
            || trusted_signing_keys.is_empty()
            || trusted_signing_keys
                .keys()
                .any(|key_id| key_id.trim().is_empty() || key_id.len() > 256)
        {
            return Err(ReplicaRepairWorkerErrorV2::InputInvalid);
        }
        Ok(Self {
            configuration,
            secrets,
            trusted_signing_keys,
            pipeline_config,
        })
    }

    fn binding(
        &self,
        destination_fingerprint: &str,
    ) -> Result<DestinationRepairBindingV2, ReplicaRepairResolutionErrorV2> {
        self.configuration
            .destination_repair_binding(destination_fingerprint)
            .map_err(repository_resolution_error)
    }

    fn secret(
        &self,
        secret_ref_id: Option<&restoreplane_domain::SecretRefId>,
        purpose: SecretPurpose,
    ) -> Result<SecretBuffer, ReplicaRepairResolutionErrorV2> {
        let secret_ref_id = secret_ref_id.ok_or_else(|| terminal(BINDING_MISSING))?;
        self.secrets
            .resolve(secret_ref_id, purpose)
            .map_err(secret_resolution_error)
    }

    fn reader(
        &self,
        binding: &DestinationRepairBindingV2,
        secret_ref_id: Option<&restoreplane_domain::SecretRefId>,
        source: bool,
    ) -> Result<Arc<dyn DestinationReaderV2>, ReplicaRepairResolutionErrorV2> {
        match binding.kind() {
            "local" => {
                reject_unexpected_secret(secret_ref_id)?;
                let root = local_root(binding.endpoint_json(), source)?;
                Ok(Arc::new(LocalDest::new(binding.fingerprint(), root)))
            }
            "s3" => {
                let secret = self.secret(secret_ref_id, SecretPurpose::RestoreReader)?;
                let endpoint = s3_endpoint(binding.endpoint_json())?;
                let credentials = s3_credentials(secret.expose())?;
                let reader = S3ManifestSource::new(S3ReaderCfg {
                    endpoint: endpoint.endpoint.unwrap_or_default(),
                    region: endpoint.region,
                    bucket: endpoint.bucket,
                    prefix: endpoint.prefix.unwrap_or_default(),
                    access_key_id: credentials.access_key_id,
                    secret_access_key: credentials.secret_access_key,
                })
                .map_err(|_| terminal(DESTINATION_INVALID))?;
                Ok(Arc::new(reader))
            }
            "ssh_sftp" => {
                let secret = self.secret(secret_ref_id, SecretPurpose::RestoreReader)?;
                let endpoint = ssh_endpoint(binding.endpoint_json())?;
                let auth = ssh_auth(secret.expose())?;
                Ok(Arc::new(SshManifestSource::new(SshReaderCfg {
                    host: endpoint.host,
                    port: endpoint.port,
                    user: endpoint.user,
                    host_key_fingerprint: endpoint.host_key_fingerprint,
                    auth,
                    remote_dir: endpoint.path,
                })))
            }
            _ => Err(terminal(DESTINATION_UNSUPPORTED)),
        }
    }

    fn writer(
        &self,
        binding: &DestinationRepairBindingV2,
        secret_ref_id: Option<&restoreplane_domain::SecretRefId>,
    ) -> Result<Arc<dyn Destination>, ReplicaRepairResolutionErrorV2> {
        match binding.kind() {
            "local" => {
                reject_unexpected_secret(secret_ref_id)?;
                let root = local_root(binding.endpoint_json(), false)?;
                Ok(Arc::new(LocalDest::new(binding.fingerprint(), root)))
            }
            "s3" => {
                let secret = self.secret(secret_ref_id, SecretPurpose::BackupWriter)?;
                let endpoint = s3_endpoint(binding.endpoint_json())?;
                let credentials = s3_credentials(secret.expose())?;
                let object_lock_mode = s3_immutability(endpoint.object_lock_mode.as_deref())?;
                let provider = s3_provider(endpoint.provider.as_deref())?;
                let writer = S3Dest::new(
                    binding.fingerprint(),
                    S3Cfg {
                        endpoint: endpoint.endpoint.unwrap_or_default(),
                        region: endpoint.region,
                        bucket: endpoint.bucket,
                        prefix: endpoint.prefix.unwrap_or_default(),
                        access_key_id: credentials.access_key_id,
                        secret_access_key: credentials.secret_access_key,
                        object_lock_mode,
                        retention_days: endpoint.retention_days,
                        min_dwell_days: endpoint.min_dwell_days,
                        checksum_algo: endpoint.checksum_algo,
                        compliance_opt_in: endpoint.compliance_opt_in,
                        provider,
                    },
                )
                .map_err(|_| terminal(DESTINATION_INVALID))?;
                Ok(Arc::new(writer))
            }
            "ssh_sftp" => {
                let secret = self.secret(secret_ref_id, SecretPurpose::BackupWriter)?;
                let endpoint = ssh_endpoint(binding.endpoint_json())?;
                let auth = ssh_auth(secret.expose())?;
                let label = format!("{}:{}", endpoint.host, endpoint.port);
                let policy = HostKeyPolicy::new(endpoint.host_key_fingerprint, label);
                Ok(Arc::new(SshDest::new(
                    binding.fingerprint(),
                    endpoint.host,
                    endpoint.port,
                    endpoint.user,
                    policy,
                    auth,
                    endpoint.path,
                    endpoint.append_only,
                )))
            }
            _ => Err(terminal(DESTINATION_UNSUPPORTED)),
        }
    }
}

#[async_trait]
impl ReplicaRepairResolverV2 for ServiceReplicaRepairResolverV2 {
    async fn resolve(
        &self,
        request: &ClaimedReplicaRepairV2,
    ) -> Result<ResolvedReplicaRepairV2, ReplicaRepairResolutionErrorV2> {
        let source_binding = self.binding(request.source_destination_fingerprint())?;
        let source_reader =
            self.reader(&source_binding, request.source_reader_secret_ref_id(), true)?;
        let signed_bytes = source_reader
            .read_bytes_bounded(request.source_manifest_object_key(), MAX_SET_MANIFEST_BYTES)
            .await
            .map_err(|error| resolution(MANIFEST_READ_FAILED, error.is_retryable()))?;
        if blake3::hash(&signed_bytes).to_hex().as_str()
            != request.source_manifest_checksum_blake3()
        {
            return Err(terminal(MANIFEST_INVALID));
        }
        let signed = serde_json::from_slice::<SignedSetManifestV2>(&signed_bytes)
            .map_err(|_| terminal(MANIFEST_INVALID))?;
        if signed.manifest.recovery_point_id.to_string() != request.recovery_point_id().as_str()
            || signed.manifest.signing_key_id != request.signing_key_id()
        {
            return Err(terminal(MANIFEST_INVALID));
        }
        let verifying_key = self
            .trusted_signing_keys
            .get(request.signing_key_id())
            .copied()
            .ok_or_else(|| terminal(MANIFEST_INVALID))?;
        if signed.verify(&verifying_key).is_err() {
            return Err(terminal(MANIFEST_INVALID));
        }

        let target_binding = self.binding(request.target_destination_fingerprint())?;
        let target_writer = self.writer(&target_binding, request.target_writer_secret_ref_id())?;
        let target_reader = self.reader(
            &target_binding,
            request.target_reader_secret_ref_id(),
            false,
        )?;
        let source = RepairSourceV2::new(request.source_destination_fingerprint(), source_reader)
            .map_err(|error| terminal(error.code()))?;
        let target = ReplicaEndpointV2::new(
            request.target_destination_fingerprint(),
            target_writer,
            target_reader,
        )
        .map_err(|error| terminal(error.code()))?;
        ResolvedReplicaRepairV2::new(
            signed,
            source,
            target,
            self.pipeline_config.clone(),
            verifying_key,
        )
        .map_err(|error| terminal(error.code()))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalEndpointV1 {
    path: PathBuf,
}

fn local_root(
    endpoint_json: &str,
    require_existing: bool,
) -> Result<PathBuf, ReplicaRepairResolutionErrorV2> {
    let endpoint: LocalEndpointV1 =
        serde_json::from_str(endpoint_json).map_err(|_| terminal(DESTINATION_INVALID))?;
    if !endpoint.path.is_absolute() {
        return Err(terminal(DESTINATION_INVALID));
    }
    if !require_existing {
        return Ok(endpoint.path);
    }
    let root = std::fs::canonicalize(&endpoint.path).map_err(io_resolution_error)?;
    if !root.is_dir() {
        return Err(terminal(DESTINATION_INVALID));
    }
    Ok(root)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct S3EndpointV1 {
    #[serde(alias = "endpointUrl")]
    endpoint: Option<String>,
    region: String,
    bucket: String,
    prefix: Option<String>,
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
    #[serde(default, rename = "forcePathStyle")]
    _force_path_style: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct S3CredentialV1 {
    schema_version: u32,
    access_key_id: String,
    secret_access_key: String,
}

fn s3_endpoint(endpoint_json: &str) -> Result<S3EndpointV1, ReplicaRepairResolutionErrorV2> {
    let endpoint: S3EndpointV1 =
        serde_json::from_str(endpoint_json).map_err(|_| terminal(DESTINATION_INVALID))?;
    if endpoint.region.trim().is_empty()
        || endpoint.bucket.trim().is_empty()
        || endpoint.retention_days < endpoint.min_dwell_days
    {
        return Err(terminal(DESTINATION_INVALID));
    }
    Ok(endpoint)
}

fn s3_credentials(secret: &[u8]) -> Result<S3CredentialV1, ReplicaRepairResolutionErrorV2> {
    let credentials: S3CredentialV1 =
        serde_json::from_slice(secret).map_err(|_| terminal(SECRET_INVALID))?;
    if credentials.schema_version != 1
        || credentials.access_key_id.trim().is_empty()
        || credentials.secret_access_key.is_empty()
    {
        return Err(terminal(SECRET_INVALID));
    }
    Ok(credentials)
}

fn s3_immutability(
    value: Option<&str>,
) -> Result<ImmutabilityKind, ReplicaRepairResolutionErrorV2> {
    match value.unwrap_or("none") {
        "none" => Ok(ImmutabilityKind::None),
        "governance" => Ok(ImmutabilityKind::ObjectLockGovernance),
        "compliance" => Ok(ImmutabilityKind::ObjectLockCompliance),
        _ => Err(terminal(DESTINATION_INVALID)),
    }
}

fn s3_provider(value: Option<&str>) -> Result<S3Provider, ReplicaRepairResolutionErrorV2> {
    match value.unwrap_or("aws") {
        "aws" => Ok(S3Provider::Aws),
        "minio" => Ok(S3Provider::MinIO),
        "backblaze_b2" => Ok(S3Provider::BackblazeB2),
        "cloudflare_r2" => Ok(S3Provider::CloudflareR2),
        _ => Err(terminal(DESTINATION_INVALID)),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SshEndpointV1 {
    host: String,
    port: u16,
    #[serde(alias = "username")]
    user: String,
    #[serde(alias = "hostKeySha256")]
    host_key_fingerprint: String,
    path: String,
    #[serde(default)]
    append_only: bool,
    #[serde(default)]
    private_key_path: Option<String>,
    #[serde(default)]
    tunnel: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SshCredentialV1 {
    schema_version: u32,
    auth: SshCredentialAuthV1,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SshCredentialAuthV1 {
    Password {
        password: String,
    },
    PublicKey {
        identity_pem: String,
        passphrase: Option<String>,
    },
}

fn ssh_endpoint(endpoint_json: &str) -> Result<SshEndpointV1, ReplicaRepairResolutionErrorV2> {
    let endpoint: SshEndpointV1 =
        serde_json::from_str(endpoint_json).map_err(|_| terminal(DESTINATION_INVALID))?;
    if endpoint.host.trim().is_empty()
        || endpoint.port == 0
        || endpoint.user.trim().is_empty()
        || endpoint.host_key_fingerprint.trim().is_empty()
        || endpoint.path.trim().is_empty()
    {
        return Err(terminal(DESTINATION_INVALID));
    }
    let _ = &endpoint.private_key_path;
    let _ = &endpoint.tunnel;
    Ok(endpoint)
}

fn ssh_auth(secret: &[u8]) -> Result<SshAuth, ReplicaRepairResolutionErrorV2> {
    let credential: SshCredentialV1 =
        serde_json::from_slice(secret).map_err(|_| terminal(SECRET_INVALID))?;
    if credential.schema_version != 1 {
        return Err(terminal(SECRET_INVALID));
    }
    match credential.auth {
        SshCredentialAuthV1::Password { password } if !password.is_empty() => {
            Ok(SshAuth::Password {
                password: Zeroizing::new(password),
            })
        }
        SshCredentialAuthV1::PublicKey {
            identity_pem,
            passphrase,
        } if !identity_pem.is_empty() => Ok(SshAuth::PublicKey {
            identity_pem: Zeroizing::new(identity_pem),
            passphrase: passphrase.map(Zeroizing::new),
        }),
        SshCredentialAuthV1::Password { .. } | SshCredentialAuthV1::PublicKey { .. } => {
            Err(terminal(SECRET_INVALID))
        }
    }
}

fn reject_unexpected_secret(
    secret_ref_id: Option<&restoreplane_domain::SecretRefId>,
) -> Result<(), ReplicaRepairResolutionErrorV2> {
    if secret_ref_id.is_some() {
        Err(terminal(SECRET_UNEXPECTED))
    } else {
        Ok(())
    }
}

fn repository_resolution_error(error: RepositoryError) -> ReplicaRepairResolutionErrorV2 {
    let retryable = matches!(
        error,
        RepositoryError::Sqlite(_) | RepositoryError::Store(_)
    );
    resolution(error.code(), retryable)
}

fn secret_resolution_error(error: SecretStoreError) -> ReplicaRepairResolutionErrorV2 {
    let retryable = matches!(
        error,
        SecretStoreError::Io(_) | SecretStoreError::Metadata(_)
    );
    resolution(error.code(), retryable)
}

fn io_resolution_error(error: std::io::Error) -> ReplicaRepairResolutionErrorV2 {
    let retryable = matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::NotFound
    );
    resolution(DESTINATION_INVALID, retryable)
}

fn terminal(code: &str) -> ReplicaRepairResolutionErrorV2 {
    resolution(code, false)
}

fn resolution(code: &str, retryable: bool) -> ReplicaRepairResolutionErrorV2 {
    let result = if retryable {
        ReplicaRepairResolutionErrorV2::retryable(code)
    } else {
        ReplicaRepairResolutionErrorV2::terminal(code)
    };
    result.expect("static repair resolution code is valid")
}
