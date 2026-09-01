// crates/restoreplane-pipeline/src/fanout.rs
//! S05 §8 + S08 FR3 — 팬아웃(옵션 C). 커밋된 스풀과 서명 매니페스트를 목적지별로 독립
//! 업로드(JoinSet)하고 부분 성공 벡터를 반환한다. 한 목적지 실패가 다른 목적지를 막지 않는다.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use ed25519_dalek::VerifyingKey;
use futures::{stream, StreamExt};
use thiserror::Error;
use uuid::Uuid;

use crate::commit::{PipelineCfg, SpoolCommit};
use crate::dest::{
    upload_resumable, DestError, Destination, DestinationReaderV2, UploadError, UploadReceipt,
};
use crate::manifest::signed_manifest_path;
use crate::manifest_v2::{
    ManifestV2Error, ReaderStatusV2, ReplicaCommitmentV2, ReplicaRequirementV2, SignedSetManifestV2,
};

pub struct DestOutcome {
    pub dest_name: String,
    /// 암호문과 서명 매니페스트가 모두 커밋된 경우에만 `Ok`이다.
    pub result: Result<UploadReceipt, UploadError>,
    /// 암호문과 매니페스트에 사용한 전체 객체 업로드 시도 횟수.
    pub attempts: u32,
}

#[derive(Clone)]
pub(crate) enum UploadSource {
    File(std::path::PathBuf),
    Bytes(Arc<[u8]>),
}

async fn upload_source(
    dest: &dyn Destination,
    source: &UploadSource,
    key: &str,
) -> Result<UploadReceipt, UploadError> {
    match source {
        UploadSource::File(path) => upload_resumable(dest, path, key).await,
        UploadSource::Bytes(bytes) => {
            let mut writer = dest.writer(key).await?;
            if let Err(error) = writer.write_all(bytes).await {
                writer.abort().await;
                return Err(UploadError::from(error));
            }
            writer.commit().await.map_err(UploadError::from)
        }
    }
}

pub(crate) async fn upload_with_retry(
    dest: &dyn Destination,
    source: &UploadSource,
    key: &str,
    retries: u32,
    artifact: &str,
) -> (Result<UploadReceipt, UploadError>, u32) {
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match upload_source(dest, source, key).await {
            Ok(receipt) => return (Ok(receipt), attempts),
            Err(error) if error.is_retryable() && attempts <= retries => {
                let backoff = std::time::Duration::from_millis(50 * (1u64 << (attempts - 1)));
                tracing::warn!(
                    name = dest.name(),
                    attempts,
                    artifact,
                    ?error,
                    "업로드 재시도"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(error) => return (Err(error), attempts),
        }
    }
}

/// 커밋된 스풀과 서명 매니페스트를 목적지별로 독립 업로드한다.
///
/// 매니페스트는 로컬에서 먼저 전부 읽어 존재를 확인한 뒤, 각 목적지에서 암호문 다음에 마지막 커밋
/// 마커로 게시한다. 두 객체가 모두 커밋된 경우에만 목적지 성공으로 반환한다.
pub async fn fan_out(
    commit: &SpoolCommit,
    dests: Vec<Arc<dyn Destination>>,
    cfg: &PipelineCfg,
) -> Vec<DestOutcome> {
    if dests.is_empty() {
        return Vec::new();
    }

    let spool_key = commit
        .spool_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "backup.spool".to_string());
    let manifest_path = signed_manifest_path(&commit.spool_path);
    let manifest_key = manifest_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{spool_key}.manifest.json"));
    let manifest_bytes: Arc<[u8]> = match tokio::fs::read(&manifest_path).await {
        Ok(bytes) => bytes.into(),
        Err(error) => {
            let kind = error.kind();
            let message = format!(
                "서명 매니페스트를 팬아웃 전에 읽을 수 없음({}): {error}",
                manifest_path.display()
            );
            return dests
                .into_iter()
                .map(|dest| DestOutcome {
                    dest_name: dest.name().to_string(),
                    result: Err(UploadError::Io(std::io::Error::new(kind, message.clone()))),
                    attempts: 0,
                })
                .collect();
        }
    };

    let mut set: tokio::task::JoinSet<DestOutcome> = tokio::task::JoinSet::new();
    for dest in dests {
        let spool_source = UploadSource::File(commit.spool_path.clone());
        let manifest_source = UploadSource::Bytes(manifest_bytes.clone());
        let spool_key = spool_key.clone();
        let manifest_key = manifest_key.clone();
        let retries = cfg.upload_retries;
        set.spawn(async move {
            let name = dest.name().to_string();
            let (spool_result, spool_attempts) = upload_with_retry(
                dest.as_ref(),
                &spool_source,
                &spool_key,
                retries,
                "ciphertext",
            )
            .await;
            let (result, manifest_attempts) = match spool_result {
                Ok(spool_receipt) => {
                    let (manifest_result, attempts) = upload_with_retry(
                        dest.as_ref(),
                        &manifest_source,
                        &manifest_key,
                        retries,
                        "signed-manifest",
                    )
                    .await;
                    (manifest_result.map(|_| spool_receipt), attempts)
                }
                Err(error) => (Err(error), 0),
            };
            DestOutcome {
                dest_name: name,
                result,
                attempts: spool_attempts + manifest_attempts,
            }
        });
    }

    let mut outcomes = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(o) => outcomes.push(o),
            Err(e) => outcomes.push(DestOutcome {
                dest_name: "<panicked>".into(),
                result: Err(UploadError::TaskPanic(e.to_string())),
                attempts: 0,
            }),
        }
    }
    outcomes
}

