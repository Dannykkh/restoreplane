use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ed25519_dalek::VerifyingKey;
use restoreplane_catalog::rusqlite::OptionalExtension;
use restoreplane_catalog::{open_reader_conn, CompleteRecoveryPointSpec, OperationalStore};
use restoreplane_core::{ArtifactPartRole, Engine};
use restoreplane_domain::{ArtifactReplicaId, DestinationId, RecoveryPointId, SecretRefId};
use restoreplane_pipeline::chain::{NONCE_PREFIX, STREAM_CHUNK};
use restoreplane_pipeline::{
    Destination, DestinationReaderV2, LocalDest, ReaderStatusV2, ReplicaPublishError,
    ReplicaPublishOutcomeV2, ReplicaRequirementV2, SignedSetManifestV2, UploadReceipt,
    MAX_SET_MANIFEST_BYTES_V2,
};
use restoreplane_secrets::{unwrap_dek_with_kek, Dek, WrapSet};

use crate::operations::destination::resolve_recovery_reader;
use crate::secrets::{SecretPurpose, ServiceSecretStore};

const TAG_LEN: usize = 16;
const CIPHER_CHUNK: usize = STREAM_CHUNK + TAG_LEN;
const MAX_RECOVERY_PARTS: usize = 64;
const MAX_RECOVERY_REPLICAS: usize = 1_024;
const MAX_PART_BYTES: u64 = 8 * 1_024 * 1_024 * 1_024 * 1_024;
const MATERIALIZATION_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

type MaterializationHeartbeat =
    dyn Fn() -> Result<(), RecoveryPointReadError> + Send + Sync + 'static;

#[derive(Clone)]
pub(crate) struct RecoveryMaterializationControl {
    stopping: Arc<AtomicBool>,
    deadline: Instant,
    heartbeat: Arc<MaterializationHeartbeat>,
    next_heartbeat: Arc<Mutex<Instant>>,
}

impl RecoveryMaterializationControl {
    pub(crate) fn new(
        stopping: Arc<AtomicBool>,
        deadline: Instant,
        heartbeat: Arc<MaterializationHeartbeat>,
    ) -> Self {
        Self {
            stopping,
            deadline,
            heartbeat,
            next_heartbeat: Arc::new(Mutex::new(Instant::now())),
        }
    }

    fn check(&self) -> Result<(), RecoveryPointReadError> {
        if self.stopping.load(Ordering::Acquire) {
            Err(RecoveryPointReadError::retryable(
                "RECOVERY_SERVICE_STOPPING",
            ))
        } else if Instant::now() >= self.deadline {
            Err(RecoveryPointReadError::terminal(
                "RECOVERY_EXECUTION_TIMEOUT",
            ))
        } else {
            let now = Instant::now();
            let mut next_heartbeat = self.next_heartbeat.lock().map_err(|_| {
                RecoveryPointReadError::terminal("RECOVERY_MATERIALIZATION_STATE_INVALID")
            })?;
            if now >= *next_heartbeat {
                (self.heartbeat)()?;
                *next_heartbeat = now
                    .checked_add(MATERIALIZATION_HEARTBEAT_INTERVAL)
                    .ok_or_else(|| {
                        RecoveryPointReadError::terminal("RECOVERY_EXECUTION_TIMEOUT")
                    })?;
            }
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RecoveryPointReadError {
    code: &'static str,
    retryable: bool,
}

impl RecoveryPointReadError {
    pub(crate) const fn terminal(code: &'static str) -> Self {
        Self {
            code,
            retryable: false,
        }
    }

    pub(crate) const fn retryable(code: &'static str) -> Self {
        Self {
            code,
            retryable: true,
        }
    }

    pub(crate) const fn code(self) -> &'static str {
        self.code
    }

    pub(crate) const fn is_retryable(self) -> bool {
        self.retryable
    }
}

pub(crate) struct RecoveryPointResolver {
    store: OperationalStore,
    catalog_path: PathBuf,
    secrets: Arc<ServiceSecretStore>,
    trusted_signing_keys: BTreeMap<String, VerifyingKey>,
    tool_root: PathBuf,
    staging_root: PathBuf,
}

impl RecoveryPointResolver {
    pub(crate) fn new(
        store: OperationalStore,
        catalog_path: PathBuf,
        secrets: Arc<ServiceSecretStore>,
        trusted_signing_keys: BTreeMap<String, VerifyingKey>,
        tool_root: PathBuf,
        staging_root: PathBuf,
    ) -> Result<Self, RecoveryPointReadError> {
        if !catalog_path.is_absolute() || !tool_root.is_absolute() || !staging_root.is_absolute() {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_RUNTIME_ROOT_INVALID",
            ));
        }
        fs::create_dir_all(&staging_root)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_UNAVAILABLE"))?;
        Ok(Self {
            store,
            catalog_path,
            secrets,
            trusted_signing_keys,
            tool_root,
            staging_root,
        })
    }

