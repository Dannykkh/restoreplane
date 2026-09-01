use std::fs;

use restoreplane_catalog::OperationalStore;
use restoreplane_config::{
    load_v1_shadow_model, ImportedSecretRef, LegacySecretSink, ShadowImportError,
};
use restoreplane_core::EncField;
use restoreplane_domain::SecretRefId;
use tempfile::TempDir;
use uuid::Uuid;

const SECRET_CANARY: &str = "SECRET_CANARY_SHADOW_IMPORT_9f3c";
const SECRET_CANARY_B64: &str = "U0VDUkVUX0NBTkFSWV9TSEFET1dfSU1QT1JUXzlmM2M=";

#[test]
fn v1_shadow_import_is_idempotent_stable_and_never_mutates_production_inputs() {
    let fixture = Fixture::new();
    let config_before = checksum(&fixture.config_path);
    let scheduler_before = checksum(&fixture.scheduler_evidence_path);
    let mut secret_sink = DeterministicSecretSink;
    let model =
        load_v1_shadow_model(&fixture.config_path, &mut secret_sink).expect("canonical v1 model");
    let second_model =
        load_v1_shadow_model(&fixture.config_path, &mut secret_sink).expect("stable model replay");
    assert_eq!(model, second_model);

    let store =
        OperationalStore::open(fixture.shadow_db_path.clone()).expect("open isolated shadow DB");
    let repository = store.shadow_import();
    let first = repository
        .import_v1(model.clone(), 1_000)
        .expect("first shadow import");
    let replay = repository
        .import_v1(model.clone(), 1_100)
        .expect("idempotent shadow replay");
    let second_shadow_path = fixture
        ._temp_dir
        .path()
        .join("second-shadow-operational.db");
    let second_store =
        OperationalStore::open(second_shadow_path.clone()).expect("open second shadow DB");
    let independent = second_store
        .shadow_import()
        .import_v1(model, 9_900)
        .expect("independent stable shadow import");

    assert!(first.created());
    assert!(!replay.created());
    assert!(independent.created());
    assert_eq!(
        first.relation_digest_blake3(),
        replay.relation_digest_blake3()
    );
    assert_eq!(
        first.relation_digest_blake3(),
        independent.relation_digest_blake3()
    );
    assert_eq!(first.diff_json(), replay.diff_json());
    assert_eq!(first.diff_json(), independent.diff_json());
    assert_eq!(first.connection_count(), 1);
    assert_eq!(first.target_count(), 2);
    assert_eq!(first.destination_count(), 1);
    assert_eq!(first.plan_count(), 2);
    assert_eq!(first.secret_ref_count(), 1);
    assert_eq!(repository.import_run_count().expect("import run count"), 1);

    assert_eq!(checksum(&fixture.config_path), config_before);
    assert_eq!(checksum(&fixture.scheduler_evidence_path), scheduler_before);
    assert_shadow_files_do_not_contain_secrets(&fixture.shadow_db_path);
    assert_shadow_files_do_not_contain_secrets(&second_shadow_path);
}

#[test]
fn failed_shadow_transaction_rolls_back_and_the_clean_model_can_be_reimported() {
    let fixture = Fixture::new();
    let config_before = checksum(&fixture.config_path);
    let scheduler_before = checksum(&fixture.scheduler_evidence_path);
    let mut secret_sink = DeterministicSecretSink;
    let model =
        load_v1_shadow_model(&fixture.config_path, &mut secret_sink).expect("canonical v1 model");
    let mut colliding_model = model.clone();
    let mut duplicate_destination = colliding_model.destinations[0].clone();
    duplicate_destination.key.push_str("|mid-transaction-fault");
    colliding_model.destinations.push(duplicate_destination);

    let store =
        OperationalStore::open(fixture.shadow_db_path.clone()).expect("open isolated shadow DB");
    let repository = store.shadow_import();
    let error = repository
        .import_v1(colliding_model, 2_000)
        .expect_err("duplicate destination display name must fail after earlier inserts");
    assert_eq!(error.code(), "OPERATIONAL_REPOSITORY_SQLITE");
    assert_eq!(repository.import_run_count().expect("rolled-back count"), 0);

    let recovered = repository
        .import_v1(model, 2_100)
        .expect("clean replay after transactional failure");
    assert!(recovered.created());
    assert_eq!(recovered.connection_count(), 1);
    assert_eq!(recovered.destination_count(), 1);
    assert_eq!(repository.import_run_count().expect("successful count"), 1);
    assert_eq!(checksum(&fixture.config_path), config_before);
    assert_eq!(checksum(&fixture.scheduler_evidence_path), scheduler_before);
    assert_shadow_files_do_not_contain_secrets(&fixture.shadow_db_path);
}

