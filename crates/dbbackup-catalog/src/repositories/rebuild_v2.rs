use std::collections::{BTreeMap, BTreeSet};

use dbbackup_pipeline::{
    DestinationReaderV2, ReaderStatusV2, ReplicaPublishError, ReplicaPublishOutcomeV2,
    SignedSetManifestV2, UploadReceipt, MAX_SET_MANIFEST_BYTES_V2,
};
use ed25519_dalek::VerifyingKey;

use super::{catalog_projection::CompleteRecoveryPointSpec, RepositoryError};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogRebuildDispositionV2 {
    Accepted,
    Rejected,
    UnknownKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogRebuildRejectReasonV2 {
    ReadFailed,
    MalformedEnvelope,
    NonCanonicalEnvelope,
    SignatureInvalid,
    DestinationNotDeclared,
    ManifestKeyMismatch,
    PayloadUnavailable,
    PayloadMismatch,
    ProjectionInvalid,
    DuplicateRecoveryPoint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogRebuildCandidateV2 {
    key: String,
    disposition: CatalogRebuildDispositionV2,
    reject_reason: Option<CatalogRebuildRejectReasonV2>,
    signing_key_id: Option<String>,
    recovery_point_id: Option<String>,
    manifest_checksum_blake3: Option<String>,
}

impl CatalogRebuildCandidateV2 {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub const fn disposition(&self) -> CatalogRebuildDispositionV2 {
        self.disposition
    }

    pub const fn reject_reason(&self) -> Option<CatalogRebuildRejectReasonV2> {
        self.reject_reason
    }

    pub fn signing_key_id(&self) -> Option<&str> {
        self.signing_key_id.as_deref()
    }

    pub fn recovery_point_id(&self) -> Option<&str> {
        self.recovery_point_id.as_deref()
    }

    pub fn manifest_checksum_blake3(&self) -> Option<&str> {
        self.manifest_checksum_blake3.as_deref()
    }

    fn with_manifest_checksum(mut self, checksum: &str) -> Self {
        self.manifest_checksum_blake3 = Some(checksum.to_owned());
        self
    }

    fn rejected(
        key: String,
        reason: CatalogRebuildRejectReasonV2,
        signed: Option<&SignedSetManifestV2>,
    ) -> Self {
        Self {
            key,
            disposition: CatalogRebuildDispositionV2::Rejected,
            reject_reason: Some(reason),
            signing_key_id: signed.map(|value| value.manifest.signing_key_id.clone()),
            recovery_point_id: signed.map(|value| value.manifest.recovery_point_id.to_string()),
            manifest_checksum_blake3: None,
        }
    }

    fn unknown_key(key: String, signed: &SignedSetManifestV2) -> Self {
        Self {
            key,
            disposition: CatalogRebuildDispositionV2::UnknownKey,
            reject_reason: None,
            signing_key_id: Some(signed.manifest.signing_key_id.clone()),
            recovery_point_id: Some(signed.manifest.recovery_point_id.to_string()),
            manifest_checksum_blake3: None,
        }
    }

    fn accepted(key: String, signed: &SignedSetManifestV2) -> Self {
        Self {
            key,
            disposition: CatalogRebuildDispositionV2::Accepted,
            reject_reason: None,
            signing_key_id: Some(signed.manifest.signing_key_id.clone()),
            recovery_point_id: Some(signed.manifest.recovery_point_id.to_string()),
            manifest_checksum_blake3: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CatalogRebuildApplyModeV2 {
    DryRun,
    Merge,
    Replace,
}

pub struct CatalogRebuildPlanV2 {
    candidates: Vec<CatalogRebuildCandidateV2>,
    pub(super) accepted_specs: Vec<CompleteRecoveryPointSpec>,
}

impl CatalogRebuildPlanV2 {
    pub fn candidates(&self) -> &[CatalogRebuildCandidateV2] {
        &self.candidates
    }

    pub fn accepted_count(&self) -> usize {
        self.count(CatalogRebuildDispositionV2::Accepted)
    }

    pub fn rejected_count(&self) -> usize {
        self.count(CatalogRebuildDispositionV2::Rejected)
    }

    pub fn unknown_key_count(&self) -> usize {
        self.count(CatalogRebuildDispositionV2::UnknownKey)
    }

    fn count(&self, disposition: CatalogRebuildDispositionV2) -> usize {
        self.candidates
            .iter()
            .filter(|candidate| candidate.disposition == disposition)
            .count()
    }

    pub(super) fn validate_for_apply(
        &self,
        mode: CatalogRebuildApplyModeV2,
    ) -> Result<(), RepositoryError> {
        if self.accepted_count() != self.accepted_specs.len()
            || (mode == CatalogRebuildApplyModeV2::Replace && self.accepted_specs.is_empty())
        {
            return Err(RepositoryError::CatalogProjectionInputInvalid);
        }
        let mut recovery_point_ids = BTreeSet::new();
        for spec in &self.accepted_specs {
            spec.validate()?;
            if !recovery_point_ids.insert(spec.id().as_str()) {
                return Err(RepositoryError::CatalogProjectionInputInvalid);
            }
        }
        Ok(())
    }
}

pub async fn prepare_catalog_rebuild_v2(
    reader: &dyn DestinationReaderV2,
    destination_fingerprint: &str,
    mut manifest_keys: Vec<String>,
    trusted_keys: &BTreeMap<String, VerifyingKey>,
    observed_at_ms: i64,
) -> Result<CatalogRebuildPlanV2, RepositoryError> {
    if destination_fingerprint.trim().is_empty()
        || destination_fingerprint.len() > 512
        || manifest_keys.len() > 10_000
        || observed_at_ms < 0
        || manifest_keys.iter().any(|key| !valid_object_key(key))
    {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }
    manifest_keys.sort();
    if manifest_keys.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(RepositoryError::CatalogProjectionInputInvalid);
    }

    let mut candidates = Vec::with_capacity(manifest_keys.len());
    let mut accepted_specs = Vec::new();
    let mut accepted_recovery_points = BTreeSet::new();
    for key in manifest_keys {
        let bytes = match reader
            .read_bytes_bounded(&key, MAX_SET_MANIFEST_BYTES_V2)
            .await
        {
            Ok(bytes) => bytes,
            Err(_) => {
                candidates.push(CatalogRebuildCandidateV2::rejected(
                    key,
                    CatalogRebuildRejectReasonV2::ReadFailed,
                    None,
                ));
                continue;
            }
        };
        let manifest_checksum = blake3::hash(&bytes).to_hex().to_string();
        let manifest_probe = match reader.probe_object(&key).await {
            Ok(probe) if probe.bytes == bytes.len() as u64 && probe.blake3 == manifest_checksum => {
                probe
            }
            Ok(_) => {
                candidates.push(
                    CatalogRebuildCandidateV2::rejected(
                        key,
                        CatalogRebuildRejectReasonV2::PayloadMismatch,
                        None,
                    )
                    .with_manifest_checksum(&manifest_checksum),
                );
                continue;
            }
            Err(_) => {
                candidates.push(
                    CatalogRebuildCandidateV2::rejected(
                        key,
                        CatalogRebuildRejectReasonV2::ReadFailed,
                        None,
                    )
                    .with_manifest_checksum(&manifest_checksum),
                );
                continue;
            }
        };
        let signed = match serde_json::from_slice::<SignedSetManifestV2>(&bytes) {
            Ok(signed) => signed,
            Err(_) => {
                candidates.push(
                    CatalogRebuildCandidateV2::rejected(
                        key,
                        CatalogRebuildRejectReasonV2::MalformedEnvelope,
                        None,
                    )
                    .with_manifest_checksum(&manifest_checksum),
                );
                continue;
            }
        };
        if signed.manifest.validate().is_err() {
            candidates.push(
                CatalogRebuildCandidateV2::rejected(
                    key,
                    CatalogRebuildRejectReasonV2::MalformedEnvelope,
                    None,
                )
                .with_manifest_checksum(&manifest_checksum),
            );
            continue;
        }
        let canonical_envelope = match signed.to_json() {
            Ok(value) => value,
            Err(_) => {
                candidates.push(
                    CatalogRebuildCandidateV2::rejected(
                        key,
                        CatalogRebuildRejectReasonV2::MalformedEnvelope,
                        Some(&signed),
                    )
                    .with_manifest_checksum(&manifest_checksum),
                );
                continue;
            }
        };
        if canonical_envelope.as_bytes() != bytes {
            candidates.push(
                CatalogRebuildCandidateV2::rejected(
                    key,
                    CatalogRebuildRejectReasonV2::NonCanonicalEnvelope,
                    Some(&signed),
                )
                .with_manifest_checksum(&manifest_checksum),
            );
            continue;
        }
        let Some(verifying_key) = trusted_keys.get(&signed.manifest.signing_key_id) else {
            candidates.push(
                CatalogRebuildCandidateV2::unknown_key(key, &signed)
                    .with_manifest_checksum(&manifest_checksum),
            );
            continue;
        };
        if signed.verify(verifying_key).is_err() {
            candidates.push(
                CatalogRebuildCandidateV2::rejected(
                    key,
                    CatalogRebuildRejectReasonV2::SignatureInvalid,
                    Some(&signed),
                )
                .with_manifest_checksum(&manifest_checksum),
            );
            continue;
        }
        let Some(source_replica) = signed
            .manifest
            .replicas
            .iter()
            .find(|replica| replica.destination_fingerprint == destination_fingerprint)
        else {
            candidates.push(
                CatalogRebuildCandidateV2::rejected(
                    key,
                    CatalogRebuildRejectReasonV2::DestinationNotDeclared,
                    Some(&signed),
                )
                .with_manifest_checksum(&manifest_checksum),
            );
            continue;
        };
        if source_replica.manifest_object_key != key {
            candidates.push(
                CatalogRebuildCandidateV2::rejected(
                    key,
                    CatalogRebuildRejectReasonV2::ManifestKeyMismatch,
                    Some(&signed),
                )
                .with_manifest_checksum(&manifest_checksum),
            );
            continue;
        }

        let mut payload_receipts = Vec::with_capacity(signed.manifest.parts.len());
        let mut payload_failure = None;
        for part in &signed.manifest.parts {
            match reader.probe_object(&part.object_key).await {
                Ok(probe)
                    if probe.bytes == part.ciphertext_bytes
                        && probe.blake3 == part.ciphertext_blake3 =>
                {
                    payload_receipts.push(UploadReceipt {
                        key: part.object_key.clone(),
                        bytes: probe.bytes,
                        version: probe.version,
                    });
                }
                Ok(_) => {
                    payload_failure = Some(CatalogRebuildRejectReasonV2::PayloadMismatch);
                    break;
                }
                Err(_) => {
                    payload_failure = Some(CatalogRebuildRejectReasonV2::PayloadUnavailable);
                    break;
                }
            }
        }
        if let Some(reason) = payload_failure {
            candidates.push(
                CatalogRebuildCandidateV2::rejected(key, reason, Some(&signed))
                    .with_manifest_checksum(&manifest_checksum),
            );
            continue;
        }

        let outcomes = signed
            .manifest
            .replicas
            .iter()
            .map(|replica| {
                if replica.destination_fingerprint == destination_fingerprint {
                    ReplicaPublishOutcomeV2 {
                        destination_fingerprint: replica.destination_fingerprint.clone(),
                        requirement: replica.requirement,
                        reader_status: ReaderStatusV2::Eligible,
                        payload_receipts: payload_receipts.clone(),
                        manifest_receipt: Some(UploadReceipt {
                            key: key.clone(),
                            bytes: bytes.len() as u64,
                            version: manifest_probe.version.clone(),
                        }),
                        attempts: 0,
                        result: Ok(()),
                    }
                } else {
                    ReplicaPublishOutcomeV2 {
                        destination_fingerprint: replica.destination_fingerprint.clone(),
                        requirement: replica.requirement,
                        reader_status: ReaderStatusV2::Unavailable,
                        payload_receipts: Vec::new(),
                        manifest_receipt: None,
                        attempts: 0,
                        result: Err(ReplicaPublishError::ConfigInvalid {
                            reason: "replica was not observed by this rebuild source".to_owned(),
                        }),
                    }
                }
            })
            .collect::<Vec<_>>();
        let spec = match CompleteRecoveryPointSpec::from_verified_publish(
            &signed,
            &outcomes,
            verifying_key,
            observed_at_ms,
        ) {
            Ok(spec) => spec,
            Err(_) => {
                candidates.push(
                    CatalogRebuildCandidateV2::rejected(
                        key,
                        CatalogRebuildRejectReasonV2::ProjectionInvalid,
                        Some(&signed),
                    )
                    .with_manifest_checksum(&manifest_checksum),
                );
                continue;
            }
        };
        if !accepted_recovery_points.insert(spec.id().as_str().to_owned()) {
            candidates.push(
                CatalogRebuildCandidateV2::rejected(
                    key,
                    CatalogRebuildRejectReasonV2::DuplicateRecoveryPoint,
                    Some(&signed),
                )
                .with_manifest_checksum(&manifest_checksum),
            );
            continue;
        }
        candidates.push(
            CatalogRebuildCandidateV2::accepted(key, &signed)
                .with_manifest_checksum(&manifest_checksum),
        );
        accepted_specs.push(spec);
    }

    Ok(CatalogRebuildPlanV2 {
        candidates,
        accepted_specs,
    })
}

fn valid_object_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 2_048
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}