    pub(crate) async fn resolve(
        &self,
        recovery_point_id: &RecoveryPointId,
        source_replica_id: &str,
        observed_at_ms: i64,
    ) -> Result<VerifiedRecoveryPoint, RecoveryPointReadError> {
        if source_replica_id.is_empty() || observed_at_ms < 0 {
            return Err(RecoveryPointReadError::terminal("RECOVERY_BINDING_INVALID"));
        }
        let catalog = self.catalog_evidence(recovery_point_id, source_replica_id)?;
        let destination_id = parse_destination_fingerprint(
            &catalog.selected.destination_fingerprint,
            &catalog.selected.destination_kind,
        )?;
        let binding = self
            .store
            .configuration()
            .destination_probe_binding(&destination_id)
            .map_err(|_| {
                RecoveryPointReadError::terminal("RECOVERY_DESTINATION_BINDING_INVALID")
            })?;
        if binding.fingerprint() != catalog.selected.destination_fingerprint
            || binding.kind() != catalog.selected.destination_kind
        {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_DESTINATION_BINDING_INVALID",
            ));
        }
        let reader = resolve_recovery_reader(&binding, Some(self.secrets.as_ref()))
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_READER_BINDING_INVALID"))?;
        let manifest_bytes = reader
            .read_bytes_bounded(
                &catalog.selected.manifest_object_key,
                MAX_SET_MANIFEST_BYTES_V2,
            )
            .await
            .map_err(|_| RecoveryPointReadError::retryable("RECOVERY_MANIFEST_READ_FAILED"))?;
        if u64::try_from(manifest_bytes.len()).ok() != Some(catalog.selected.manifest_size_bytes)
            || blake3::hash(&manifest_bytes).to_hex().as_str() != catalog.manifest_checksum_blake3
        {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_MANIFEST_COMMITMENT_MISMATCH",
            ));
        }
        let signed: SignedSetManifestV2 = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_MANIFEST_INVALID"))?;
        signed
            .manifest
            .validate()
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_MANIFEST_INVALID"))?;
        let canonical = signed
            .to_json()
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_MANIFEST_INVALID"))?;
        if canonical.as_bytes() != manifest_bytes {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_MANIFEST_NOT_CANONICAL",
            ));
        }
        let verifying_key = self
            .trusted_signing_keys
            .get(&catalog.signing_key_id)
            .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SIGNING_KEY_UNTRUSTED"))?;
        if signed.manifest.signing_key_id != catalog.signing_key_id {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_SIGNING_KEY_MISMATCH",
            ));
        }
        signed
            .verify(verifying_key)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_MANIFEST_SIGNATURE_INVALID"))?;
        if signed.manifest.recovery_point_id != recovery_point_id.as_uuid()
            || engine_name(signed.manifest.engine) != catalog.engine
            || signed.manifest.committed_at_ms > observed_at_ms
        {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_MANIFEST_IDENTITY_MISMATCH",
            ));
        }
        require_exact_scope_evidence(&signed)?;
        let scope_tables = self.catalog_scope_tables(recovery_point_id, &signed)?;
        validate_catalog_commitments(&signed, &catalog, verifying_key, observed_at_ms)?;
        validate_historical_dump_identity(&signed)?;
        let kek = self.resolve_kek(&signed)?;

        Ok(VerifiedRecoveryPoint {
            signed,
            reader,
            source_replica_id: source_replica_id.to_owned(),
            destination_fingerprint: catalog.selected.destination_fingerprint,
            selected_payloads: catalog.selected.payloads,
            scope_tables,
            kek,
            staging_root: self.staging_root.clone(),
        })
    }

    pub(crate) fn trusted_tool(
        &self,
        tool_name: &str,
    ) -> Result<TrustedNativeTool, RecoveryPointReadError> {
        trusted_tool(&self.catalog_path, &self.tool_root, tool_name, None, None)
    }

    fn resolve_kek(&self, signed: &SignedSetManifestV2) -> Result<Dek, RecoveryPointReadError> {
        let key_ref = signed
            .manifest
            .options
            .get("encryptionKeyRef")
            .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_KEY_REF_MISSING"))?;
        let key_ref = SecretRefId::from_str(key_ref)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_KEY_REF_INVALID"))?;
        let key = self
            .secrets
            .resolve(&key_ref, SecretPurpose::EncryptionKey)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_KEY_UNAVAILABLE"))?;
        let bytes: [u8; 32] = key
            .expose()
            .try_into()
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_KEY_INVALID"))?;
        Ok(Dek::from_bytes(bytes))
    }

    fn catalog_evidence(
        &self,
        recovery_point_id: &RecoveryPointId,
        source_replica_id: &str,
    ) -> Result<CatalogEvidence, RecoveryPointReadError> {
        let connection = open_reader_conn(&self.catalog_path)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
        let recovery: Option<(String, String, String, String, String)> = connection
            .query_row(
                "SELECT engine, state, signature_status,
                        set_manifest_checksum_blake3, signing_key_id
                 FROM catalog_recovery_points WHERE id = ?1",
                [recovery_point_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
        let Some((engine, state, signature_status, manifest_checksum_blake3, signing_key_id)) =
            recovery
        else {
            return Err(RecoveryPointReadError::terminal("RECOVERY_POINT_NOT_FOUND"));
        };
        if state != "complete" || signature_status != "valid" {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_POINT_NOT_ELIGIBLE",
            ));
        }

        let mut statement = connection
            .prepare(
                "SELECT id, destination_fingerprint, destination_kind, requirement,
                        state, reader_status, immutability_capability
                 FROM catalog_artifact_replicas
                 WHERE recovery_point_id = ?1 ORDER BY destination_fingerprint",
            )
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
        let rows = statement
            .query_map([recovery_point_id.as_str()], |row| {
                Ok(CatalogReplica {
                    id: row.get(0)?,
                    destination_fingerprint: row.get(1)?,
                    destination_kind: row.get(2)?,
                    requirement: row.get(3)?,
                    state: row.get(4)?,
                    reader_status: row.get(5)?,
                    immutability_capability: row.get(6)?,
                    manifest_object_key: String::new(),
                    manifest_size_bytes: 0,
                    payloads: BTreeMap::new(),
                })
            })
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
        drop(statement);
        if rows.is_empty() || rows.len() > MAX_RECOVERY_REPLICAS {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_REPLICA_SET_INVALID",
            ));
        }
        let mut replicas = BTreeMap::new();
        for mut replica in rows {
            let manifest: Option<(String, Option<String>, Option<i64>, String)> = connection
                .query_row(
                    "SELECT object_key, checksum_blake3, size_bytes, state
                     FROM catalog_stored_objects
                     WHERE recovery_point_id = ?1 AND artifact_replica_id = ?2
                       AND object_role = 'set_manifest'",
                    [recovery_point_id.as_str(), replica.id.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
            if let Some((key, checksum, size, object_state)) = manifest {
                if checksum.as_deref() != Some(manifest_checksum_blake3.as_str())
                    || object_state != "present"
                {
                    return Err(RecoveryPointReadError::terminal(
                        "RECOVERY_MANIFEST_COMMITMENT_MISMATCH",
                    ));
                }
                replica.manifest_object_key = key;
                replica.manifest_size_bytes = u64::try_from(size.unwrap_or(-1)).map_err(|_| {
                    RecoveryPointReadError::terminal("RECOVERY_MANIFEST_COMMITMENT_MISMATCH")
                })?;
            }
            let mut object_statement = connection
                .prepare(
                    "SELECT artifact_part_id, object_key, object_version,
                            checksum_blake3, size_bytes, state
                     FROM catalog_stored_objects
                     WHERE recovery_point_id = ?1 AND artifact_replica_id = ?2
                       AND object_role = 'payload'
                     ORDER BY artifact_part_id",
                )
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
            let objects = object_statement
                .query_map([recovery_point_id.as_str(), replica.id.as_str()], |row| {
                    Ok(CatalogPayload {
                        part_id: row.get(0)?,
                        object_key: row.get(1)?,
                        object_version: row.get(2)?,
                        checksum_blake3: row.get(3)?,
                        size_bytes: row.get(4)?,
                        state: row.get(5)?,
                    })
                })
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
            for payload in objects {
                let part_id = payload.part_id.clone();
                if replica.payloads.insert(part_id, payload).is_some() {
                    return Err(RecoveryPointReadError::terminal(
                        "RECOVERY_PAYLOAD_COMMITMENT_MISMATCH",
                    ));
                }
            }
            if replicas.insert(replica.id.clone(), replica).is_some() {
                return Err(RecoveryPointReadError::terminal(
                    "RECOVERY_REPLICA_SET_INVALID",
                ));
            }
        }
        let selected = replicas.get(source_replica_id).cloned().ok_or_else(|| {
            RecoveryPointReadError::terminal("RECOVERY_SELECTED_REPLICA_NOT_FOUND")
        })?;
        if selected.state != "committed"
            || selected.requirement != "required"
            || selected.reader_status != "eligible"
            || selected.manifest_object_key.is_empty()
        {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_SELECTED_REPLICA_NOT_ELIGIBLE",
            ));
        }
        Ok(CatalogEvidence {
            engine,
            manifest_checksum_blake3,
            signing_key_id,
            selected,
            replicas,
        })
    }

    fn catalog_scope_tables(
        &self,
        recovery_point_id: &RecoveryPointId,
        signed: &SignedSetManifestV2,
    ) -> Result<Option<BTreeSet<(String, String)>>, RecoveryPointReadError> {
        let connection = open_reader_conn(&self.catalog_path)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
        let row: Option<(String, i64, String, String, String)> = connection
            .query_row(
                "SELECT scope.scope_mode, scope.resolved_object_count,
                        scope.scope_snapshot_json, scope.scope_snapshot_hash_blake3,
                        scope.backup_scope_id
                 FROM catalog_recovery_points AS recovery
                 JOIN backup_run_scopes AS scope
                   ON scope.backup_run_id = recovery.origin_backup_run_id
                 WHERE recovery.id = ?1",
                [recovery_point_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_READ_FAILED"))?;
        let (mode, object_count, snapshot, snapshot_hash, scope_id) = row.ok_or_else(|| {
            RecoveryPointReadError::terminal("RECOVERY_SCOPE_CATALOG_EVIDENCE_MISSING")
        })?;
        resolve_catalog_scope_tables(
            &signed.manifest.target_id.to_string(),
            &signed.manifest.options,
            &CatalogScopeEvidence {
                mode,
                object_count,
                snapshot,
                snapshot_hash,
                scope_id,
            },
        )
    }
}

struct CatalogScopeEvidence {
    mode: String,
    object_count: i64,
    snapshot: String,
    snapshot_hash: String,
    scope_id: String,
}

fn resolve_catalog_scope_tables(
    manifest_target_id: &str,
    options: &BTreeMap<String, String>,
    catalog: &CatalogScopeEvidence,
) -> Result<Option<BTreeSet<(String, String)>>, RecoveryPointReadError> {
    let manifest_mode = options
        .get("backupScopeMode")
        .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
    let manifest_count = options
        .get("backupScopeObjectCount")
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
    let manifest_hash = options
        .get("backupScopeSnapshotHashBlake3")
        .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
    if catalog.scope_id != manifest_target_id
        || &catalog.mode != manifest_mode
        || catalog.object_count != manifest_count
        || catalog.snapshot_hash != *manifest_hash
        || blake3::hash(catalog.snapshot.as_bytes()).to_hex().as_str() != catalog.snapshot_hash
    {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_SCOPE_CATALOG_EVIDENCE_MISMATCH",
        ));
    }
    let catalog_snapshot: serde_json::Value = serde_json::from_str(&catalog.snapshot)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
    if serde_json::to_string(&catalog_snapshot).ok().as_deref() != Some(catalog.snapshot.as_str())
        || catalog_snapshot
            .get("mode")
            .and_then(serde_json::Value::as_str)
            != Some(catalog.mode.as_str())
    {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_SCOPE_EVIDENCE_INVALID",
        ));
    }
    let objects = catalog_snapshot
        .get("objects")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
    if i64::try_from(objects.len()).ok() != Some(catalog.object_count) {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_SCOPE_EVIDENCE_INVALID",
        ));
    }
    let manifest_snapshot = options
        .get("backupScopeSnapshot")
        .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
    let manifest_snapshot: serde_json::Value = serde_json::from_str(manifest_snapshot)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
    let manifest_object = manifest_snapshot
        .as_object()
        .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
    let snapshot_matches = if manifest_object.len() == 1 {
        manifest_object
            .get("blake3")
            .and_then(serde_json::Value::as_str)
            == Some(catalog.snapshot_hash.as_str())
    } else {
        manifest_snapshot == catalog_snapshot
    };
    if !snapshot_matches {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_SCOPE_CATALOG_EVIDENCE_MISMATCH",
        ));
    }
    if catalog.mode == "whole_database" {
        return Ok(None);
    }
    if catalog.mode != "explicit_tables" {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_SCOPE_EVIDENCE_INVALID",
        ));
    }
    parse_scope_table_objects(objects).map(Some)
}