struct DeterministicSecretSink;

impl LegacySecretSink for DeterministicSecretSink {
    fn import_encrypted_bundle(
        &mut self,
        purpose: &str,
        fields: &[&EncField],
    ) -> Result<ImportedSecretRef, ShadowImportError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(purpose.as_bytes());
        for field in fields {
            hasher.update(&field.wrap_gen.to_le_bytes());
            hasher.update(&(field.ciphertext.len() as u64).to_le_bytes());
            hasher.update(&field.ciphertext);
        }
        let fingerprint = hasher.finalize().to_hex().to_string();
        let id = SecretRefId::from_stable_name(Uuid::NAMESPACE_URL, fingerprint.as_bytes());
        Ok(ImportedSecretRef::external(
            id.to_string(),
            format!("test-secret/{id}"),
            fingerprint,
            purpose,
            1,
        ))
    }
}

struct Fixture {
    _temp_dir: TempDir,
    config_path: std::path::PathBuf,
    scheduler_evidence_path: std::path::PathBuf,
    shadow_db_path: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let config_path = temp_dir.path().join("production-restoreplane.toml");
        let scheduler_evidence_path = temp_dir.path().join("production-task.xml");
        let shadow_db_path = temp_dir.path().join("shadow-operational.db");
        fs::write(
            &config_path,
            format!(
                r#"[meta]
schema_version = 1

[[database]]
alias = "orders"
engine = "postgresql"
host = "DB.INTERNAL"
port = 5432
dbname = "orders"
user = "backup"
password = "enc:1:{SECRET_CANARY_B64}"

[[database]]
alias = "billing"
engine = "postgresql"
host = "db.internal"
port = 5432
dbname = "billing"
user = "backup"
password = "enc:1:{SECRET_CANARY_B64}"

[[destination]]
name = "local-vault"
kind = "local"
path = "D:/backup-vault"

[[jobs]]
name = "orders-nightly"
source = "orders"
destinations = ["local-vault"]
required_immutability = "none"
schedule = "0 2 * * *"

[[jobs]]
name = "billing-nightly"
source = "billing"
destinations = ["local-vault"]
required_immutability = "none"
schedule = "30 2 * * *"

[policy]
spool_dir = "D:/spool"
"#
            ),
        )
        .expect("write legacy config fixture");
        fs::write(
            &scheduler_evidence_path,
            "<Task><URI>restoreplane-legacy</URI></Task>",
        )
        .expect("write scheduler evidence fixture");
        Self {
            _temp_dir: temp_dir,
            config_path,
            scheduler_evidence_path,
            shadow_db_path,
        }
    }
}

fn checksum(path: &std::path::Path) -> String {
    blake3::hash(&fs::read(path).expect("read fixture"))
        .to_hex()
        .to_string()
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn assert_shadow_files_do_not_contain_secrets(database_path: &std::path::Path) {
    for suffix in ["", "-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", database_path.display()));
        if !path.exists() {
            continue;
        }
        let bytes = fs::read(&path).expect("read shadow SQLite file");
        assert!(
            !contains_bytes(&bytes, SECRET_CANARY.as_bytes()),
            "plaintext secret canary leaked into {}",
            path.display()
        );
        assert!(
            !contains_bytes(&bytes, b"enc:1:"),
            "encrypted field envelope leaked into {}",
            path.display()
        );
        assert!(
            !contains_bytes(&bytes, SECRET_CANARY_B64.as_bytes()),
            "encrypted payload canary leaked into {}",
            path.display()
        );
    }
}