pub const MAX_SET_MANIFEST_BYTES_V2: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedPartSourceV2 {
    part_id: Uuid,
    path: PathBuf,
}

impl EncodedPartSourceV2 {
    pub fn new(part_id: Uuid, path: impl Into<PathBuf>) -> Result<Self, ReplicaPublishError> {
        let path = path.into();
        if part_id.is_nil() || path.as_os_str().is_empty() {
            return Err(ReplicaPublishError::SourceSetInvalid {
                reason: "part ID and source path are required".to_owned(),
            });
        }
        Ok(Self { part_id, path })
    }
}

pub struct ReplicaEndpointV2 {
    pub(crate) destination_fingerprint: String,
    pub(crate) writer: Arc<dyn Destination>,
    pub(crate) reader: Arc<dyn DestinationReaderV2>,
}

impl ReplicaEndpointV2 {
    pub fn new(
        destination_fingerprint: impl Into<String>,
        writer: Arc<dyn Destination>,
        reader: Arc<dyn DestinationReaderV2>,
    ) -> Result<Self, ReplicaPublishError> {
        let destination_fingerprint = destination_fingerprint.into();
        if destination_fingerprint.trim().is_empty() || destination_fingerprint.len() > 512 {
            return Err(ReplicaPublishError::EndpointSetInvalid {
                reason: "destination fingerprint must contain 1..=512 bytes".to_owned(),
            });
        }
        Ok(Self {
            destination_fingerprint,
            writer,
            reader,
        })
    }
}

pub struct ReplicaPublishOutcomeV2 {
    pub destination_fingerprint: String,
    pub requirement: ReplicaRequirementV2,
    pub reader_status: ReaderStatusV2,
    pub payload_receipts: Vec<UploadReceipt>,
    pub manifest_receipt: Option<UploadReceipt>,
    pub attempts: u32,
    pub result: Result<(), ReplicaPublishError>,
}

#[derive(Debug, Error)]
pub enum ReplicaPublishError {
    #[error("signed set Manifest is invalid: {reason}")]
    ManifestInvalid { reason: String },
    #[error("encoded part source set is invalid: {reason}")]
    SourceSetInvalid { reason: String },
    #[error("replica endpoint set is invalid: {reason}")]
    EndpointSetInvalid { reason: String },
    #[error("replica publishing configuration is invalid: {reason}")]
    ConfigInvalid { reason: String },
    #[error("upload failed for {artifact}: {source}")]
    Upload {
        artifact: String,
        #[source]
        source: UploadError,
    },
    #[error("reader verification failed for {artifact}: {source}")]
    Reader {
        artifact: String,
        #[source]
        source: DestError,
    },
    #[error("reader checksum or byte count did not match the signed Manifest")]
    ReaderChecksumMismatch,
    #[error("published set Manifest did not survive the pinned-reader round trip")]
    ManifestRoundTripInvalid,
}

impl ReplicaPublishError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ManifestInvalid { .. } => "REPLICA_MANIFEST_INVALID",
            Self::SourceSetInvalid { .. } => "REPLICA_SOURCE_SET_INVALID",
            Self::EndpointSetInvalid { .. } => "REPLICA_ENDPOINT_SET_INVALID",
            Self::ConfigInvalid { .. } => "REPLICA_CONFIG_INVALID",
            Self::Upload { .. } => "REPLICA_UPLOAD_FAILED",
            Self::Reader { .. } => "REPLICA_READER_FAILED",
            Self::ReaderChecksumMismatch => "REPLICA_READER_CHECKSUM_MISMATCH",
            Self::ManifestRoundTripInvalid => "REPLICA_MANIFEST_ROUND_TRIP_INVALID",
        }
    }
}

