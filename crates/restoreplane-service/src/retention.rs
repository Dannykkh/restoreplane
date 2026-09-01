use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use restoreplane_catalog::{
    ConfigurationRepository, DestinationDeleteBindingV2, RepositoryError,
    RetentionPlannerRepository,
};
use restoreplane_dest_s3::{S3DeleteCfg, S3DeleteOperatorV2};
use restoreplane_dest_ssh::{SshAuth, SshDeleteCfg, SshDeleteOperatorV2};
use restoreplane_domain::RecoveryPointId;
use restoreplane_pipeline::{DestinationDeleteV2, LocalDeleteOperatorV2, RetentionDeletePlanV2};
use restoreplane_runtime::{RetentionDeleteOperatorResolverV2, RetentionDeleteResolutionErrorV2};
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::secrets::{SecretPurpose, SecretStoreError, ServiceSecretStore};

const BINDING_MISSING: &str = "RETENTION_DELETE_SECRET_BINDING_MISSING";
const DESTINATION_INVALID: &str = "RETENTION_DELETE_DESTINATION_INVALID";
const DESTINATION_UNSUPPORTED: &str = "RETENTION_DELETE_DESTINATION_UNSUPPORTED";
const SECRET_INVALID: &str = "RETENTION_DELETE_SECRET_INVALID";

#[derive(Clone)]
pub struct ServiceRetentionPlannerV2 {
    planner: RetentionPlannerRepository,
}

impl ServiceRetentionPlannerV2 {
    pub const fn new(planner: RetentionPlannerRepository) -> Self {
        Self { planner }
    }

    pub fn prepare_delete_plan(
        &self,
        recovery_point_id: &RecoveryPointId,
        destination_fingerprint: &str,
        now_ms: i64,
        grace_period_ms: i64,
    ) -> Result<RetentionDeletePlanV2, RepositoryError> {
        self.planner.prepare_delete_plan(
            recovery_point_id,
            destination_fingerprint,
            now_ms,
            grace_period_ms,
        )
    }
}

pub struct ServiceRetentionDeleteResolverV2 {
    configuration: ConfigurationRepository,
    secrets: Arc<ServiceSecretStore>,
}

impl ServiceRetentionDeleteResolverV2 {
    pub fn new(configuration: ConfigurationRepository, secrets: Arc<ServiceSecretStore>) -> Self {
        Self {
            configuration,
            secrets,
        }
    }

    fn resolve_binding(
        &self,
        destination_fingerprint: &str,
    ) -> Result<DestinationDeleteBindingV2, RetentionDeleteResolutionErrorV2> {
        self.configuration
            .destination_delete_binding(destination_fingerprint)
            .map_err(repository_resolution_error)
    }

    fn resolve_secret(
        &self,
        delete_secret_ref_id: Option<&str>,
    ) -> Result<crate::secrets::SecretBuffer, RetentionDeleteResolutionErrorV2> {
        let secret_ref_id = delete_secret_ref_id.ok_or_else(|| terminal(BINDING_MISSING));
        let secret_ref_id = restoreplane_domain::SecretRefId::from_str(secret_ref_id?)
            .map_err(|_| terminal(SECRET_INVALID))?;
        self.secrets
            .resolve(&secret_ref_id, SecretPurpose::RetentionDelete)
            .map_err(secret_resolution_error)
    }
}

