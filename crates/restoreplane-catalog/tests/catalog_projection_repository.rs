use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use restoreplane_catalog::{
    rusqlite::{params, Connection},
    BackupWorkSpec, CompleteRecoveryPointSpec, ConnectionProfileSpec, DatabaseTargetSpec,
    DestinationSpec, OccurrenceSpec, OperationalStore, PlanDraftSpec, RepositoryError,
};
use restoreplane_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use restoreplane_domain::{
    ArtifactPartId, ArtifactReplicaId, BackupPlanId, BackupRunId, ConnectionProfileId,
    DatabaseTargetId, DestinationId, DestinationRequirement, OccurrenceIdentity, OccurrenceOrigin,
    PlanRevisionId, ProtectionCompliance, RecoveryPointId, RestoreEligibility,
    SnapshotArtifactCompleteness, SnapshotReplicaDurability, StoredObjectId, VerificationRunId,
    WorkItemId,
};
use restoreplane_pipeline::{
    EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2, ReaderStatusV2, RecoveryOriginV2,
    ReplicaCommitmentV2, ReplicaPublishError, ReplicaPublishOutcomeV2, ReplicaRequirementV2,
    SetManifestV2, SignedSetManifestV2, UploadReceipt,
};
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn complete_recovery_point_uses_actual_parts_and_preserves_four_snapshot_axes() {
    let fixture = Fixture::new();
    let repository = fixture.store.catalog_projection();
    let recovery_point_id = RecoveryPointId::new();

    repository
        .ingest_complete(
            signed_projection_spec(
                recovery_point_id.clone(),
                "target:orders",
                1_000,
                1_010,
                ProjectionFixture::Degraded,
                Some("version-17"),
            )
            .expect("derive signed projection"),
        )
        .expect("ingest complete recovery point");

    let snapshot = repository
        .snapshot(&recovery_point_id)
        .expect("query snapshot");
    assert_eq!(
        snapshot.artifact_completeness(),
        SnapshotArtifactCompleteness::Complete
    );
    assert_eq!(
        snapshot.replica_durability(),
        SnapshotReplicaDurability::Degraded
    );
    assert_eq!(
        snapshot.protection_compliance(),
        ProtectionCompliance::NonCompliant
    );
    assert_eq!(
        snapshot.restore_eligibility(),
        RestoreEligibility::Conditional
    );
    assert_eq!(
        repository
            .recovery_point_counts(&recovery_point_id)
            .expect("read actual counts"),
        (1, 1)
    );
    let direct = Connection::open(&fixture.db_path).expect("open projection reader");
    let object_version: Option<String> = direct
        .query_row(
            "SELECT object_version
             FROM catalog_stored_objects
             WHERE recovery_point_id = ?1
               AND object_role = 'payload'",
            [recovery_point_id.as_str()],
            |row| row.get(0),
        )
        .expect("read stored object version");
    assert_eq!(object_version.as_deref(), Some("version-17"));
    let plaintext: Option<i64> = direct
        .query_row(
            "SELECT plaintext_bytes FROM catalog_recovery_points WHERE id=?1",
            [recovery_point_id.as_str()],
            |row| row.get(0),
        )
        .expect("verified plaintext aggregate");
    assert_eq!(plaintext, Some(128));
}

