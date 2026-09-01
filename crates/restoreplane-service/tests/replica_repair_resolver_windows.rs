#![cfg(windows)]

use std::collections::BTreeMap;
use std::process::Command;
use std::str::FromStr;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use restoreplane_catalog::{
    rusqlite::Connection, ClaimRequest, CompleteRecoveryPointSpec, DestinationSecretBindingSpec,
    DestinationSecretRoleV2, DestinationSpec, OperationalStore, ReplicaRepairSecretRefsV2,
    ReplicaRepairWorkSpec,
};
use restoreplane_core::{ArtifactPartRole, ConsistencyGrade, Engine, PartSpec};
use restoreplane_domain::{DestinationId, RecoveryPointId, SecretRefId};
use restoreplane_pipeline::{
    EncodingEvidenceV2, ManifestPartV2, NativeToolIdentityV2, PipelineCfg, ReaderStatusV2,
    RecoveryOriginV2, ReplicaCommitmentV2, ReplicaPublishError, ReplicaPublishOutcomeV2,
    ReplicaRequirementV2, SetManifestV2, SignedSetManifestV2, UploadReceipt,
};
use restoreplane_runtime::ReplicaRepairResolverV2;
use restoreplane_service::ipc::WindowsSid;
use restoreplane_service::repair::ServiceReplicaRepairResolverV2;
use restoreplane_service::secrets::{AclPolicy, SecretBuffer, SecretPurpose, ServiceSecretStore};
use tempfile::TempDir;
use uuid::Uuid;

#[tokio::test]
async fn resolver_uses_pinned_role_separated_versions_and_catalog_manifest_identity() {
    let fixture = Fixture::new();
    let pinned_refs = fixture
        .store
        .configuration()
        .replica_repair_secret_refs(&fixture.source_fingerprint, &fixture.target_fingerprint)
        .expect("capture current repair credential versions");
    let upsert = fixture
        .store
        .runtime()
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            fixture.recovery_point_id.clone(),
            &fixture.source_fingerprint,
            &fixture.target_fingerprint,
            pinned_refs,
            500,
        ))
        .expect("enqueue repair");

    let writer_v2 = fixture.rotate_secret(
        &fixture.writer_v1,
        SecretPurpose::BackupWriter,
        br#"{"schemaVersion":1,"accessKeyId":"WRITER_V2","secretAccessKey":"writer-v2"}"#,
        600,
    );
    let reader_v2 = fixture.rotate_secret(
        &fixture.reader_v1,
        SecretPurpose::RestoreReader,
        br#"{"schemaVersion":1,"accessKeyId":"READER_V2","secretAccessKey":"reader-v2"}"#,
        601,
    );
    fixture.activate_target_credentials(&writer_v2, &reader_v2);
    let rotated_refs = fixture
        .store
        .configuration()
        .replica_repair_secret_refs(&fixture.source_fingerprint, &fixture.target_fingerprint)
        .expect("capture rotated repair credential versions");
    assert_eq!(rotated_refs.target_writer_secret_ref_id(), Some(&writer_v2));
    assert_eq!(rotated_refs.target_reader_secret_ref_id(), Some(&reader_v2));
    let rotated_upsert = fixture
        .store
        .runtime()
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            fixture.recovery_point_id.clone(),
            &fixture.source_fingerprint,
            &fixture.target_fingerprint,
            rotated_refs,
            900,
        ))
        .expect("enqueue repair with rotated credential versions");
    assert_ne!(
        rotated_upsert.operation_run_id(),
        upsert.operation_run_id(),
        "credential rotation must produce a distinct immutable operation identity"
    );
    fixture
        .secrets
        .revoke(&writer_v2, 602)
        .expect("revoke current writer to detect fallback");
    fixture
        .secrets
        .revoke(&reader_v2, 603)
        .expect("revoke current reader to detect fallback");
    let revoked_current = fixture
        .store
        .configuration()
        .replica_repair_secret_refs(&fixture.source_fingerprint, &fixture.target_fingerprint)
        .expect_err("new repair work must not capture a revoked current credential");
    assert_eq!(revoked_current.code(), "OPERATIONAL_SECURITY_INPUT_INVALID");

    let claim = fixture
        .store
        .runtime()
        .claim_next(ClaimRequest::new("repair-worker", "boot-a", 700, 1_000))
        .expect("claim repair")
        .expect("repair work");
    assert_eq!(claim.work_item_id(), upsert.work_item_id());
    let execution = fixture
        .store
        .runtime()
        .load_claimed_replica_repair_v2(&claim, 700)
        .expect("load authoritative repair inputs");
    assert_eq!(
        execution.target_writer_secret_ref_id(),
        Some(&fixture.writer_v1)
    );
    assert_eq!(
        execution.target_reader_secret_ref_id(),
        Some(&fixture.reader_v1)
    );
    assert_eq!(execution.source_reader_secret_ref_id(), None);
    assert_eq!(
        execution.source_manifest_object_key(),
        fixture.source_manifest_key()
    );
    assert_eq!(
        execution.source_manifest_checksum_blake3(),
        blake3::hash(fixture.signed_json.as_bytes())
            .to_hex()
            .as_str()
    );
    assert_eq!(execution.signing_key_id(), "signing-key-01");

    let resolver = fixture.resolver(BTreeMap::from([(
        "signing-key-01".to_owned(),
        fixture.signing_key.verifying_key(),
    )]));
    resolver
        .resolve(&execution)
        .await
        .expect("retired pinned versions remain usable by queued repair");

    let wrong_key_resolver = fixture.resolver(BTreeMap::from([(
        "signing-key-01".to_owned(),
        SigningKey::from_bytes(&[99_u8; 32]).verifying_key(),
    )]));
    let error = match wrong_key_resolver.resolve(&execution).await {
        Ok(_) => panic!("Catalog signing key ID must resolve to the original pinned public key"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "REPLICA_REPAIR_MANIFEST_INVALID");
}

