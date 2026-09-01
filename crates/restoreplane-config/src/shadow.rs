use std::collections::BTreeMap;
use std::path::Path;

use restoreplane_core::{DestKind, EncField, Engine, ImmutabilityCapability, ObjectLockMode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::schema::{Config, Destination};
use crate::{validate, ConfigError};

#[derive(Debug, Error)]
pub enum ShadowImportError {
    #[error("SHADOW_IMPORT_CONFIG")]
    Config(#[from] ConfigError),
    #[error("SHADOW_IMPORT_CONFIG_INVALID")]
    ConfigInvalid { error_count: usize },
    #[error("SHADOW_IMPORT_SECRET_STORE")]
    SecretStore,
    #[error("SHADOW_IMPORT_SECRET_RECEIPT_INVALID")]
    SecretReceiptInvalid,
    #[error("SHADOW_IMPORT_CONNECTION_CREDENTIAL_CONFLICT")]
    ConnectionCredentialConflict,
    #[error("SHADOW_IMPORT_MODEL_INVALID")]
    ModelInvalid,
}

pub trait LegacySecretSink {
    fn import_encrypted_bundle(
        &mut self,
        purpose: &str,
        fields: &[&EncField],
    ) -> Result<ImportedSecretRef, ShadowImportError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedSecretRef {
    pub id: String,
    pub provider: String,
    pub provider_locator: String,
    pub fingerprint: String,
    pub purpose: String,
    pub version: i64,
}

impl ImportedSecretRef {
    pub fn external(
        id: impl Into<String>,
        provider_locator: impl Into<String>,
        fingerprint: impl Into<String>,
        purpose: impl Into<String>,
        version: i64,
    ) -> Self {
        Self {
            id: id.into(),
            provider: "external".to_owned(),
            provider_locator: provider_locator.into(),
            fingerprint: fingerprint.into(),
            purpose: purpose.into(),
            version,
        }
    }

    fn validate(&self, expected_purpose: &str) -> Result<(), ShadowImportError> {
        if self.id.is_empty()
            || !matches!(self.provider.as_str(), "windows_dpapi" | "external")
            || self.provider_locator.is_empty()
            || self.fingerprint.len() != 64
            || !self
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || self.purpose != expected_purpose
            || self.version <= 0
        {
            return Err(ShadowImportError::SecretReceiptInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalConnection {
    pub key: String,
    pub display_name: String,
    pub engine: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub source_resource_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalTarget {
    pub key: String,
    pub connection_key: String,
    pub engine_object_id: String,
    pub display_name: String,
    pub normalized_name: String,
    pub source_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalDestination {
    pub key: String,
    pub display_name: String,
    pub kind: String,
    pub endpoint_json: String,
    pub declared_capabilities_json: String,
    pub immutability_capability: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalPlan {
    pub key: String,
    pub revision_key: String,
    pub display_name: String,
    pub target_key: String,
    pub destination_keys: Vec<String>,
    pub enabled: bool,
    pub schedule_expression: String,
    pub retention_policy_json: String,
    pub verification_policy_json: String,
    pub execution_policy_json: String,
    pub content_hash_blake3: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalSecretBinding {
    pub owner_kind: String,
    pub owner_key: String,
    pub role: String,
    pub secret_ref_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalV1ShadowModel {
    pub source_checksum_blake3: String,
    pub connections: Vec<CanonicalConnection>,
    pub targets: Vec<CanonicalTarget>,
    pub destinations: Vec<CanonicalDestination>,
    pub plans: Vec<CanonicalPlan>,
    pub secret_refs: Vec<ImportedSecretRef>,
    pub secret_bindings: Vec<CanonicalSecretBinding>,
}

impl CanonicalV1ShadowModel {
    pub fn relation_fingerprint_blake3(&self) -> Result<String, ShadowImportError> {
        let canonical = serde_json::to_vec(self).map_err(|_| ShadowImportError::ModelInvalid)?;
        Ok(blake3::hash(&canonical).to_hex().to_string())
    }
}

pub fn load_v1_shadow_model(
    config_path: &Path,
    secret_sink: &mut dyn LegacySecretSink,
) -> Result<CanonicalV1ShadowModel, ShadowImportError> {
    let raw = std::fs::read(config_path).map_err(|source| {
        ShadowImportError::Config(ConfigError::Io(config_path.to_path_buf(), source))
    })?;
    let source_checksum_blake3 = blake3::hash(&raw).to_hex().to_string();
    let config = Config::load(config_path)?;
    if config.meta.schema_version != 1 {
        return Err(ShadowImportError::ModelInvalid);
    }
    if let Err(errors) = validate(&config) {
        return Err(ShadowImportError::ConfigInvalid {
            error_count: errors.len(),
        });
    }
    canonicalize_v1(config, source_checksum_blake3, secret_sink)
}

fn canonicalize_v1(
    config: Config,
    source_checksum_blake3: String,
    secret_sink: &mut dyn LegacySecretSink,
) -> Result<CanonicalV1ShadowModel, ShadowImportError> {
    let mut connections = BTreeMap::<String, CanonicalConnection>::new();
    let mut connection_credentials = BTreeMap::<String, EncField>::new();
    let mut targets = Vec::with_capacity(config.databases.len());
    let mut target_by_alias = BTreeMap::new();
    let mut secret_refs = BTreeMap::<String, ImportedSecretRef>::new();
    let mut secret_bindings = Vec::new();

    for database in &config.databases {
        let engine = engine_str(database.engine);
        let host = database.host.trim().to_ascii_lowercase();
        let user = database.user.trim().to_ascii_lowercase();
        let tunnel = database
            .tunnel
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let connection_key = format!(
            "connection|{engine}|{host}|{}|{user}|{tunnel}",
            database.port
        );
        if let Some(existing) = connection_credentials.get(&connection_key) {
            if existing != &database.password {
                return Err(ShadowImportError::ConnectionCredentialConflict);
            }
        } else {
            connection_credentials.insert(connection_key.clone(), database.password.clone());
            let purpose = format!("{connection_key}|database_auth");
            let receipt = secret_sink.import_encrypted_bundle(&purpose, &[&database.password])?;
            receipt.validate(&purpose)?;
            secret_bindings.push(CanonicalSecretBinding {
                owner_kind: "connection".to_owned(),
                owner_key: connection_key.clone(),
                role: "database_auth".to_owned(),
                secret_ref_id: receipt.id.clone(),
            });
            secret_refs.insert(receipt.id.clone(), receipt);
        }
        connections
            .entry(connection_key.clone())
            .or_insert_with(|| CanonicalConnection {
                key: connection_key.clone(),
                display_name: format!("{engine} {host}:{} ({user})", database.port),
                engine: engine.to_owned(),
                host: host.clone(),
                port: database.port,
                user,
                source_resource_key: format!("tcp:{engine}:{host}:{}", database.port),
            });

        let normalized_name = database.dbname.trim().to_ascii_lowercase();
        let target_key = format!("{connection_key}|database|{normalized_name}");
        target_by_alias.insert(database.alias.clone(), target_key.clone());
        targets.push(CanonicalTarget {
            key: target_key.clone(),
            connection_key,
            engine_object_id: normalized_name.clone(),
            display_name: database.alias.clone(),
            normalized_name,
            source_fingerprint: blake3::hash(target_key.as_bytes()).to_hex().to_string(),
        });
    }

    let mut destinations = Vec::with_capacity(config.destinations.len());
    let mut destination_by_name = BTreeMap::new();
    for destination in &config.destinations {
        let kind = destination_kind_str(destination.kind);
        let key = format!(
            "destination|{kind}|{}",
            destination.name.trim().to_ascii_lowercase()
        );
        destination_by_name.insert(destination.name.clone(), key.clone());
        let immutability = destination_immutability(destination);
        destinations.push(CanonicalDestination {
            key: key.clone(),
            display_name: destination.name.clone(),
            kind: kind.to_owned(),
            endpoint_json: destination_endpoint_json(destination)?,
            declared_capabilities_json: json!({
                "immutability": immutability,
            })
            .to_string(),
            immutability_capability: immutability.to_owned(),
        });
        import_destination_secrets(
            destination,
            &key,
            secret_sink,
            &mut secret_refs,
            &mut secret_bindings,
        )?;
    }

    let mut plans = Vec::with_capacity(config.jobs.len());
    for job in &config.jobs {
        let key = format!("plan|{}", job.name.trim().to_ascii_lowercase());
        let revision_key = format!("{key}|revision|1");
        let target_key = target_by_alias
            .get(&job.source)
            .cloned()
            .ok_or(ShadowImportError::ModelInvalid)?;
        let mut destination_keys = job
            .destinations
            .iter()
            .map(|name| {
                destination_by_name
                    .get(name)
                    .cloned()
                    .ok_or(ShadowImportError::ModelInvalid)
            })
            .collect::<Result<Vec<_>, _>>()?;
        destination_keys.sort();
        let retention_policy_json = json!({
            "daily": job.retention.daily,
            "weekly": job.retention.weekly,
            "monthly": job.retention.monthly,
        })
        .to_string();
        let verification_policy_json = json!({
            "requestedLevel": job.verify.level,
            "schedule": job.verify.schedule,
            "executionAuthority": "legacy_direct",
        })
        .to_string();
        let execution_policy_json = json!({
            "enabled": job.enabled,
            "requiredImmutability": immutability_str(job.required_immutability),
            "maxAttempts": config.policy.retry.attempts,
            "backoffSeconds": config.policy.retry.backoff_secs,
        })
        .to_string();
        let content = json!({
            "key": key,
            "revisionKey": revision_key,
            "targetKey": target_key,
            "destinationKeys": destination_keys,
            "scheduleExpression": job.schedule,
            "retention": serde_json::from_str::<serde_json::Value>(&retention_policy_json)
                .map_err(|_| ShadowImportError::ModelInvalid)?,
            "verification": serde_json::from_str::<serde_json::Value>(&verification_policy_json)
                .map_err(|_| ShadowImportError::ModelInvalid)?,
            "execution": serde_json::from_str::<serde_json::Value>(&execution_policy_json)
                .map_err(|_| ShadowImportError::ModelInvalid)?,
        });
        plans.push(CanonicalPlan {
            key,
            revision_key,
            display_name: job.name.clone(),
            target_key,
            destination_keys,
            enabled: job.enabled,
            schedule_expression: job.schedule.clone(),
            retention_policy_json,
            verification_policy_json,
            execution_policy_json,
            content_hash_blake3: blake3::hash(content.to_string().as_bytes())
                .to_hex()
                .to_string(),
        });
    }

    let mut model = CanonicalV1ShadowModel {
        source_checksum_blake3,
        connections: connections.into_values().collect(),
        targets,
        destinations,
        plans,
        secret_refs: secret_refs.into_values().collect(),
        secret_bindings,
    };
    model
        .targets
        .sort_by(|left, right| left.key.cmp(&right.key));
    model
        .destinations
        .sort_by(|left, right| left.key.cmp(&right.key));
    model.plans.sort_by(|left, right| left.key.cmp(&right.key));
    model.secret_bindings.sort_by(|left, right| {
        (&left.owner_kind, &left.owner_key, &left.role).cmp(&(
            &right.owner_kind,
            &right.owner_key,
            &right.role,
        ))
    });
    model.relation_fingerprint_blake3()?;
    Ok(model)
}

fn import_destination_secrets(
    destination: &Destination,
    destination_key: &str,
    secret_sink: &mut dyn LegacySecretSink,
    secret_refs: &mut BTreeMap<String, ImportedSecretRef>,
    bindings: &mut Vec<CanonicalSecretBinding>,
) -> Result<(), ShadowImportError> {
    let writer_fields = [
        destination.access_key_id.as_ref(),
        destination.secret_access_key.as_ref(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    if !writer_fields.is_empty() {
        import_binding(
            destination_key,
            "backup_writer",
            writer_fields,
            secret_sink,
            secret_refs,
            bindings,
        )?;
    }
    if let Some(reader) = &destination.reader {
        let reader_fields = [
            reader.access_key_id.as_ref(),
            reader.secret_access_key.as_ref(),
            reader.private_key_passphrase.as_ref(),
            reader.password.as_ref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        if !reader_fields.is_empty() {
            import_binding(
                destination_key,
                "restore_reader",
                reader_fields,
                secret_sink,
                secret_refs,
                bindings,
            )?;
        }
    }
    Ok(())
}

fn import_binding(
    owner_key: &str,
    role: &str,
    fields: Vec<&EncField>,
    secret_sink: &mut dyn LegacySecretSink,
    secret_refs: &mut BTreeMap<String, ImportedSecretRef>,
    bindings: &mut Vec<CanonicalSecretBinding>,
) -> Result<(), ShadowImportError> {
    let purpose = format!("{owner_key}|{role}");
    let receipt = secret_sink.import_encrypted_bundle(&purpose, &fields)?;
    receipt.validate(&purpose)?;
    bindings.push(CanonicalSecretBinding {
        owner_kind: "destination".to_owned(),
        owner_key: owner_key.to_owned(),
        role: role.to_owned(),
        secret_ref_id: receipt.id.clone(),
    });
    secret_refs.insert(receipt.id.clone(), receipt);
    Ok(())
}

fn destination_endpoint_json(destination: &Destination) -> Result<String, ShadowImportError> {
    let value = match destination.kind {
        DestKind::Local => json!({
            "path": destination.path,
        }),
        DestKind::S3 => json!({
            "endpoint": destination.endpoint,
            "region": destination.region,
            "bucket": destination.bucket,
            "prefix": destination.prefix,
        }),
        DestKind::Ssh => json!({
            "tunnel": destination.tunnel,
            "host": destination.host,
            "port": destination.port,
            "user": destination.user,
            "privateKeyPath": destination.private_key_path,
            "hostKeyFingerprint": destination.host_key_fingerprint,
            "path": destination.path,
        }),
    };
    serde_json::to_string(&value).map_err(|_| ShadowImportError::ModelInvalid)
}

const fn engine_str(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgresql => "postgresql",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Sqlserver => "sqlserver",
        Engine::Sqlite => "sqlite",
    }
}

const fn destination_kind_str(kind: DestKind) -> &'static str {
    match kind {
        DestKind::S3 => "s3",
        DestKind::Ssh => "ssh_sftp",
        DestKind::Local => "local",
    }
}

fn destination_immutability(destination: &Destination) -> &'static str {
    match destination.object_lock.as_ref().map(|lock| lock.mode) {
        Some(ObjectLockMode::Governance) => "governance",
        Some(ObjectLockMode::Compliance) => "compliance",
        Some(ObjectLockMode::None) | None => "none",
    }
}

const fn immutability_str(capability: ImmutabilityCapability) -> &'static str {
    match capability {
        ImmutabilityCapability::None => "none",
        ImmutabilityCapability::Governance => "governance",
        ImmutabilityCapability::Compliance => "compliance",
        ImmutabilityCapability::ServerAppendOnly => "server_append_only",
    }
}