#[test]
fn signed_part_plaintext_sizes_are_aggregated_and_reingest_keeps_the_same_projection() {
    let fixture = Fixture::new();
    let repository = fixture.store.catalog_projection();
    let id = RecoveryPointId::new();
    let spec = signed_projection_spec_with_sizes(
        id.clone(),
        "target:aggregate",
        1000,
        1010,
        ProjectionFixture::Healthy,
        None,
        &[128, 256],
    )
    .expect("verified signed multipart specification");
    repository
        .ingest_complete(spec.clone())
        .expect("ingest signed aggregate");
    repository
        .ingest_complete(spec)
        .expect("same signed projection is idempotent");
    let direct = Connection::open(&fixture.db_path).unwrap();
    let actual: (Option<i64>, i64) = direct.query_row(
        "SELECT plaintext_bytes,(SELECT SUM(plaintext_bytes) FROM catalog_artifact_parts WHERE recovery_point_id=?1) FROM catalog_recovery_points WHERE id=?1",
        [id.as_str()], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
    assert_eq!(actual, (Some(384), 384));
    let incompatible = signed_projection_spec_with_sizes(
        id.clone(),
        "target:aggregate",
        1000,
        1010,
        ProjectionFixture::Healthy,
        None,
        &[512],
    )
    .unwrap();
    assert!(matches!(
        repository.ingest_complete(incompatible),
        Err(RepositoryError::CatalogProjectionInputInvalid)
    ));
    let after: Option<i64> = direct
        .query_row(
            "SELECT plaintext_bytes FROM catalog_recovery_points WHERE id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        after,
        Some(384),
        "a different signed manifest cannot rewrite completed metadata"
    );
}

#[test]
fn signed_plaintext_aggregate_overflow_refuses_the_entire_ingest() {
    let fixture = Fixture::new();
    let repository = fixture.store.catalog_projection();
    let spec = signed_projection_spec_with_sizes(
        RecoveryPointId::new(),
        "target:overflow",
        1000,
        1010,
        ProjectionFixture::Healthy,
        None,
        &[i64::MAX as u64, 1],
    )
    .expect("each signed part fits i64");
    assert!(matches!(
        repository.ingest_complete(spec),
        Err(RepositoryError::CatalogProjectionInputInvalid)
    ));
    assert_eq!(repository.recovery_point_count().unwrap(), 0);
    let direct = Connection::open(&fixture.db_path).unwrap();
    let parts: i64 = direct
        .query_row("SELECT COUNT(*) FROM catalog_artifact_parts", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        parts, 0,
        "aggregate overflow leaves no incomplete recovery point or parts"
    );
}

#[test]
fn incomplete_or_cross_recovery_evidence_and_fake_l3_are_rejected() {
    let fixture = Fixture::new();
    let repository = fixture.store.catalog_projection();
    let missing_object = signed_projection_spec(
        RecoveryPointId::new(),
        "target:orders",
        1_000,
        1_010,
        ProjectionFixture::Unreadable,
        None,
    )
    .expect_err("required part without present object cannot be complete");
    assert_eq!(missing_object.code(), "RECOVERY_POINT_INCOMPLETE");
    assert_eq!(
        repository
            .recovery_point_count()
            .expect("failed ingest rolled back"),
        0
    );

    let direct = Connection::open(&fixture.db_path).expect("open direct invariant probe");
    direct
        .execute_batch("PRAGMA foreign_keys=ON;")
        .expect("enable foreign keys");
    let fake_complete = direct.execute(
        "INSERT INTO catalog_recovery_points(
             id, origin_instance_id, source_target_identity, source_display_name,
             engine, state, consistency_grade, committed_at_ms,
             manifest_schema_version, set_manifest_checksum_blake3, signing_key_id,
             signature_status, required_part_count, present_part_count,
             engine_metadata_json, first_ingested_at_ms, last_ingested_at_ms
         ) VALUES (
             ?1, 'instance', 'target:a', 'a', 'postgresql', 'complete', 'consistent', 100,
             1, ?2, 'key', 'invalid', 0, 0, '{}', 100, 100
         )",
        params![RecoveryPointId::new().as_str(), "f".repeat(64)],
    );
    assert!(
        fake_complete.is_err(),
        "complete state requires valid signature and actual counts"
    );

    let first_rp = RecoveryPointId::new();
    let second_rp = RecoveryPointId::new();
    insert_incomplete_recovery_point(&direct, &first_rp, "1");
    insert_incomplete_recovery_point(&direct, &second_rp, "2");
    let first_part = ArtifactPartId::new();
    direct
        .execute(
            "INSERT INTO catalog_artifact_parts(
                 id, recovery_point_id, part_role, logical_name, required,
                 dump_format, compression_format, encryption_format,
                 metadata_json, created_at_ms
             ) VALUES (?1, ?2, 'database_dump', 'db.dump', 1, 'custom', 'zstd', 'age', '{}', 100)",
            params![first_part.as_str(), first_rp.as_str()],
        )
        .expect("insert first recovery part");
    let second_replica = ArtifactReplicaId::new();
    direct
        .execute(
            "INSERT INTO catalog_artifact_replicas(
                 id, recovery_point_id, destination_fingerprint, destination_kind,
                 requirement, state, immutability_capability, reader_status,
                 last_observed_at_ms, metadata_json
             ) VALUES (?1, ?2, 'local:b', 'local', 'required', 'committed',
                       'none', 'eligible', 100, '{}')",
            params![second_replica.as_str(), second_rp.as_str()],
        )
        .expect("insert second recovery replica");
    let cross_recovery_object = direct.execute(
        "INSERT INTO catalog_stored_objects(
             id, recovery_point_id, artifact_replica_id, artifact_part_id,
             object_role, object_key, state, observed_at_ms, metadata_json
         ) VALUES (?1, ?2, ?3, ?4, 'payload', 'cross.dump', 'present', 100, '{}')",
        params![
            StoredObjectId::new().as_str(),
            first_rp.as_str(),
            second_replica.as_str(),
            first_part.as_str()
        ],
    );
    assert!(
        cross_recovery_object.is_err(),
        "part and replica must belong to the same recovery point"
    );

    let fake_l3 = direct.execute(
        "INSERT INTO verification_runs(
             id, recovery_point_id, requested_level, trigger_kind, state,
             created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, 3, 'manual', 'passed', 200, 200)",
        params![VerificationRunId::new().as_str(), first_rp.as_str()],
    );
    assert!(fake_l3.is_err(), "L3 requires actual Restore Run evidence");
}

