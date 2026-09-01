use std::collections::BTreeMap;

use dbbackup_catalog::{
    ArtifactPartSpec, ArtifactReplicaSpec, ArtifactReplicaState, CompleteRecoveryPointSpec,
    ReplicaReaderStatus, SnapshotAxes, StoredObjectSpec,
};
use dbbackup_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use dbbackup_domain::{
    ArtifactPartId, ArtifactReplicaId, DestinationRequirement, ProtectionCompliance,
    RestoreEligibility, SnapshotArtifactCompleteness, SnapshotReplicaDurability, StoredObjectId,
};
use dbbackup_pipeline::{
    EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2, ReaderStatusV2, RecoveryOriginV2,
    ReplicaCommitmentV2, ReplicaPublishOutcomeV2, ReplicaRequirementV2, SetManifestV2,
    SignedSetManifestV2, UploadReceipt,
};
use ed25519_dalek::SigningKey;
use uuid::Uuid;

#[test]
fn best_effort_copy_keeps_restore_possible_without_claiming_required_protection() {
    let part = required_database_part();
    let missing_nas = observed_replica(
        DestinationRequirement::Required,
        ArtifactReplicaState::Missing,
        ReplicaReaderStatus::Unavailable,
        "smb:nas-a",
    );
    let local = observed_replica(
        DestinationRequirement::BestEffort,
        ArtifactReplicaState::Committed,
        ReplicaReaderStatus::Eligible,
        "local:vault-a",
    );
    let object = present_object(&part, &local);

    let axes = SnapshotAxes::calculate(&[part], &[missing_nas, local], &[object])
        .expect("actual evidence is projectable");

    assert_eq!(
        axes.artifact_completeness(),
        SnapshotArtifactCompleteness::Complete
    );
    assert_eq!(
        axes.replica_durability(),
        SnapshotReplicaDurability::Degraded
    );
    assert_eq!(
        axes.protection_compliance(),
        ProtectionCompliance::NonCompliant
    );
    assert_eq!(axes.restore_eligibility(), RestoreEligibility::Conditional);
}

#[test]
fn failed_best_effort_copy_does_not_downgrade_an_eligible_required_replica() {
    let part = required_database_part();
    let local = observed_replica(
        DestinationRequirement::Required,
        ArtifactReplicaState::Committed,
        ReplicaReaderStatus::Eligible,
        "local:vault-a",
    );
    let missing_archive = observed_replica(
        DestinationRequirement::BestEffort,
        ArtifactReplicaState::Missing,
        ReplicaReaderStatus::Unavailable,
        "s3:archive",
    );
    let object = present_object(&part, &local);

    let axes = SnapshotAxes::calculate(&[part], &[local, missing_archive], &[object])
        .expect("actual evidence is projectable");

    assert_eq!(
        axes.replica_durability(),
        SnapshotReplicaDurability::Healthy
    );
    assert_eq!(
        axes.protection_compliance(),
        ProtectionCompliance::Compliant
    );
    assert_eq!(axes.restore_eligibility(), RestoreEligibility::Eligible);
}

#[test]
fn writer_success_without_an_eligible_reader_is_not_a_complete_recovery_point() {
    let part = required_database_part();
    let unreadable = observed_replica(
        DestinationRequirement::Required,
        ArtifactReplicaState::Committed,
        ReplicaReaderStatus::Unavailable,
        "local:vault-a",
    );
    let object = present_object(&part, &unreadable);

    let axes = SnapshotAxes::calculate(&[part], &[unreadable], &[object])
        .expect("unavailable evidence still has a truthful projection");

    assert_eq!(
        axes.artifact_completeness(),
        SnapshotArtifactCompleteness::Incomplete
    );
    assert_eq!(
        axes.replica_durability(),
        SnapshotReplicaDurability::Unavailable
    );
    assert_eq!(
        axes.protection_compliance(),
        ProtectionCompliance::NonCompliant
    );
    assert_eq!(axes.restore_eligibility(), RestoreEligibility::Blocked);
}

