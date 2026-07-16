// validate() 교차참조/필수필드 검증 케이스.
use dbbackup_config::schema::{Config, Database, Destination, Job, Retention, VerifyPolicy};
use dbbackup_config::{validate, ValidationError};
use dbbackup_core::{ChecksumAlgo, DestKind, EncField, Engine, ImmutabilityCapability, ObjectLockMode};

fn enc() -> EncField {
    EncField::new(1, b"secret".to_vec())
}

fn db(alias: &str) -> Database {
    Database {
        alias: alias.to_string(),
        engine: Engine::Postgresql,
        host: "127.0.0.1".to_string(),
        port: 5432,
        dbname: "app".to_string(),
        user: "backup_reader".to_string(),
        password: enc(),
        tunnel: None,
        dump_globals: true,
        single_transaction: None,
    }
}

fn local_dest(name: &str) -> Destination {
    Destination {
        name: name.to_string(),
        kind: DestKind::Local,
        endpoint: None,
        region: None,
        bucket: None,
        prefix: String::new(),
        access_key_id: None,
        secret_access_key: None,
        checksum_algo: None,
        object_lock: None,
        tunnel: None,
        host: None,
        port: None,
        user: None,
        private_key_path: None,
        host_key_fingerprint: None,
        path: Some("D:/backup-spool".into()),
    }
}

fn job(name: &str, source: &str, dests: &[&str]) -> Job {
    Job {
        name: name.to_string(),
        source: source.to_string(),
        destinations: dests.iter().map(|s| s.to_string()).collect(),
        required_immutability: ImmutabilityCapability::None,
        retention: Retention::default(),
        verify: VerifyPolicy::default(),
        schedule: "0 3 * * *".to_string(),
        enabled: true,
    }
}

#[test]
fn empty_config_is_valid() {
    assert!(validate(&Config::empty()).is_ok());
}

#[test]
fn consistent_config_is_valid() {
    let mut config = Config::empty();
    config.databases.push(db("app"));
    config.destinations.push(local_dest("local1"));
    config.jobs.push(job("nightly", "app", &["local1"]));
    assert!(validate(&config).is_ok());
}

#[test]
fn duplicate_database_alias_is_caught() {
    let mut config = Config::empty();
    config.databases.push(db("app"));
    config.databases.push(db("app"));
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::DuplicateDatabaseAlias("app".to_string())));
}

#[test]
fn job_with_unknown_destination_is_caught() {
    let mut config = Config::empty();
    config.databases.push(db("app"));
    config.jobs.push(job("nightly", "app", &["nope"]));
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::UnknownDestinationRef {
        job: "nightly".to_string(),
        dest: "nope".to_string(),
    }));
}

#[test]
fn job_with_unknown_source_is_caught() {
    let mut config = Config::empty();
    config.destinations.push(local_dest("local1"));
    config.jobs.push(job("nightly", "ghost", &["local1"]));
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::UnknownSourceRef {
        job: "nightly".to_string(),
        source_db: "ghost".to_string(),
    }));
}

#[test]
fn database_with_unknown_tunnel_is_caught() {
    let mut config = Config::empty();
    let mut d = db("app");
    d.tunnel = Some("bastion-missing".to_string());
    config.databases.push(d);
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::UnknownTunnelRef {
        db: "app".to_string(),
        tunnel: "bastion-missing".to_string(),
    }));
}

#[test]
fn s3_object_lock_without_checksum_is_caught() {
    let mut config = Config::empty();
    let mut d = local_dest("s3");
    d.kind = DestKind::S3;
    d.endpoint = Some("https://s3.example.com".to_string());
    d.bucket = Some("bucket".to_string());
    d.access_key_id = Some(enc());
    d.secret_access_key = Some(enc());
    d.checksum_algo = None;
    d.object_lock = Some(dbbackup_config::schema::ObjectLockConfig {
        mode: ObjectLockMode::Governance,
        retention_days: 35,
    });
    config.destinations.push(d);
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::S3ChecksumRequiredForObjectLock("s3".to_string())));
}

#[test]
fn s3_missing_required_fields_are_all_collected() {
    let mut config = Config::empty();
    let mut d = local_dest("s3");
    d.kind = DestKind::S3;
    d.path = None;
    config.destinations.push(d);
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::MissingS3Field("s3".to_string(), "endpoint")));
    assert!(errors.contains(&ValidationError::MissingS3Field("s3".to_string(), "bucket")));
    assert!(errors.contains(&ValidationError::MissingS3Field("s3".to_string(), "access_key_id")));
    assert!(errors.contains(&ValidationError::MissingS3Field("s3".to_string(), "secret_access_key")));
}

#[test]
fn ssh_dest_missing_fields_are_caught() {
    let mut config = Config::empty();
    let mut d = local_dest("ssh1");
    d.kind = DestKind::Ssh;
    d.path = None;
    config.destinations.push(d);
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::MissingSshField("ssh1".to_string(), "path")));
    assert!(errors.contains(&ValidationError::MissingSshField("ssh1".to_string(), "user")));
    assert!(errors.contains(&ValidationError::MissingSshField("ssh1".to_string(), "host_key_fingerprint")));
}

#[test]
fn local_dest_without_path_is_caught() {
    let mut config = Config::empty();
    let mut d = local_dest("loc");
    d.path = None;
    config.destinations.push(d);
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::MissingLocalPath("loc".to_string())));
}

#[test]
fn invalid_verify_level_is_caught() {
    let mut config = Config::empty();
    config.databases.push(db("app"));
    config.destinations.push(local_dest("local1"));
    let mut j = job("nightly", "app", &["local1"]);
    j.verify = VerifyPolicy { level: 4, schedule: "every_run".to_string() };
    config.jobs.push(j);
    let errors = validate(&config).unwrap_err();
    assert!(errors.contains(&ValidationError::InvalidVerifyLevel("nightly".to_string())));
}

#[test]
fn checksum_variants_are_accepted() {
    // ChecksumAlgo가 스키마에 그대로 들어가는지(컴파일 수준 고정).
    let _ = ChecksumAlgo::Sha256;
    let _ = ChecksumAlgo::Crc32c;
}