#[test]
fn clearing_and_rebuilding_every_catalog_table_preserves_control_state_digest() {
    let fixture = Fixture::new();
    seed_control_execution(&fixture.store);
    let repository = fixture.store.catalog_projection();
    let recovery_point_id = RecoveryPointId::new();
    let spec = signed_projection_spec(
        recovery_point_id.clone(),
        "target:orders",
        3_000,
        3_010,
        ProjectionFixture::Healthy,
        None,
    )
    .expect("derive signed projection");
    repository
        .ingest_complete(spec.clone())
        .expect("seed catalog projection");
    let before = repository
        .control_state_probe()
        .expect("control state before rebuild");
    assert_eq!(before.plan_count(), 1);
    assert_eq!(before.occurrence_count(), 1);
    assert_eq!(before.backup_run_count(), 1);
    assert_eq!(before.work_item_count(), 1);
    assert_eq!(before.attempt_count(), 1);
    assert_eq!(
        repository
            .control_to_catalog_foreign_key_count()
            .expect("FK direction"),
        0
    );

    repository
        .clear_all_projections()
        .expect("clear every rebuildable catalog table");
    assert_eq!(
        repository.recovery_point_count().expect("catalog cleared"),
        0
    );
    let during = repository
        .control_state_probe()
        .expect("control state while catalog empty");
    assert_eq!(during, before);

    repository
        .ingest_complete(spec)
        .expect("rebuild same recovery point");
    let after = repository
        .control_state_probe()
        .expect("control state after rebuild");
    assert_eq!(after, before);
    assert_eq!(
        repository
            .snapshot(&recovery_point_id)
            .expect("rebuilt snapshot")
            .restore_eligibility(),
        RestoreEligibility::Eligible
    );
}

#[derive(Clone, Copy)]
enum ProjectionFixture {
    Degraded,
    Healthy,
    Unreadable,
}

fn signed_projection_spec(
    recovery_point_id: RecoveryPointId,
    source_identity: &str,
    committed_at_ms: i64,
    ingested_at_ms: i64,
    fixture: ProjectionFixture,
    payload_version: Option<&str>,
) -> Result<CompleteRecoveryPointSpec, RepositoryError> {
    signed_projection_spec_with_sizes(
        recovery_point_id,
        source_identity,
        committed_at_ms,
        ingested_at_ms,
        fixture,
        payload_version,
        &[128],
    )
}

