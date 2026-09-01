use std::sync::Arc;

use ed25519_dalek::VerifyingKey;

use crate::dest::{DestinationReaderV2, UploadError};
use crate::fanout::{
    upload_with_retry, ReplicaEndpointV2, ReplicaPublishError, ReplicaPublishOutcomeV2,
    UploadSource,
};
use crate::manifest_v2::{ReaderStatusV2, ReplicaCommitmentV2, SignedSetManifestV2};
use crate::PipelineCfg;

const MAX_SET_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

pub struct RepairSourceV2 {
    destination_fingerprint: String,
    reader: Arc<dyn DestinationReaderV2>,
}

impl RepairSourceV2 {
    pub fn new(
        destination_fingerprint: impl Into<String>,
        reader: Arc<dyn DestinationReaderV2>,
    ) -> Result<Self, ReplicaPublishError> {
        let destination_fingerprint = destination_fingerprint.into();
        if destination_fingerprint.trim().is_empty() || destination_fingerprint.len() > 512 {
            return Err(ReplicaPublishError::EndpointSetInvalid {
                reason: "repair source fingerprint must contain 1..=512 bytes".to_owned(),
            });
        }
        Ok(Self {
            destination_fingerprint,
            reader,
        })
    }
}

pub async fn repair_replica_v2(
    signed: &SignedSetManifestV2,
    source: RepairSourceV2,
    target: ReplicaEndpointV2,
    cfg: &PipelineCfg,
    expected_verifying_key: &VerifyingKey,
) -> Result<ReplicaPublishOutcomeV2, ReplicaPublishError> {
    signed.verify(expected_verifying_key).map_err(|error| {
        ReplicaPublishError::ManifestInvalid {
            reason: error.code().to_owned(),
        }
    })?;
    if cfg.max_parallel_destinations == 0 {
        return Err(ReplicaPublishError::ConfigInvalid {
            reason: "max_parallel_destinations must be greater than zero".to_owned(),
        });
    }
    if source.destination_fingerprint == target.destination_fingerprint {
        return Err(ReplicaPublishError::EndpointSetInvalid {
            reason: "repair source and target must be different replicas".to_owned(),
        });
    }
    let source_replica = signed
        .manifest
        .replicas
        .iter()
        .find(|replica| replica.destination_fingerprint == source.destination_fingerprint)
        .ok_or_else(|| ReplicaPublishError::EndpointSetInvalid {
            reason: "repair source is not declared by the signed Manifest".to_owned(),
        })?;
    let target_replica = signed
        .manifest
        .replicas
        .iter()
        .find(|replica| replica.destination_fingerprint == target.destination_fingerprint)
        .cloned()
        .ok_or_else(|| ReplicaPublishError::EndpointSetInvalid {
            reason: "repair target is not declared by the signed Manifest".to_owned(),
        })?;

    let source_manifest_bytes = source
        .reader
        .read_bytes_bounded(&source_replica.manifest_object_key, MAX_SET_MANIFEST_BYTES)
        .await
        .map_err(|source_error| ReplicaPublishError::Reader {
            artifact: "repair-source-set-manifest-v2".to_owned(),
            source: source_error,
        })?;
    let source_manifest = serde_json::from_slice::<SignedSetManifestV2>(&source_manifest_bytes)
        .map_err(|_| ReplicaPublishError::ManifestRoundTripInvalid)?;
    if source_manifest.verify(expected_verifying_key).is_err() || source_manifest != *signed {
        return Err(ReplicaPublishError::ManifestRoundTripInvalid);
    }

    Ok(repair_target(
        signed,
        source,
        target,
        target_replica,
        cfg.upload_retries,
        expected_verifying_key,
    )
    .await)
}