pub(crate) struct VerifiedRecoveryPoint {
    signed: SignedSetManifestV2,
    reader: Arc<dyn DestinationReaderV2>,
    source_replica_id: String,
    destination_fingerprint: String,
    selected_payloads: BTreeMap<String, CatalogPayload>,
    scope_tables: Option<BTreeSet<(String, String)>>,
    kek: Dek,
    staging_root: PathBuf,
}

impl VerifiedRecoveryPoint {
    pub(crate) const fn signed(&self) -> &SignedSetManifestV2 {
        &self.signed
    }

    pub(crate) fn destination_fingerprint(&self) -> &str {
        &self.destination_fingerprint
    }

    pub(crate) fn scope_tables(&self) -> Option<&BTreeSet<(String, String)>> {
        self.scope_tables.as_ref()
    }

    pub(crate) fn required_staging_bytes(&self) -> Result<i64, RecoveryPointReadError> {
        self.signed
            .manifest
            .parts
            .iter()
            .try_fold(0_u64, |total, part| {
                part.ciphertext_bytes
                    .checked_mul(2)
                    .and_then(|cipher_and_compressed| {
                        cipher_and_compressed.checked_add(part.plaintext_bytes)
                    })
                    .and_then(|part_bytes| total.checked_add(part_bytes))
            })
            .and_then(|bytes| i64::try_from(bytes).ok())
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_STAGING_SIZE_INVALID"))
    }