impl RetentionDeleteOperatorResolverV2 for ServiceRetentionDeleteResolverV2 {
    fn resolve(
        &self,
        destination_fingerprint: &str,
        delete_secret_ref_id: Option<&str>,
    ) -> Result<Box<dyn DestinationDeleteV2>, RetentionDeleteResolutionErrorV2> {
        let binding = self.resolve_binding(destination_fingerprint)?;
        match binding.kind() {
            "local" => resolve_local(&binding),
            "s3" => {
                let secret = self.resolve_secret(delete_secret_ref_id)?;
                resolve_s3(&binding, secret.expose())
            }
            "ssh_sftp" => {
                let secret = self.resolve_secret(delete_secret_ref_id)?;
                resolve_ssh(&binding, secret.expose())
            }
            _ => Err(terminal(DESTINATION_UNSUPPORTED)),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LocalDeleteEndpointV1 {
    path: PathBuf,
}

fn resolve_local(
    binding: &DestinationDeleteBindingV2,
) -> Result<Box<dyn DestinationDeleteV2>, RetentionDeleteResolutionErrorV2> {
    let endpoint: LocalDeleteEndpointV1 =
        serde_json::from_str(binding.endpoint_json()).map_err(|_| terminal(DESTINATION_INVALID))?;
    if !endpoint.path.is_absolute() {
        return Err(terminal(DESTINATION_INVALID));
    }
    let root =
        std::fs::canonicalize(&endpoint.path).map_err(|error| io_resolution_error(&error))?;
    if !root.is_dir() {
        return Err(terminal(DESTINATION_INVALID));
    }
    Ok(Box::new(LocalDeleteOperatorV2::new(root)))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct S3DeleteEndpointV1 {
    #[serde(alias = "endpointUrl")]
    endpoint: Option<String>,
    region: String,
    bucket: String,
    prefix: Option<String>,
    #[serde(default, rename = "forcePathStyle")]
    _force_path_style: bool,
    #[serde(default, rename = "objectLockMode")]
    _object_lock_mode: Option<String>,
    #[serde(default, rename = "retentionDays")]
    _retention_days: u32,
    #[serde(default, rename = "minDwellDays")]
    _min_dwell_days: u32,
    #[serde(default, rename = "checksumAlgo")]
    _checksum_algo: Option<String>,
    #[serde(default, rename = "complianceOptIn")]
    _compliance_opt_in: bool,
    #[serde(default)]
    _provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct S3DeleteSecretV1 {
    schema_version: u32,
    access_key_id: String,
    secret_access_key: String,
}

fn resolve_s3(
    binding: &DestinationDeleteBindingV2,
    secret: &[u8],
) -> Result<Box<dyn DestinationDeleteV2>, RetentionDeleteResolutionErrorV2> {
    let endpoint: S3DeleteEndpointV1 =
        serde_json::from_str(binding.endpoint_json()).map_err(|_| terminal(DESTINATION_INVALID))?;
    let secret: S3DeleteSecretV1 =
        serde_json::from_slice(secret).map_err(|_| terminal(SECRET_INVALID))?;
    if secret.schema_version != 1 {
        return Err(terminal(SECRET_INVALID));
    }
    let operator = S3DeleteOperatorV2::new(S3DeleteCfg {
        endpoint: endpoint.endpoint.unwrap_or_default(),
        region: endpoint.region,
        bucket: endpoint.bucket,
        prefix: endpoint.prefix.unwrap_or_default(),
        access_key_id: secret.access_key_id,
        secret_access_key: secret.secret_access_key,
    })
    .map_err(|_| terminal(DESTINATION_INVALID))?;
    Ok(Box::new(operator))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SshDeleteEndpointV1 {
    host: String,
    port: u16,
    #[serde(alias = "username")]
    user: String,
    #[serde(alias = "hostKeySha256")]
    host_key_fingerprint: String,
    path: String,
    #[serde(default, rename = "appendOnly")]
    _append_only: bool,
    #[serde(default)]
    private_key_path: Option<String>,
    #[serde(default)]
    tunnel: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SshDeleteSecretV1 {
    schema_version: u32,
    auth: SshDeleteAuthV1,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SshDeleteAuthV1 {
    Password {
        password: String,
    },
    PublicKey {
        identity_pem: String,
        passphrase: Option<String>,
    },
}

fn resolve_ssh(
    binding: &DestinationDeleteBindingV2,
    secret: &[u8],
) -> Result<Box<dyn DestinationDeleteV2>, RetentionDeleteResolutionErrorV2> {
    let endpoint: SshDeleteEndpointV1 =
        serde_json::from_str(binding.endpoint_json()).map_err(|_| terminal(DESTINATION_INVALID))?;
    let secret: SshDeleteSecretV1 =
        serde_json::from_slice(secret).map_err(|_| terminal(SECRET_INVALID))?;
    if secret.schema_version != 1 {
        return Err(terminal(SECRET_INVALID));
    }
    let auth = match secret.auth {
        SshDeleteAuthV1::Password { password } => SshAuth::Password {
            password: Zeroizing::new(password),
        },
        SshDeleteAuthV1::PublicKey {
            identity_pem,
            passphrase,
        } => SshAuth::PublicKey {
            identity_pem: Zeroizing::new(identity_pem),
            passphrase: passphrase.map(Zeroizing::new),
        },
    };
    let _ = endpoint.private_key_path;
    let _ = endpoint.tunnel;
    let operator = SshDeleteOperatorV2::new(SshDeleteCfg {
        host: endpoint.host,
        port: endpoint.port,
        user: endpoint.user,
        host_key_fingerprint: endpoint.host_key_fingerprint,
        auth,
        remote_dir: endpoint.path,
    })
    .map_err(|_| terminal(DESTINATION_INVALID))?;
    Ok(Box::new(operator))
}

fn repository_resolution_error(error: RepositoryError) -> RetentionDeleteResolutionErrorV2 {
    let retryable = matches!(
        error,
        RepositoryError::Sqlite(_) | RepositoryError::Store(_)
    );
    resolution(error.code(), retryable)
}

fn secret_resolution_error(error: SecretStoreError) -> RetentionDeleteResolutionErrorV2 {
    let retryable = matches!(
        error,
        SecretStoreError::Io(_) | SecretStoreError::Metadata(_)
    );
    resolution(error.code(), retryable)
}

fn io_resolution_error(error: &std::io::Error) -> RetentionDeleteResolutionErrorV2 {
    let retryable = matches!(
        error.kind(),
        std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::WouldBlock
    );
    resolution(DESTINATION_INVALID, retryable)
}

fn terminal(code: &str) -> RetentionDeleteResolutionErrorV2 {
    resolution(code, false)
}

fn resolution(code: &str, retryable: bool) -> RetentionDeleteResolutionErrorV2 {
    let result = if retryable {
        RetentionDeleteResolutionErrorV2::retryable(code)
    } else {
        RetentionDeleteResolutionErrorV2::terminal(code)
    };
    result.expect("static retention resolution code is valid")
}