#[tokio::test]
async fn resolver_rejects_writer_reader_role_substitution_without_fallback() {
    let fixture = Fixture::new();
    fixture
        .store
        .runtime()
        .enqueue_replica_repair(ReplicaRepairWorkSpec::new(
            fixture.recovery_point_id.clone(),
            &fixture.source_fingerprint,
            &fixture.target_fingerprint,
            ReplicaRepairSecretRefsV2::new(
                None,
                Some(fixture.reader_v1.clone()),
                Some(fixture.reader_v1.clone()),
            ),
            500,
        ))
        .expect("enqueue role-substituted repair fixture");

    let claim = fixture
        .store
        .runtime()
        .claim_next(ClaimRequest::new("repair-worker", "boot-b", 700, 1_000))
        .expect("claim repair")
        .expect("repair work");
    let execution = fixture
        .store
        .runtime()
        .load_claimed_replica_repair_v2(&claim, 700)
        .expect("load substituted repair inputs");
    let resolver = fixture.resolver(BTreeMap::from([(
        "signing-key-01".to_owned(),
        fixture.signing_key.verifying_key(),
    )]));
    let error = match resolver.resolve(&execution).await {
        Ok(_) => panic!("restore-reader credential must not satisfy backup-writer role"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "SECRET_STORE_PURPOSE_MISMATCH");
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    secrets: Arc<ServiceSecretStore>,
    recovery_point_id: RecoveryPointId,
    target_id: DestinationId,
    source_fingerprint: String,
    target_fingerprint: String,
    signed: SignedSetManifestV2,
    signed_json: String,
    signing_key: SigningKey,
    writer_v1: SecretRefId,
    reader_v1: SecretRefId,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("operational store");
        let acl = AclPolicy::new(
            WindowsSid::parse("S-1-5-80-1001").expect("service SID"),
            current_user_sid(),
        )
        .expect("ACL");
        let secrets = Arc::new(
            ServiceSecretStore::open(temp.path().join("secret-store"), store.secret_refs(), acl)
                .expect("secret store"),
        );
        let source_id = DestinationId::new();
        let target_id = DestinationId::new();
        let source_fingerprint = format!("local:{}", source_id.as_str());
        let target_fingerprint = format!("s3:{}", target_id.as_str());
        let (signed, payload, signing_key) =
            repair_fixture(&source_fingerprint, &target_fingerprint);
        let signed_json = signed.to_json().expect("signed JSON");
        let recovery_point_id =
            RecoveryPointId::from_str(&signed.manifest.recovery_point_id.to_string())
                .expect("recovery point ID");
        let source_root = temp.path().join("source");
        seed_source(
            &source_root,
            &signed,
            signed_json.as_bytes(),
            &payload,
            &source_fingerprint,
        );
        seed_catalog(
            &store,
            &signed,
            &signed_json,
            &source_fingerprint,
            &target_fingerprint,
            signing_key.verifying_key(),
        );
        store
            .configuration()
            .insert_destination(DestinationSpec::local(
                source_id.clone(),
                "repair-source",
                serde_json::json!({"path": source_root}).to_string(),
                10,
            ))
            .expect("insert source destination");
        store
            .configuration()
            .insert_destination(DestinationSpec::s3(
                target_id.clone(),
                "repair-target",
                r#"{
                    "endpoint":"http://127.0.0.1:9000",
                    "region":"us-east-1",
                    "bucket":"backup-bucket",
                    "prefix":"objects",
                    "objectLockMode":"none",
                    "retentionDays":0,
                    "minDwellDays":0,
                    "checksumAlgo":null,
                    "complianceOptIn":false,
                    "provider":"minio"
                }"#,
                11,
            ))
            .expect("insert target destination");
        let ready = Connection::open(&db_path).expect("open destination state");
        ready
            .execute(
                "UPDATE destinations SET state = 'ready' WHERE id IN (?1, ?2)",
                [source_id.as_str(), target_id.as_str()],
            )
            .expect("mark destinations ready");
        let writer_v1 = create_secret(
            &secrets,
            SecretPurpose::BackupWriter,
            br#"{"schemaVersion":1,"accessKeyId":"WRITER_V1","secretAccessKey":"writer-v1"}"#,
            100,
        );
        let reader_v1 = create_secret(
            &secrets,
            SecretPurpose::RestoreReader,
            br#"{"schemaVersion":1,"accessKeyId":"READER_V1","secretAccessKey":"reader-v1"}"#,
            101,
        );
        bind(
            &store,
            &target_id,
            DestinationSecretRoleV2::BackupWriter,
            &writer_v1,
            110,
        );
        bind(
            &store,
            &target_id,
            DestinationSecretRoleV2::RestoreReader,
            &reader_v1,
            111,
        );
        Self {
            _temp: temp,
            db_path,
            store,
            secrets,
            recovery_point_id,
            target_id,
            source_fingerprint,
            target_fingerprint,
            signed,
            signed_json,
            signing_key,
            writer_v1,
            reader_v1,
        }
    }

    fn source_manifest_key(&self) -> &str {
        &self
            .signed
            .manifest
            .replicas
            .iter()
            .find(|replica| replica.destination_fingerprint == self.source_fingerprint)
            .expect("source replica")
            .manifest_object_key
    }

    fn rotate_secret(
        &self,
        previous: &SecretRefId,
        purpose: SecretPurpose,
        value: &[u8],
        rotated_at_ms: i64,
    ) -> SecretRefId {
        self.secrets
            .rotate(
                previous,
                purpose,
                SecretBuffer::new(value.to_vec()).expect("secret buffer"),
                rotated_at_ms,
            )
            .expect("rotate secret")
            .id()
            .clone()
    }

    fn activate_target_credentials(&self, writer: &SecretRefId, reader: &SecretRefId) {
        let connection = Connection::open(&self.db_path).expect("open binding rotation");
        connection
            .execute(
                "UPDATE destination_secret_bindings
                 SET secret_ref_id = ?2
                 WHERE destination_id = ?1 AND secret_role = 'backup_writer'",
                [self.target_id.as_str(), writer.as_str()],
            )
            .expect("activate writer");
        connection
            .execute(
                "UPDATE destination_secret_bindings
                 SET secret_ref_id = ?2
                 WHERE destination_id = ?1 AND secret_role = 'restore_reader'",
                [self.target_id.as_str(), reader.as_str()],
            )
            .expect("activate reader");
    }

    fn resolver(
        &self,
        trusted_keys: BTreeMap<String, ed25519_dalek::VerifyingKey>,
    ) -> ServiceReplicaRepairResolverV2 {
        ServiceReplicaRepairResolverV2::new(
            self.store.configuration(),
            Arc::clone(&self.secrets),
            trusted_keys,
            PipelineCfg::default(),
        )
        .expect("repair resolver")
    }
}