    pub(crate) fn staging_path(&self, stage_key: &str) -> Result<PathBuf, RecoveryPointReadError> {
        validate_stage_key(stage_key)?;
        Ok(self.staging_root.join(stage_key))
    }

    pub(crate) async fn materialize(
        &self,
        control: &RecoveryMaterializationControl,
        stage_key: &str,
    ) -> Result<MaterializedRecoveryPoint, RecoveryPointReadError> {
        control.check()?;
        if self.signed.manifest.parts.is_empty()
            || self.signed.manifest.parts.len() > MAX_RECOVERY_PARTS
        {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_PART_SET_INVALID",
            ));
        }
        let stage = ManagedStage::create(&self.staging_root, stage_key)?;
        let local = LocalDest::new("recovery-materialization", stage.path());
        let mut parts = Vec::with_capacity(self.signed.manifest.parts.len());
        for part in &self.signed.manifest.parts {
            control.check()?;
            if part.compression_format != "zstd"
                || !matches!(
                    part.encryption_format.as_str(),
                    "chacha20poly1305_stream" | "chacha20poly1305-stream"
                )
                || part.ciphertext_bytes == 0
                || part.plaintext_bytes == 0
                || part.ciphertext_bytes > MAX_PART_BYTES
                || part.plaintext_bytes > MAX_PART_BYTES
            {
                return Err(RecoveryPointReadError::terminal(
                    "RECOVERY_ENCODING_UNSUPPORTED",
                ));
            }
            let part_id = part.part_id.to_string();
            let committed = self.selected_payloads.get(&part_id).ok_or_else(|| {
                RecoveryPointReadError::terminal("RECOVERY_PAYLOAD_COMMITMENT_MISSING")
            })?;
            if committed.state != "present"
                || committed.object_key != part.object_key
                || committed.checksum_blake3.as_deref() != Some(part.ciphertext_blake3.as_str())
                || u64::try_from(committed.size_bytes.unwrap_or(-1)).ok()
                    != Some(part.ciphertext_bytes)
            {
                return Err(RecoveryPointReadError::terminal(
                    "RECOVERY_PAYLOAD_COMMITMENT_MISMATCH",
                ));
            }
            let cipher_key = format!("cipher/{}.bin", part.part_id);
            let mut writer = local
                .writer(&cipher_key)
                .await
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
            let copied = match self
                .reader
                .copy_object_to(&part.object_key, &mut writer, part.ciphertext_bytes)
                .await
            {
                Ok(copied) => copied,
                Err(_) => {
                    writer.abort().await;
                    return Err(RecoveryPointReadError::retryable(
                        "RECOVERY_PAYLOAD_READ_FAILED",
                    ));
                }
            };
            control.check()?;
            if copied.bytes != part.ciphertext_bytes || copied.blake3 != part.ciphertext_blake3 {
                writer.abort().await;
                return Err(RecoveryPointReadError::terminal(
                    "RECOVERY_CIPHERTEXT_MISMATCH",
                ));
            }
            writer
                .commit()
                .await
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
            let wrap = self.part_wrap(part.part_id, part.dek_wrap_generation)?;
            let dek = unwrap_dek_with_kek(&self.kek, &wrap)
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_DEK_UNWRAP_FAILED"))?;
            let cipher_path = stage.path().join(&cipher_key);
            let compressed_path = stage
                .path()
                .join(format!("compressed/{}.zst", part.part_id));
            let plaintext_path = stage.path().join(format!("plain/{}.dump", part.part_id));
            if let Some(parent) = compressed_path.parent() {
                fs::create_dir_all(parent).map_err(|_| {
                    RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED")
                })?;
            }
            if let Some(parent) = plaintext_path.parent() {
                fs::create_dir_all(parent).map_err(|_| {
                    RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED")
                })?;
            }
            decrypt_to_compressed(&cipher_path, &compressed_path, &dek, control)?;
            let (plaintext_bytes, plaintext_blake3) = decompress_to_plaintext(
                &compressed_path,
                &plaintext_path,
                part.plaintext_bytes,
                control,
            )?;
            if plaintext_bytes != part.plaintext_bytes || plaintext_blake3 != part.plaintext_blake3
            {
                return Err(RecoveryPointReadError::terminal(
                    "RECOVERY_PLAINTEXT_MISMATCH",
                ));
            }
            parts.push(MaterializedPart {
                part_id,
                role: part.role,
                dump_format: part.dump_format.clone(),
                path: plaintext_path,
                plaintext_bytes,
                plaintext_blake3,
            });
        }
        Ok(MaterializedRecoveryPoint {
            _stage: stage,
            parts,
            source_tool_version: self.signed.manifest.tool.version.clone(),
            recovery_point_id: self.signed.manifest.recovery_point_id.to_string(),
            source_replica_id: self.source_replica_id.clone(),
            destination_fingerprint: self.destination_fingerprint.clone(),
        })
    }

    fn part_wrap(
        &self,
        part_id: uuid::Uuid,
        expected_generation: u32,
    ) -> Result<WrapSet, RecoveryPointReadError> {
        let value = self
            .signed
            .manifest
            .options
            .get(&format!("dekWrap.{part_id}"))
            .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_DEK_WRAP_MISSING"))?;
        let wrap: WrapSet = serde_json::from_str(value)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_DEK_WRAP_INVALID"))?;
        if wrap.dek_wrap_gen != expected_generation {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_DEK_WRAP_INVALID",
            ));
        }
        Ok(wrap)
    }
}