#[test]
fn signed_manifest_and_actual_publish_outcomes_are_the_only_axis_inputs() {
    let recovery_point_id = uuid("11111111-1111-4111-8111-111111111111");
    let occurrence_id = uuid("22222222-2222-4222-8222-222222222222");
    let target_id = uuid("33333333-3333-4333-8333-333333333333");
    let prefix = format!("v2/{occurrence_id}/{target_id}/{recovery_point_id}");
    let part_id = uuid("44444444-4444-4444-8444-444444444444");
    let part = ManifestPartV2::encoded(
        PartSpec::new(
            part_id,
            ArtifactPartRole::DatabaseDump,
            "orders.dump",
            true,
            "custom",
        )
        .expect("part"),
        EncodingEvidenceV2::new(
            "a".repeat(64),
            "b".repeat(64),
            128,
            96,
            "zstd",
            "chacha20poly1305-stream",
            1,
        )
        .expect("evidence"),
        format!("{prefix}/parts/orders.dump.enc"),
    )
    .expect("manifest part");
    let manifest = SetManifestV2::new(
        RecoveryOriginV2::new(
            recovery_point_id,
            "instance-a",
            occurrence_id,
            uuid("55555555-5555-4555-8555-555555555555"),
            target_id,
            "oid:100",
            "orders",
        )
        .expect("origin"),
        Engine::Postgresql,
        ConsistencyGrade::ApplicationConsistent,
        1_000,
        "signing-key-01",
        NativeToolIdentityV2::new("pg_dump", "17.4", "c".repeat(64)).expect("tool"),
        BTreeMap::new(),
        vec![part],
        vec![
            ReplicaCommitmentV2::new(
                "local:vault-a",
                "local",
                ReplicaRequirementV2::BestEffort,
                ReaderStatusV2::Eligible,
                "none",
                format!("{prefix}/set-manifest.json"),
            )
            .expect("local"),
            ReplicaCommitmentV2::new(
                "smb:nas-a",
                "smb",
                ReplicaRequirementV2::Required,
                ReaderStatusV2::Eligible,
                "none",
                format!("{prefix}/set-manifest.json"),
            )
            .expect("nas"),
        ],
    )
    .expect("manifest");
    let signing_key = SigningKey::from_bytes(&[8_u8; 32]);
    let signed = SignedSetManifestV2::create(manifest, &signing_key).expect("signed");
    let signed_bytes =
        u64::try_from(signed.to_json().expect("signed JSON").len()).expect("signed Manifest size");
    let outcomes = vec![
        ReplicaPublishOutcomeV2 {
            destination_fingerprint: "smb:nas-a".to_owned(),
            requirement: ReplicaRequirementV2::Required,
            reader_status: ReaderStatusV2::Unavailable,
            payload_receipts: Vec::new(),
            manifest_receipt: None,
            attempts: 1,
            result: Err(dbbackup_pipeline::ReplicaPublishError::ReaderChecksumMismatch),
        },
        ReplicaPublishOutcomeV2 {
            destination_fingerprint: "local:vault-a".to_owned(),
            requirement: ReplicaRequirementV2::BestEffort,
            reader_status: ReaderStatusV2::Eligible,
            payload_receipts: vec![UploadReceipt {
                key: format!("{prefix}/parts/orders.dump.enc"),
                bytes: 96,
                version: Some("payload-version-1".to_owned()),
            }],
            manifest_receipt: Some(UploadReceipt {
                key: format!("{prefix}/set-manifest.json"),
                bytes: signed_bytes,
                version: Some("manifest-version-1".to_owned()),
            }),
            attempts: 2,
            result: Ok(()),
        },
    ];

    let spec = CompleteRecoveryPointSpec::from_verified_publish(
        &signed,
        &outcomes,
        &signing_key.verifying_key(),
        1_010,
    )
    .expect("derive projection input");
    let axes = spec.snapshot_axes().expect("calculated axes");
    assert_eq!(
        axes.artifact_completeness(),
        SnapshotArtifactCompleteness::Complete
    );
    assert_eq!(
        axes.replica_durability(),
        SnapshotReplicaDurability::Degraded
    );
    assert_eq!(
        axes.protection_compliance(),
        ProtectionCompliance::NonCompliant
    );
    assert_eq!(axes.restore_eligibility(), RestoreEligibility::Conditional);
}

fn required_database_part() -> ArtifactPartSpec {
    ArtifactPartSpec::database_dump(
        ArtifactPartId::new(),
        "orders.dump",
        "a".repeat(64),
        128,
        1_000,
    )
}

fn observed_replica(
    requirement: DestinationRequirement,
    state: ArtifactReplicaState,
    reader_status: ReplicaReaderStatus,
    fingerprint: &str,
) -> ArtifactReplicaSpec {
    ArtifactReplicaSpec::observed(
        ArtifactReplicaId::new(),
        fingerprint,
        "local",
        requirement,
        state,
        "none",
        reader_status,
        (state == ArtifactReplicaState::Committed).then_some(1_000),
        1_010,
    )
}

fn present_object(part: &ArtifactPartSpec, replica: &ArtifactReplicaSpec) -> StoredObjectSpec {
    StoredObjectSpec::payload(
        StoredObjectId::new(),
        replica.id().clone(),
        part.id().clone(),
        "v2/orders.dump.enc",
        "b".repeat(64),
        96,
        1_010,
    )
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("fixture UUID")
}