fn create_secret(
    secrets: &ServiceSecretStore,
    purpose: SecretPurpose,
    value: &[u8],
    created_at_ms: i64,
) -> SecretRefId {
    secrets
        .create(
            purpose,
            SecretBuffer::new(value.to_vec()).expect("secret buffer"),
            created_at_ms,
        )
        .expect("create secret")
        .id()
        .clone()
}

fn bind(
    store: &OperationalStore,
    destination_id: &DestinationId,
    role: DestinationSecretRoleV2,
    secret_ref_id: &SecretRefId,
    created_at_ms: i64,
) {
    store
        .configuration()
        .bind_destination_secret(DestinationSecretBindingSpec::new(
            destination_id.clone(),
            role,
            secret_ref_id.clone(),
            created_at_ms,
        ))
        .expect("bind destination secret");
}

fn seed_catalog(
    store: &OperationalStore,
    signed: &SignedSetManifestV2,
    signed_json: &str,
    source_fingerprint: &str,
    target_fingerprint: &str,
    verifying_key: ed25519_dalek::VerifyingKey,
) {
    let source_outcome = ReplicaPublishOutcomeV2 {
        destination_fingerprint: source_fingerprint.to_owned(),
        requirement: ReplicaRequirementV2::Required,
        reader_status: ReaderStatusV2::Eligible,
        payload_receipts: vec![UploadReceipt {
            key: signed.manifest.parts[0].object_key.clone(),
            bytes: signed.manifest.parts[0].ciphertext_bytes,
            version: None,
        }],
        manifest_receipt: Some(UploadReceipt {
            key: signed
                .manifest
                .replicas
                .iter()
                .find(|replica| replica.destination_fingerprint == source_fingerprint)
                .expect("source replica")
                .manifest_object_key
                .clone(),
            bytes: u64::try_from(signed_json.len()).expect("manifest size"),
            version: None,
        }),
        attempts: 2,
        result: Ok(()),
    };
    let target_outcome = ReplicaPublishOutcomeV2 {
        destination_fingerprint: target_fingerprint.to_owned(),
        requirement: ReplicaRequirementV2::Required,
        reader_status: ReaderStatusV2::Unavailable,
        payload_receipts: Vec::new(),
        manifest_receipt: None,
        attempts: 1,
        result: Err(ReplicaPublishError::ReaderChecksumMismatch),
    };
    let spec = CompleteRecoveryPointSpec::from_verified_publish(
        signed,
        &[source_outcome, target_outcome],
        &verifying_key,
        100,
    )
    .expect("catalog projection spec");
    store
        .catalog_projection()
        .ingest_complete(spec)
        .expect("ingest degraded snapshot");
}