pub(crate) struct MaterializedRecoveryPoint {
    _stage: ManagedStage,
    parts: Vec<MaterializedPart>,
    source_tool_version: String,
    recovery_point_id: String,
    source_replica_id: String,
    destination_fingerprint: String,
}

impl MaterializedRecoveryPoint {
    pub(crate) fn parts(&self) -> &[MaterializedPart] {
        &self.parts
    }

    pub(crate) fn source_tool_version(&self) -> &str {
        &self.source_tool_version
    }

    pub(crate) fn database_part(&self) -> Result<&MaterializedPart, RecoveryPointReadError> {
        let mut parts = self
            .parts
            .iter()
            .filter(|part| part.role == ArtifactPartRole::DatabaseDump);
        let part = parts
            .next()
            .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_DATABASE_PART_MISSING"))?;
        if parts.next().is_some() {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_DATABASE_PART_AMBIGUOUS",
            ));
        }
        Ok(part)
    }

    pub(crate) fn evidence(&self) -> serde_json::Value {
        serde_json::json!({
            "recoveryPointId": self.recovery_point_id,
            "sourceReplicaId": self.source_replica_id,
            "destinationFingerprint": self.destination_fingerprint,
            "partCount": self.parts.len(),
            "parts": self.parts.iter().map(|part| serde_json::json!({
                "partId": part.part_id,
                "role": role_name(part.role),
                "plaintextBytes": part.plaintext_bytes,
                "plaintextBlake3": part.plaintext_blake3,
            })).collect::<Vec<_>>(),
        })
    }
}

pub(crate) struct MaterializedPart {
    part_id: String,
    role: ArtifactPartRole,
    dump_format: String,
    path: PathBuf,
    plaintext_bytes: u64,
    plaintext_blake3: String,
}

impl MaterializedPart {
    pub(crate) fn dump_format(&self) -> &str {
        &self.dump_format
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Clone)]
pub(crate) struct TrustedNativeTool {
    path: PathBuf,
    sha256: [u8; 32],
    version: String,
}

impl TrustedNativeTool {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) const fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    pub(crate) fn version(&self) -> &str {
        &self.version
    }
}

#[derive(Clone)]
struct CatalogReplica {
    id: String,
    destination_fingerprint: String,
    destination_kind: String,
    requirement: String,
    state: String,
    reader_status: String,
    immutability_capability: String,
    manifest_object_key: String,
    manifest_size_bytes: u64,
    payloads: BTreeMap<String, CatalogPayload>,
}