fn signed_projection_spec_with_sizes(
    recovery_point_id: RecoveryPointId,
    source_identity: &str,
    committed_at_ms: i64,
    ingested_at_ms: i64,
    fixture: ProjectionFixture,
    payload_version: Option<&str>,
    plaintext_sizes: &[u64],
) -> Result<CompleteRecoveryPointSpec, RepositoryError> {
    let occurrence_id = Uuid::new_v4();
    let target_id = Uuid::new_v4();
    let prefix = format!(
        "v2/{occurrence_id}/{target_id}/{}",
        recovery_point_id.as_str()
    );
    let parts = plaintext_sizes
        .iter()
        .enumerate()
        .map(|(index, plaintext_bytes)| {
            let logical_name = if index == 0 {
                "orders.dump".to_string()
            } else {
                format!("orders-{index}.dump")
            };
            ManifestPartV2::encoded(
                PartSpec::new(
                    Uuid::new_v4(),
                    ArtifactPartRole::DatabaseDump,
                    &logical_name,
                    true,
                    "custom",
                )
                .expect("part contract"),
                EncodingEvidenceV2::new(
                    "a".repeat(64),
                    "b".repeat(64),
                    *plaintext_bytes,
                    96,
                    "zstd",
                    "chacha20poly1305-stream",
                    1,
                )
                .expect("encoding evidence"),
                format!("{prefix}/parts/{logical_name}.enc"),
            )
            .expect("Manifest part")
        })
        .collect::<Vec<_>>();
    let payload_receipts = parts
        .iter()
        .map(|part| UploadReceipt {
            key: part.object_key.clone(),
            bytes: part.ciphertext_bytes,
            version: payload_version.map(str::to_owned),
        })
        .collect::<Vec<_>>();
    let manifest_key = format!("{prefix}/set-manifest.json");
    let mut replicas = vec![ReplicaCommitmentV2::new(
        "local:vault-a",
        "local",
        match fixture {
            ProjectionFixture::Degraded => ReplicaRequirementV2::BestEffort,
            ProjectionFixture::Healthy | ProjectionFixture::Unreadable => {
                ReplicaRequirementV2::Required
            }
        },
        ReaderStatusV2::Eligible,
        "none",
        manifest_key.clone(),
    )
    .expect("local replica")];
    if matches!(fixture, ProjectionFixture::Degraded) {
        replicas.push(
            ReplicaCommitmentV2::new(
                "smb:nas-a",
                "smb",
                ReplicaRequirementV2::Required,
                ReaderStatusV2::Eligible,
                "none",
                manifest_key.clone(),
            )
            .expect("required NAS replica"),
        );
    }
    let manifest = SetManifestV2::new(
        RecoveryOriginV2::new(
            recovery_point_id.as_uuid(),
            "instance-a",
            occurrence_id,
            Uuid::new_v4(),
            target_id,
            source_identity,
            "orders",
        )
        .expect("origin"),
        Engine::Postgresql,
        ConsistencyGrade::ApplicationConsistent,
        committed_at_ms,
        "signing-key-01",
        NativeToolIdentityV2::new("pg_dump", "17.4", "c".repeat(64)).expect("native tool"),
        BTreeMap::new(),
        parts,
        replicas,
    )
    .expect("set Manifest");
    let signing_key = SigningKey::from_bytes(&[17_u8; 32]);
    let signed = SignedSetManifestV2::create(manifest, &signing_key).expect("signed Manifest");
    let manifest_bytes =
        u64::try_from(signed.to_json().expect("Manifest JSON").len()).expect("Manifest size");
    let local_requirement = match fixture {
        ProjectionFixture::Degraded => ReplicaRequirementV2::BestEffort,
        ProjectionFixture::Healthy | ProjectionFixture::Unreadable => {
            ReplicaRequirementV2::Required
        }
    };
    let mut outcomes = vec![match fixture {
        ProjectionFixture::Unreadable => ReplicaPublishOutcomeV2 {
            destination_fingerprint: "local:vault-a".to_owned(),
            requirement: local_requirement,
            reader_status: ReaderStatusV2::Unavailable,
            payload_receipts: Vec::new(),
            manifest_receipt: None,
            attempts: 1,
            result: Err(ReplicaPublishError::ReaderChecksumMismatch),
        },
        ProjectionFixture::Degraded | ProjectionFixture::Healthy => ReplicaPublishOutcomeV2 {
            destination_fingerprint: "local:vault-a".to_owned(),
            requirement: local_requirement,
            reader_status: ReaderStatusV2::Eligible,
            payload_receipts,
            manifest_receipt: Some(UploadReceipt {
                key: manifest_key,
                bytes: manifest_bytes,
                version: None,
            }),
            attempts: 1,
            result: Ok(()),
        },
    }];
    if matches!(fixture, ProjectionFixture::Degraded) {
        outcomes.push(ReplicaPublishOutcomeV2 {
            destination_fingerprint: "smb:nas-a".to_owned(),
            requirement: ReplicaRequirementV2::Required,
            reader_status: ReaderStatusV2::Unavailable,
            payload_receipts: Vec::new(),
            manifest_receipt: None,
            attempts: 1,
            result: Err(ReplicaPublishError::ReaderChecksumMismatch),
        });
    }
    CompleteRecoveryPointSpec::from_verified_publish(
        &signed,
        &outcomes,
        &signing_key.verifying_key(),
        ingested_at_ms,
    )
}