fn repair_fixture(
    source_fingerprint: &str,
    target_fingerprint: &str,
) -> (SignedSetManifestV2, Vec<u8>, SigningKey) {
    let recovery_point_id = uuid("11111111-1111-4111-8111-111111111111");
    let occurrence_id = uuid("22222222-2222-4222-8222-222222222222");
    let target_id = uuid("33333333-3333-4333-8333-333333333333");
    let prefix = format!("v2/{occurrence_id}/{target_id}/{recovery_point_id}");
    let payload = b"already-encoded-database".to_vec();
    let part = ManifestPartV2::encoded(
        PartSpec::new(
            uuid("44444444-4444-4444-8444-444444444444"),
            ArtifactPartRole::DatabaseDump,
            "orders.dump",
            true,
            "custom",
        )
        .expect("part"),
        EncodingEvidenceV2::new(
            "a".repeat(64),
            blake3::hash(&payload).to_hex().to_string(),
            128,
            u64::try_from(payload.len()).expect("payload size"),
            "zstd",
            "chacha20poly1305-stream",
            1,
        )
        .expect("evidence"),
        format!("{prefix}/parts/orders.dump.enc"),
    )
    .expect("encoded part");
    let replicas = vec![
        ReplicaCommitmentV2::new(
            source_fingerprint,
            "local",
            ReplicaRequirementV2::Required,
            ReaderStatusV2::Eligible,
            "none",
            format!("{prefix}/set-manifest.json"),
        )
        .expect("source replica"),
        ReplicaCommitmentV2::new(
            target_fingerprint,
            "s3",
            ReplicaRequirementV2::Required,
            ReaderStatusV2::Eligible,
            "none",
            format!("{prefix}/set-manifest.json"),
        )
        .expect("target replica"),
    ];
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
        100,
        "signing-key-01",
        NativeToolIdentityV2::new("pg_dump", "17.4", "b".repeat(64)).expect("tool"),
        BTreeMap::new(),
        vec![part],
        replicas,
    )
    .expect("manifest");
    let signing_key = SigningKey::from_bytes(&[3_u8; 32]);
    let signed = SignedSetManifestV2::create(manifest, &signing_key).expect("signed");
    (signed, payload, signing_key)
}

fn seed_source(
    root: &std::path::Path,
    signed: &SignedSetManifestV2,
    signed_json: &[u8],
    payload: &[u8],
    source_fingerprint: &str,
) {
    let part_path = root.join(&signed.manifest.parts[0].object_key);
    std::fs::create_dir_all(part_path.parent().expect("part parent")).expect("source parent");
    std::fs::write(part_path, payload).expect("source payload");
    let manifest_path = root.join(
        &signed
            .manifest
            .replicas
            .iter()
            .find(|replica| replica.destination_fingerprint == source_fingerprint)
            .expect("source replica")
            .manifest_object_key,
    );
    std::fs::write(manifest_path, signed_json).expect("source Manifest");
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).expect("fixture UUID")
}

fn current_user_sid() -> WindowsSid {
    let output = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .expect("whoami");
    assert!(output.status.success());
    let line = String::from_utf8(output.stdout).expect("whoami UTF-8");
    let sid = line
        .trim()
        .trim_matches('"')
        .rsplit("\",\"")
        .next()
        .expect("SID")
        .trim_matches('"');
    WindowsSid::parse(sid).expect("current user SID")
}