/// Publishes every encoded part to each replica, verifies the committed objects through a
/// separately supplied reader, and only then publishes the signed set Manifest as the replica's
/// commit marker. A failed replica is returned as an outcome; invalid global input fails the call.
pub async fn publish_set_manifest_v2(
    signed: &SignedSetManifestV2,
    sources: Vec<EncodedPartSourceV2>,
    endpoints: Vec<ReplicaEndpointV2>,
    cfg: &PipelineCfg,
    expected_verifying_key: &VerifyingKey,
) -> Result<Vec<ReplicaPublishOutcomeV2>, ReplicaPublishError> {
    signed
        .verify(expected_verifying_key)
        .map_err(manifest_publish_error)?;
    if cfg.max_parallel_destinations == 0 {
        return Err(ReplicaPublishError::ConfigInvalid {
            reason: "max_parallel_destinations must be greater than zero".to_owned(),
        });
    }

    let mut source_by_id = BTreeMap::new();
    for source in sources {
        if !source.path.is_file() {
            return Err(ReplicaPublishError::SourceSetInvalid {
                reason: format!(
                    "encoded part source is not a file: {}",
                    source.path.display()
                ),
            });
        }
        if source_by_id.insert(source.part_id, source.path).is_some() {
            return Err(ReplicaPublishError::SourceSetInvalid {
                reason: "duplicate part ID".to_owned(),
            });
        }
    }
    let manifest_part_ids: BTreeSet<_> = signed
        .manifest
        .parts
        .iter()
        .map(|part| part.part_id)
        .collect();
    let source_part_ids: BTreeSet<_> = source_by_id.keys().copied().collect();
    if manifest_part_ids != source_part_ids {
        return Err(ReplicaPublishError::SourceSetInvalid {
            reason: "source part IDs must exactly match the signed Manifest".to_owned(),
        });
    }

    let replica_by_fingerprint: BTreeMap<_, _> = signed
        .manifest
        .replicas
        .iter()
        .map(|replica| (replica.destination_fingerprint.clone(), replica.clone()))
        .collect();
    let mut endpoint_fingerprints = BTreeSet::new();
    for endpoint in &endpoints {
        if !endpoint_fingerprints.insert(endpoint.destination_fingerprint.clone()) {
            return Err(ReplicaPublishError::EndpointSetInvalid {
                reason: "duplicate destination fingerprint".to_owned(),
            });
        }
    }
    let manifest_fingerprints: BTreeSet<_> = replica_by_fingerprint.keys().cloned().collect();
    if endpoint_fingerprints != manifest_fingerprints {
        return Err(ReplicaPublishError::EndpointSetInvalid {
            reason: "endpoint fingerprints must exactly match the signed Manifest".to_owned(),
        });
    }

    let manifest_bytes: Arc<[u8]> = signed
        .to_json()
        .map_err(manifest_publish_error)?
        .into_bytes()
        .into();
    if u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX) > MAX_SET_MANIFEST_BYTES_V2 {
        return Err(ReplicaPublishError::ManifestInvalid {
            reason: "serialized set Manifest exceeds the reader bound".to_owned(),
        });
    }

    let signed = Arc::new(signed.clone());
    let source_by_id = Arc::new(source_by_id);
    let expected_verifying_key = Arc::new(*expected_verifying_key);
    let upload_retries = cfg.upload_retries;
    let parallelism = cfg.max_parallel_destinations;
    let mut outcomes = stream::iter(endpoints.into_iter().map(|endpoint| {
        let replica = replica_by_fingerprint
            .get(&endpoint.destination_fingerprint)
            .expect("endpoint set was validated")
            .clone();
        let signed = signed.clone();
        let source_by_id = source_by_id.clone();
        let expected_verifying_key = expected_verifying_key.clone();
        let manifest_bytes = manifest_bytes.clone();
        async move {
            publish_replica_v2(
                endpoint,
                replica,
                signed,
                source_by_id,
                manifest_bytes,
                upload_retries,
                expected_verifying_key,
            )
            .await
        }
    }))
    .buffer_unordered(parallelism)
    .collect::<Vec<_>>()
    .await;
    outcomes.sort_by(|left, right| {
        left.destination_fingerprint
            .cmp(&right.destination_fingerprint)
    });
    Ok(outcomes)
}