async fn repair_target(
    signed: &SignedSetManifestV2,
    source: RepairSourceV2,
    target: ReplicaEndpointV2,
    target_replica: ReplicaCommitmentV2,
    upload_retries: u32,
    expected_verifying_key: &VerifyingKey,
) -> ReplicaPublishOutcomeV2 {
    let mut payload_receipts = Vec::with_capacity(signed.manifest.parts.len());
    let mut manifest_receipt = None;
    let mut attempts = 0_u32;

    for part in &signed.manifest.parts {
        attempts = attempts.saturating_add(1);
        let mut writer = match target.writer.writer(&part.object_key).await {
            Ok(writer) => writer,
            Err(source_error) => {
                return failed(
                    target_replica,
                    payload_receipts,
                    manifest_receipt,
                    attempts,
                    ReplicaPublishError::Upload {
                        artifact: part.object_key.clone(),
                        source: UploadError::Dest(source_error),
                    },
                );
            }
        };
        let source_probe = match source
            .reader
            .copy_object_to(&part.object_key, &mut writer, part.ciphertext_bytes)
            .await
        {
            Ok(probe) => probe,
            Err(source_error) => {
                writer.abort().await;
                return failed(
                    target_replica,
                    payload_receipts,
                    manifest_receipt,
                    attempts,
                    ReplicaPublishError::Reader {
                        artifact: part.object_key.clone(),
                        source: source_error,
                    },
                );
            }
        };
        if source_probe.bytes != part.ciphertext_bytes
            || source_probe.blake3 != part.ciphertext_blake3
        {
            writer.abort().await;
            return failed(
                target_replica,
                payload_receipts,
                manifest_receipt,
                attempts,
                ReplicaPublishError::ReaderChecksumMismatch,
            );
        }
        let receipt = match writer.commit().await {
            Ok(receipt) => receipt,
            Err(source_error) => {
                return failed(
                    target_replica,
                    payload_receipts,
                    manifest_receipt,
                    attempts,
                    ReplicaPublishError::Upload {
                        artifact: part.object_key.clone(),
                        source: UploadError::Dest(source_error),
                    },
                );
            }
        };
        let target_probe = match target.reader.probe_object(&part.object_key).await {
            Ok(probe) => probe,
            Err(source_error) => {
                return failed(
                    target_replica,
                    payload_receipts,
                    manifest_receipt,
                    attempts,
                    ReplicaPublishError::Reader {
                        artifact: part.object_key.clone(),
                        source: source_error,
                    },
                );
            }
        };
        payload_receipts.push(receipt);
        if target_probe.bytes != part.ciphertext_bytes
            || target_probe.blake3 != part.ciphertext_blake3
        {
            return failed(
                target_replica,
                payload_receipts,
                manifest_receipt,
                attempts,
                ReplicaPublishError::ReaderChecksumMismatch,
            );
        }
    }

    let manifest_bytes: Arc<[u8]> = match signed.to_json() {
        Ok(json) => json.into_bytes().into(),
        Err(error) => {
            return failed(
                target_replica,
                payload_receipts,
                manifest_receipt,
                attempts,
                ReplicaPublishError::ManifestInvalid {
                    reason: error.code().to_owned(),
                },
            );
        }
    };
    let (manifest_upload, used_attempts) = upload_with_retry(
        target.writer.as_ref(),
        &UploadSource::Bytes(manifest_bytes),
        &target_replica.manifest_object_key,
        upload_retries,
        "repair-signed-set-manifest-v2",
    )
    .await;
    attempts = attempts.saturating_add(used_attempts);
    match manifest_upload {
        Ok(receipt) => manifest_receipt = Some(receipt),
        Err(source_error) => {
            return failed(
                target_replica,
                payload_receipts,
                manifest_receipt,
                attempts,
                ReplicaPublishError::Upload {
                    artifact: "repair-signed-set-manifest-v2".to_owned(),
                    source: source_error,
                },
            );
        }
    }

    let landed = target
        .reader
        .read_bytes_bounded(&target_replica.manifest_object_key, MAX_SET_MANIFEST_BYTES)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SignedSetManifestV2>(&bytes).ok());
    if landed
        .as_ref()
        .is_none_or(|landed| landed.verify(expected_verifying_key).is_err() || landed != signed)
    {
        return failed(
            target_replica,
            payload_receipts,
            manifest_receipt,
            attempts,
            ReplicaPublishError::ManifestRoundTripInvalid,
        );
    }

    ReplicaPublishOutcomeV2 {
        destination_fingerprint: target_replica.destination_fingerprint,
        requirement: target_replica.requirement,
        reader_status: ReaderStatusV2::Eligible,
        payload_receipts,
        manifest_receipt,
        attempts,
        result: Ok(()),
    }
}

fn failed(
    target_replica: ReplicaCommitmentV2,
    payload_receipts: Vec<crate::UploadReceipt>,
    manifest_receipt: Option<crate::UploadReceipt>,
    attempts: u32,
    error: ReplicaPublishError,
) -> ReplicaPublishOutcomeV2 {
    ReplicaPublishOutcomeV2 {
        destination_fingerprint: target_replica.destination_fingerprint,
        requirement: target_replica.requirement,
        reader_status: ReaderStatusV2::Unavailable,
        payload_receipts,
        manifest_receipt,
        attempts,
        result: Err(error),
    }
}