fn seed_control_execution(store: &OperationalStore) {
    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    let destination_id = DestinationId::new();
    let plan_id = BackupPlanId::new();
    let revision_id = PlanRevisionId::new();
    let configuration = store.configuration();
    configuration
        .insert_connection(ConnectionProfileSpec::tcp(
            connection_id.clone(),
            "primary-pg",
            "postgresql",
            "db.internal",
            5432,
            100,
        ))
        .expect("insert connection");
    configuration
        .insert_target(DatabaseTargetSpec::new(
            target_id.clone(),
            connection_id,
            "oid:100",
            "orders",
            "orders",
            "source-fingerprint",
            110,
        ))
        .expect("insert target");
    configuration
        .insert_destination(DestinationSpec::local(
            destination_id.clone(),
            "vault",
            r#"{"path":"D:\\backup"}"#,
            120,
        ))
        .expect("insert destination");
    configuration
        .insert_plan_draft(PlanDraftSpec::manual(
            plan_id,
            revision_id.clone(),
            "nightly",
            1,
            "6".repeat(64),
            130,
        ))
        .expect("insert plan");
    configuration
        .add_revision_target(&revision_id, &target_id, 131)
        .expect("add target");
    configuration
        .add_revision_destination(
            &revision_id,
            &destination_id,
            DestinationRequirement::Required,
            132,
        )
        .expect("add destination");
    configuration
        .publish_revision(&revision_id, 140)
        .expect("publish plan");

    let runtime = store.runtime();
    let identity =
        OccurrenceIdentity::scheduled(revision_id.clone(), OccurrenceOrigin::Scheduled, 2_000)
            .expect("occurrence identity");
    let occurrence_id = identity.deterministic_id(Uuid::NAMESPACE_URL);
    runtime
        .upsert_occurrence(OccurrenceSpec::scheduled(
            occurrence_id.clone(),
            revision_id.clone(),
            OccurrenceOrigin::Scheduled,
            2_000,
            1_900,
        ))
        .expect("insert occurrence");
    let work_item_id = WorkItemId::new();
    runtime
        .enqueue_backup_work(BackupWorkSpec::new(
            BackupRunId::new(),
            work_item_id.clone(),
            occurrence_id,
            revision_id,
            target_id,
            "postgresql",
            "source-fingerprint",
            1_950,
        ))
        .expect("enqueue backup");
    runtime
        .claim_next(restoreplane_catalog::ClaimRequest::new(
            "worker-a", "boot-a", 2_000, 100,
        ))
        .expect("claim")
        .expect("due work");
}

fn insert_incomplete_recovery_point(
    connection: &Connection,
    recovery_point_id: &RecoveryPointId,
    checksum_digit: &str,
) {
    connection
        .execute(
            "INSERT INTO catalog_recovery_points(
                 id, origin_instance_id, source_target_identity, source_display_name,
                 engine, state, consistency_grade, manifest_schema_version,
                 set_manifest_checksum_blake3, signing_key_id, signature_status,
                 required_part_count, present_part_count, engine_metadata_json,
                 first_ingested_at_ms, last_ingested_at_ms
             ) VALUES (
                 ?1, 'instance', ?2, ?2, 'postgresql', 'incomplete', 'unknown', 1,
                 ?3, 'key', 'not_checked', 0, 0, '{}', 100, 100
             )",
            params![
                recovery_point_id.as_str(),
                format!("target:{checksum_digit}"),
                checksum_digit.repeat(64)
            ],
        )
        .expect("insert incomplete recovery point");
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("open operational store");
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
        }
    }
}