#[allow(clippy::too_many_arguments)]
async fn publish_replica_v2(
    endpoint: ReplicaEndpointV2,
    replica: ReplicaCommitmentV2,
    signed: Arc<SignedSetManifestV2>,
    source_by_id: Arc<BTreeMap<Uuid, PathBuf>>,
    manifest_bytes: Arc<[u8]>,
    upload_retries: u32,
    expected_verifying_key: Arc<VerifyingKey>,
) -> ReplicaPublishOutcomeV2 {
    let mut payload_receipts = Vec::with_capacity(signed.manifest.parts.len());
    let mut manifest_receipt = None;
    let mut attempts = 0_u32;

    for part in &signed.manifest.parts {
        let source_path = source_by_id
            .get(&part.part_id)
            .expect("source set was validated")
            .clone();
        let source = UploadSource::File(source_path);
        let (upload, used_attempts) = upload_with_retry(
            endpoint.writer.as_ref(),
            &source,
            &part.object_key,
            upload_retries,
            "encoded-part-v2",
        )
        .await;
        attempts = attempts.saturating_add(used_attempts);
        match upload {
            Ok(receipt) => payload_receipts.push(receipt),
            Err(source) => {
                return failed_replica_outcome(
                    replica,
                    payload_receipts,
                    manifest_receipt,
                    attempts,
                    ReplicaPublishError::Upload {
                        artifact: part.object_key.clone(),
                        source,
                    },
                );
            }
        }

        let probe = match endpoint.reader.probe_object(&part.object_key).await {
            Ok(probe) => probe,
            Err(source) => {
                return failed_replica_outcome(
                    replica,
                    payload_receipts,
                    manifest_receipt,
                    attempts,
                    ReplicaPublishError::Reader {
                        artifact: part.object_key.clone(),
                        source,
                    },
                );
            }
        };
        if probe.bytes != part.ciphertext_bytes || probe.blake3 != part.ciphertext_blake3 {
            return failed_replica_outcome(
                replica,
                payload_receipts,
                manifest_receipt,
                attempts,
                ReplicaPublishError::ReaderChecksumMismatch,
            );
        }
    }

    let manifest_source = UploadSource::Bytes(manifest_bytes);
    let (upload, used_attempts) = upload_with_retry(
        endpoint.writer.as_ref(),
        &manifest_source,
        &replica.manifest_object_key,
        upload_retries,
        "signed-set-manifest-v2",
    )
    .await;
    attempts = attempts.saturating_add(used_attempts);
    match upload {
        Ok(receipt) => manifest_receipt = Some(receipt),
        Err(source) => {
            return failed_replica_outcome(
                replica,
                payload_receipts,
                manifest_receipt,
                attempts,
                ReplicaPublishError::Upload {
                    artifact: "signed-set-manifest-v2".to_owned(),
                    source,
                },
            );
        }
    }

    let landed_bytes = match endpoint
        .reader
        .read_bytes_bounded(&replica.manifest_object_key, MAX_SET_MANIFEST_BYTES_V2)
        .await
    {
        Ok(bytes) => bytes,
        Err(source) => {
            return failed_replica_outcome(
                replica,
                payload_receipts,
                manifest_receipt,
                attempts,
                ReplicaPublishError::Reader {
                    artifact: "signed-set-manifest-v2".to_owned(),
                    source,
                },
            );
        }
    };
    let landed = match serde_json::from_slice::<SignedSetManifestV2>(&landed_bytes) {
        Ok(landed) => landed,
        Err(_) => {
            return failed_replica_outcome(
                replica,
                payload_receipts,
                manifest_receipt,
                attempts,
                ReplicaPublishError::ManifestRoundTripInvalid,
            );
        }
    };
    if landed.verify(&expected_verifying_key).is_err() || landed != *signed {
        return failed_replica_outcome(
            replica,
            payload_receipts,
            manifest_receipt,
            attempts,
            ReplicaPublishError::ManifestRoundTripInvalid,
        );
    }

    ReplicaPublishOutcomeV2 {
        destination_fingerprint: replica.destination_fingerprint,
        requirement: replica.requirement,
        reader_status: ReaderStatusV2::Eligible,
        payload_receipts,
        manifest_receipt,
        attempts,
        result: Ok(()),
    }
}

fn failed_replica_outcome(
    replica: ReplicaCommitmentV2,
    payload_receipts: Vec<UploadReceipt>,
    manifest_receipt: Option<UploadReceipt>,
    attempts: u32,
    error: ReplicaPublishError,
) -> ReplicaPublishOutcomeV2 {
    ReplicaPublishOutcomeV2 {
        destination_fingerprint: replica.destination_fingerprint,
        requirement: replica.requirement,
        reader_status: ReaderStatusV2::Unavailable,
        payload_receipts,
        manifest_receipt,
        attempts,
        result: Err(error),
    }
}

fn manifest_publish_error(error: ManifestV2Error) -> ReplicaPublishError {
    ReplicaPublishError::ManifestInvalid {
        reason: error.code().to_owned(),
    }
}