#[derive(Clone)]
struct CatalogPayload {
    part_id: String,
    object_key: String,
    object_version: Option<String>,
    checksum_blake3: Option<String>,
    size_bytes: Option<i64>,
    state: String,
}

struct CatalogEvidence {
    engine: String,
    manifest_checksum_blake3: String,
    signing_key_id: String,
    selected: CatalogReplica,
    replicas: BTreeMap<String, CatalogReplica>,
}

struct ManagedStage(PathBuf);

impl ManagedStage {
    fn create(root: &Path, stage_key: &str) -> Result<Self, RecoveryPointReadError> {
        validate_stage_key(stage_key)?;
        let root = fs::canonicalize(root)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_UNAVAILABLE"))?;
        let path = root.join(stage_key);
        fs::create_dir(&path)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_UNAVAILABLE"))?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

fn validate_stage_key(stage_key: &str) -> Result<(), RecoveryPointReadError> {
    if stage_key.is_empty()
        || stage_key.len() > 256
        || !stage_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_STAGING_KEY_INVALID",
        ));
    }
    Ok(())
}

impl Drop for ManagedStage {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            tracing::error!(
                error = %error,
                "managed recovery staging cleanup failed"
            );
        }
    }
}

fn validate_catalog_commitments(
    signed: &SignedSetManifestV2,
    catalog: &CatalogEvidence,
    verifying_key: &VerifyingKey,
    observed_at_ms: i64,
) -> Result<(), RecoveryPointReadError> {
    if signed.manifest.replicas.len() != catalog.replicas.len()
        || signed.manifest.parts.len() > MAX_RECOVERY_PARTS
    {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_CATALOG_COMMITMENT_MISMATCH",
        ));
    }
    let mut outcomes = Vec::with_capacity(signed.manifest.replicas.len());
    for commitment in &signed.manifest.replicas {
        let replica = catalog
            .replicas
            .values()
            .find(|replica| replica.destination_fingerprint == commitment.destination_fingerprint)
            .ok_or_else(|| {
                RecoveryPointReadError::terminal("RECOVERY_CATALOG_COMMITMENT_MISMATCH")
            })?;
        let expected_id = ArtifactReplicaId::from_stable_name(
            signed.manifest.recovery_point_id,
            format!("replica\0{}", commitment.destination_fingerprint).as_bytes(),
        );
        if replica.id != expected_id.as_str()
            || replica.destination_kind != commitment.destination_kind
            || replica.requirement != requirement_name(commitment.requirement)
            || replica.reader_status != reader_status_name(commitment.reader_status)
            || replica.immutability_capability != commitment.immutability_capability
            || (!replica.manifest_object_key.is_empty()
                && replica.manifest_object_key != commitment.manifest_object_key)
        {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_CATALOG_COMMITMENT_MISMATCH",
            ));
        }
        let complete = replica.state == "committed"
            && replica.reader_status == "eligible"
            && !replica.manifest_object_key.is_empty()
            && signed.manifest.parts.iter().all(|part| {
                replica
                    .payloads
                    .get(&part.part_id.to_string())
                    .is_some_and(|payload| {
                        payload.state == "present"
                            && payload.object_key == part.object_key
                            && payload.checksum_blake3.as_deref()
                                == Some(part.ciphertext_blake3.as_str())
                            && u64::try_from(payload.size_bytes.unwrap_or(-1)).ok()
                                == Some(part.ciphertext_bytes)
                    })
            });
        let payload_receipts = if complete {
            signed
                .manifest
                .parts
                .iter()
                .map(|part| {
                    let payload = &replica.payloads[&part.part_id.to_string()];
                    UploadReceipt {
                        key: payload.object_key.clone(),
                        bytes: part.ciphertext_bytes,
                        version: payload.object_version.clone(),
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        let manifest_receipt = complete.then(|| UploadReceipt {
            key: replica.manifest_object_key.clone(),
            bytes: replica.manifest_size_bytes,
            version: None,
        });
        outcomes.push(ReplicaPublishOutcomeV2 {
            destination_fingerprint: replica.destination_fingerprint.clone(),
            requirement: commitment.requirement,
            reader_status: commitment.reader_status,
            payload_receipts,
            manifest_receipt,
            attempts: 1,
            result: if complete {
                Ok(())
            } else {
                Err(ReplicaPublishError::ConfigInvalid {
                    reason: "catalog replica is not complete".to_owned(),
                })
            },
        });
    }
    CompleteRecoveryPointSpec::from_verified_publish(
        signed,
        &outcomes,
        verifying_key,
        observed_at_ms,
    )
    .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CATALOG_COMMITMENT_MISMATCH"))?;
    Ok(())
}

fn require_exact_scope_evidence(
    signed: &SignedSetManifestV2,
) -> Result<(), RecoveryPointReadError> {
    let required = [
        "backupScopeId",
        "backupScopeMode",
        "backupScopeSnapshot",
        "backupScopeSnapshotHashBlake3",
        "backupScopeObjectCount",
    ];
    if required
        .iter()
        .any(|key| !signed.manifest.options.contains_key(*key))
        || signed.manifest.options.get("backupScopeId")
            != Some(&signed.manifest.target_id.to_string())
    {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_SCOPE_EVIDENCE_INVALID",
        ));
    }
    Ok(())
}

fn validate_historical_dump_identity(
    signed: &SignedSetManifestV2,
) -> Result<(), RecoveryPointReadError> {
    validate_historical_tool_identity(signed.manifest.engine, &signed.manifest.tool)
}

fn validate_historical_tool_identity(
    engine: Engine,
    tool: &restoreplane_pipeline::NativeToolIdentityV2,
) -> Result<(), RecoveryPointReadError> {
    let expected = match engine {
        Engine::Postgresql => "pg_dump",
        Engine::Mysql => "mysqldump",
        Engine::Mariadb => "mariadb-dump",
        Engine::Sqlite => "sqlite-online-backup",
        Engine::Sqlserver => "sqlpackage",
    };
    if tool.name != expected
        || tool.version.trim().is_empty()
        || tool.version.len() > 256
        || tool.executable_sha256.len() != 64
        || !tool
            .executable_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_HISTORICAL_TOOL_IDENTITY_INVALID",
        ));
    }
    Ok(())
}

fn parse_scope_table_objects(
    objects: &[serde_json::Value],
) -> Result<BTreeSet<(String, String)>, RecoveryPointReadError> {
    let mut tables = BTreeSet::new();
    for object in objects {
        let schema = object
            .get("schema")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
        let name = object
            .get("name")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_SCOPE_EVIDENCE_INVALID"))?;
        if schema.is_empty()
            || schema.len() > 256
            || name.is_empty()
            || name.len() > 256
            || schema.contains(['\0', '\r', '\n'])
            || name.contains(['\0', '\r', '\n'])
            || !tables.insert((schema.to_owned(), name.to_owned()))
        {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_SCOPE_EVIDENCE_INVALID",
            ));
        }
    }
    Ok(tables)
}

fn trusted_tool(
    catalog_path: &Path,
    tool_root: &Path,
    tool_name: &str,
    expected_version: Option<&str>,
    expected_sha256: Option<&str>,
) -> Result<TrustedNativeTool, RecoveryPointReadError> {
    let reader = open_reader_conn(catalog_path)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_TOOL_CATALOG_INVALID"))?;
    let mut statement = reader
        .prepare(
            "SELECT executable_path, version_text, file_sha256
             FROM native_tool_inventory
             WHERE tool_name = ?1 AND trust_status = 'trusted'
               AND signature_status = 'valid' AND source_kind = 'bundled'
               AND deleted_at_ms IS NULL
             ORDER BY last_verified_at_ms DESC, id",
        )
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_TOOL_CATALOG_INVALID"))?;
    let candidates = statement
        .query_map([tool_name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_TOOL_CATALOG_INVALID"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_TOOL_CATALOG_INVALID"))?;
    let root = fs::canonicalize(tool_root)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_TOOL_ROOT_INVALID"))?;
    for (path, version, digest) in candidates {
        if expected_version.is_some_and(|expected| expected != version)
            || expected_sha256.is_some_and(|expected| expected != digest)
        {
            continue;
        }
        let path = PathBuf::from(path);
        let canonical = fs::canonicalize(&path)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_TOOL_IDENTITY_INVALID"))?;
        if !canonical.starts_with(&root) || !canonical.is_file() {
            continue;
        }
        let actual = restoreplane_runtime::hash_executable(&canonical)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_TOOL_IDENTITY_INVALID"))?;
        if hex_sha256(&actual) == digest {
            return Ok(TrustedNativeTool {
                path: canonical,
                sha256: actual,
                version,
            });
        }
    }
    Err(RecoveryPointReadError::terminal(
        "RECOVERY_TRUSTED_TOOL_UNAVAILABLE",
    ))
}

fn decrypt_to_compressed(
    cipher_path: &Path,
    compressed_path: &Path,
    dek: &Dek,
    control: &RecoveryMaterializationControl,
) -> Result<(), RecoveryPointReadError> {
    let file = File::open(cipher_path)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_READ_FAILED"))?;
    let mut reader = BufReader::with_capacity(CIPHER_CHUNK * 2, file);
    let output = File::create(compressed_path)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
    let mut output = BufWriter::new(output);
    let mut decryptor = Some(dek.payload_decryptor(&NONCE_PREFIX));
    loop {
        control.check()?;
        let mut block = vec![0_u8; CIPHER_CHUNK];
        let read = read_fully(&mut reader, &mut block)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CIPHERTEXT_INVALID"))?;
        if read == 0 {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_CIPHERTEXT_INVALID",
            ));
        }
        block.truncate(read);
        let more = !reader
            .fill_buf()
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CIPHERTEXT_INVALID"))?
            .is_empty();
        let compressed = if more {
            decryptor
                .as_mut()
                .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_CIPHERTEXT_INVALID"))?
                .decrypt_next(block.as_slice())
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CIPHERTEXT_INVALID"))?
        } else {
            decryptor
                .take()
                .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_CIPHERTEXT_INVALID"))?
                .decrypt_last(block.as_slice())
                .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_CIPHERTEXT_INVALID"))?
        };
        output
            .write_all(&compressed)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
        if !more {
            break;
        }
    }
    output
        .flush()
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
    output
        .get_ref()
        .sync_all()
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))
}

fn decompress_to_plaintext(
    compressed_path: &Path,
    plaintext_path: &Path,
    expected_bytes: u64,
    control: &RecoveryMaterializationControl,
) -> Result<(u64, String), RecoveryPointReadError> {
    let input = File::open(compressed_path)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_READ_FAILED"))?;
    let mut decoder = zstd::stream::read::Decoder::new(BufReader::new(input))
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_COMPRESSION_INVALID"))?;
    let output = File::create(plaintext_path)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
    let mut output = BufWriter::new(output);
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        control.check()?;
        let read = decoder
            .read(&mut buffer)
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_COMPRESSION_INVALID"))?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_PLAINTEXT_LIMIT_EXCEEDED"))?;
        if total > expected_bytes {
            return Err(RecoveryPointReadError::terminal(
                "RECOVERY_PLAINTEXT_LIMIT_EXCEEDED",
            ));
        }
        hasher.update(&buffer[..read]);
        output
            .write_all(&buffer[..read])
            .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
    }
    output
        .flush()
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
    output
        .get_ref()
        .sync_all()
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_STAGING_WRITE_FAILED"))?;
    Ok((total, hasher.finalize().to_hex().to_string()))
}

fn read_fully(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

fn parse_destination_fingerprint(
    fingerprint: &str,
    expected_kind: &str,
) -> Result<DestinationId, RecoveryPointReadError> {
    let (kind, id) = fingerprint
        .split_once(':')
        .ok_or_else(|| RecoveryPointReadError::terminal("RECOVERY_DESTINATION_BINDING_INVALID"))?;
    if kind != expected_kind {
        return Err(RecoveryPointReadError::terminal(
            "RECOVERY_DESTINATION_BINDING_INVALID",
        ));
    }
    DestinationId::from_str(id)
        .map_err(|_| RecoveryPointReadError::terminal("RECOVERY_DESTINATION_BINDING_INVALID"))
}

fn engine_name(engine: Engine) -> &'static str {
    match engine {
        Engine::Postgresql => "postgresql",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Sqlserver => "sqlserver",
        Engine::Sqlite => "sqlite",
    }
}

fn requirement_name(requirement: ReplicaRequirementV2) -> &'static str {
    match requirement {
        ReplicaRequirementV2::Required => "required",
        ReplicaRequirementV2::BestEffort => "best_effort",
    }
}

fn reader_status_name(status: ReaderStatusV2) -> &'static str {
    match status {
        ReaderStatusV2::Eligible => "eligible",
        ReaderStatusV2::Unavailable => "unavailable",
        ReaderStatusV2::Unverified => "unverified",
    }
}

fn role_name(role: ArtifactPartRole) -> &'static str {
    match role {
        ArtifactPartRole::DatabaseDump => "database_dump",
        ArtifactPartRole::Globals => "globals",
        ArtifactPartRole::Metadata => "metadata",
        ArtifactPartRole::Wal => "wal",
        ArtifactPartRole::Auxiliary => "auxiliary",
    }
}

fn hex_sha256(value: &[u8; 32]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn normalized_sha256(lines: impl IntoIterator<Item = String>) -> (String, usize) {
    use sha2::{Digest, Sha256};
    let mut lines = lines
        .into_iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    lines.sort();
    lines.dedup();
    let count = lines.len();
    let digest = Sha256::digest(lines.join("\n").as_bytes());
    (format!("{digest:x}"), count)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn materialization_chunks_renew_claim_capacity_inside_synchronous_io() {
        let calls = Arc::new(AtomicUsize::new(0));
        let heartbeat_calls = Arc::clone(&calls);
        let control = RecoveryMaterializationControl::new(
            Arc::new(AtomicBool::new(false)),
            Instant::now() + Duration::from_secs(1),
            Arc::new(move || {
                heartbeat_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        );

        control.check().expect("first materialization chunk");
        control.check().expect("second materialization chunk");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn digest_only_large_explicit_scope_resolves_exact_catalog_table_set() {
        let objects = (0..96)
            .map(|index| {
                serde_json::json!({
                    "schema": "public",
                    "name": format!("accounting_partition_{index:03}"),
                    "kind": "table",
                })
            })
            .collect::<Vec<_>>();
        let snapshot = serde_json::json!({
            "mode": "explicit_tables",
            "objects": objects,
        })
        .to_string();
        assert!(
            snapshot.len() > 1_024,
            "fixture must exercise digest envelope"
        );
        let snapshot_hash = blake3::hash(snapshot.as_bytes()).to_hex().to_string();
        let target_id = uuid::Uuid::new_v4().to_string();
        let options = BTreeMap::from([
            ("backupScopeMode".to_owned(), "explicit_tables".to_owned()),
            ("backupScopeObjectCount".to_owned(), "96".to_owned()),
            (
                "backupScopeSnapshotHashBlake3".to_owned(),
                snapshot_hash.clone(),
            ),
            (
                "backupScopeSnapshot".to_owned(),
                serde_json::json!({ "blake3": snapshot_hash }).to_string(),
            ),
        ]);
        let evidence = CatalogScopeEvidence {
            mode: "explicit_tables".to_owned(),
            object_count: 96,
            snapshot,
            snapshot_hash,
            scope_id: target_id.clone(),
        };

        let tables = resolve_catalog_scope_tables(&target_id, &options, &evidence)
            .expect("digest envelope binds immutable catalog snapshot")
            .expect("explicit scope");

        assert_eq!(tables.len(), 96);
        assert!(tables.contains(&("public".to_owned(), "accounting_partition_095".to_owned())));
    }

    #[test]
    fn historical_dump_identity_is_evidence_not_a_current_bundle_file_requirement() {
        let historical =
            restoreplane_pipeline::NativeToolIdentityV2::new("pg_dump", "14.11", "a".repeat(64))
                .expect("historical identity");

        validate_historical_tool_identity(Engine::Postgresql, &historical)
            .expect("signed historical identity remains eligible after package upgrade");
        assert_eq!(historical.version, "14.11");
    }
}
