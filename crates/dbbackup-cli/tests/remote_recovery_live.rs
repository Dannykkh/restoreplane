//! 실제 S3 API와 SSH/SFTP 서버를 통한 재해복구 드릴.
//!
//! 기본 workspace 테스트에서는 외부 인프라가 없으므로 ignore한다. Windows/Docker Desktop에서는
//! `scripts/test-remote-recovery.ps1`이 격리 컨테이너, writer/reader 분리 신원과 환경변수를 준비한 뒤
//! 이 두 테스트를 실행한다.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use dbbackup_catalog::{
    catalog_rebuild, open_reader_conn, CatalogWriterHandle, ManifestSource, PinnedKeyVerifier,
    RebuildArgs,
};
use dbbackup_core::Engine;
use dbbackup_dest_s3::{S3Cfg, S3Dest, S3ManifestSource, S3Provider, S3ReaderCfg};
use dbbackup_dest_ssh::{HostKeyPolicy, SshAuth, SshDest, SshManifestSource, SshReaderCfg};
use dbbackup_pipeline::{
    run_artifact, upload_resumable, write_signed_manifest, BackupSource, Destination, Manifest,
    PipelineCfg, StubSource,
};
use dbbackup_secrets::{generate_signing_keypair, Dek};
use dbbackup_verify::{
    load_trusted_inputs, ImagePolicy, RestoreTarget, Restorer, Verifier, VerifyResult,
};
use ed25519_dalek::VerifyingKey;
use tempfile::TempDir;
use zeroize::Zeroizing;

struct PreparedBackup {
    _tmp: TempDir,
    payload: Vec<u8>,
    spool_path: PathBuf,
    spool_key: String,
    manifest_path: PathBuf,
    manifest_key: String,
    dek: Dek,
    verifying_key: VerifyingKey,
}

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} 환경변수가 필요합니다"))
}

fn unique_prefix(protocol: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    format!(
        "dbbackup-recovery-drill/{protocol}-{}-{nanos}",
        std::process::id()
    )
}

fn deterministic_payload(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index * 31 + 7) as u8).collect()
}

async fn prepare_backup() -> PreparedBackup {
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = deterministic_payload(200 * 1024);
    let source = StubSource::ok("remote-recovery", "stub-db", payload.clone());
    let part = source.plan().await.expect("stub plan").db_parts.remove(0);
    let artifact = source.spawn(&part).expect("stub artifact");
    let dek = Dek::generate();
    let spool_path = tmp.path().join(format!("{}.spool", part.backup_id));
    let commit = run_artifact(
        artifact,
        spool_path.clone(),
        dek.clone(),
        &PipelineCfg::default(),
    )
    .await
    .expect("backup pipeline");
    let (signing_key, verifying_key) = generate_signing_keypair();
    let manifest = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
    let (manifest_path, _) =
        write_signed_manifest(&commit.spool_path, manifest, &signing_key).expect("signed manifest");
    let spool_key = spool_path
        .file_name()
        .expect("spool filename")
        .to_string_lossy()
        .into_owned();
    let manifest_key = manifest_path
        .file_name()
        .expect("manifest filename")
        .to_string_lossy()
        .into_owned();

    PreparedBackup {
        _tmp: tmp,
        payload,
        spool_path,
        spool_key,
        manifest_path,
        manifest_key,
        dek,
        verifying_key,
    }
}

async fn upload_backup(destination: &dyn Destination, backup: &PreparedBackup) {
    upload_resumable(destination, &backup.spool_path, &backup.spool_key)
        .await
        .expect("ciphertext upload");
    upload_resumable(destination, &backup.manifest_path, &backup.manifest_key)
        .await
        .expect("signed manifest upload");
}

async fn assert_remote_recovery(
    source: &dyn ManifestSource,
    backup: &PreparedBackup,
    destination_kind: &str,
) {
    assert_eq!(
        source.list_manifests(None).await.expect("manifest list"),
        std::slice::from_ref(&backup.manifest_key)
    );

    let catalog_home = tempfile::tempdir().expect("catalog tempdir");
    let catalog_path = catalog_home.path().join("catalog.db");
    let writer = CatalogWriterHandle::spawn(catalog_path.clone()).expect("catalog writer");
    let pinned = PinnedKeyVerifier::new(backup.verifying_key);
    let report = catalog_rebuild(
        source,
        &pinned,
        &writer,
        RebuildArgs {
            dest_name: format!("{destination_kind}-drill"),
            dest_kind: destination_kind.to_string(),
            ..Default::default()
        },
    )
    .await
    .expect("catalog rebuild from remote");
    assert_eq!(report.accepted, 1);
    assert!(report.rejected_unsigned_or_tampered.is_empty());

    let conn = open_reader_conn(&catalog_path).expect("catalog reader");
    let backup_rows: i64 = conn
        .query_row("SELECT count(*) FROM backups", [], |row| row.get(0))
        .expect("backup count");
    assert_eq!(backup_rows, 1);

    let inputs = load_trusted_inputs(source, &pinned, &backup.manifest_key)
        .await
        .expect("pinned signature gate");
    let verifier = Verifier {
        restorer: Restorer {
            source,
            dek: backup.dek.clone(),
            docker_bin: PathBuf::from("docker"),
            image_policy: ImagePolicy::default(),
        },
    };
    let outcome = verifier
        .l1_file_integrity(&inputs)
        .await
        .expect("remote L1 verify");
    assert_eq!(outcome.result, VerifyResult::Pass, "{}", outcome.notes);

    let restored_path = catalog_home.path().join("restored.dump");
    Restorer {
        source,
        dek: backup.dek.clone(),
        docker_bin: PathBuf::from("docker"),
        image_policy: ImagePolicy::default(),
    }
    .restore(&inputs, RestoreTarget::DecryptOnly(restored_path.clone()))
    .await
    .expect("remote decrypt-only restore");
    assert_eq!(
        std::fs::read(restored_path).expect("restored bytes"),
        backup.payload
    );
}

fn s3_cfg(prefix: String, access_key_id: String, secret_access_key: String) -> S3Cfg {
    S3Cfg {
        endpoint: required_env("S3_ENDPOINT"),
        region: required_env("S3_REGION"),
        bucket: required_env("S3_BUCKET"),
        prefix,
        access_key_id,
        secret_access_key,
        object_lock_mode: dbbackup_pipeline::ImmutabilityKind::ObjectLockGovernance,
        retention_days: 1,
        min_dwell_days: 1,
        checksum_algo: Some("crc32c".into()),
        compliance_opt_in: false,
        provider: S3Provider::MinIO,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires scripts/test-remote-recovery.ps1 MinIO fixture"]
async fn s3_reader_rebuilds_verifies_and_restores_without_write_permission() {
    let backup = prepare_backup().await;
    let prefix = unique_prefix("s3");
    let writer_access = required_env("AWS_ACCESS_KEY_ID");
    let writer_secret = required_env("AWS_SECRET_ACCESS_KEY");
    let reader_access = required_env("S3_READER_ACCESS_KEY_ID");
    let reader_secret = required_env("S3_READER_SECRET_ACCESS_KEY");

    let writer = S3Dest::new(
        "s3-writer",
        s3_cfg(prefix.clone(), writer_access, writer_secret),
    )
    .expect("S3 writer");
    upload_backup(&writer, &backup).await;

    let source = S3ManifestSource::new(S3ReaderCfg {
        endpoint: required_env("S3_ENDPOINT"),
        region: required_env("S3_REGION"),
        bucket: required_env("S3_BUCKET"),
        prefix: prefix.clone(),
        access_key_id: reader_access.clone(),
        secret_access_key: reader_secret.clone(),
    })
    .expect("S3 reader source");
    assert_remote_recovery(&source, &backup, "s3").await;

    let reader_as_writer = S3Dest::new(
        "s3-reader-must-not-write",
        s3_cfg(prefix, reader_access, reader_secret),
    )
    .expect("S3 reader write probe");
    let denied = upload_resumable(
        &reader_as_writer,
        &backup.manifest_path,
        "reader-write-must-fail.manifest.json",
    )
    .await;
    assert!(
        denied.is_err(),
        "readonly S3 reader가 쓰기에 성공하면 안 됨"
    );
}

fn ssh_password(password: String) -> SshAuth {
    SshAuth::Password {
        password: Zeroizing::new(password),
    }
}

fn ssh_writer(name: &str, user: String, password: String, remote_dir: String) -> SshDest {
    let host = required_env("SSH_HOST");
    let port = required_env("SSH_PORT")
        .parse::<u16>()
        .expect("SSH_PORT u16");
    let policy = HostKeyPolicy::new(required_env("SSH_HOST_FPR"), format!("{host}:{port}"));
    SshDest::new(
        name,
        host,
        port,
        user,
        policy,
        ssh_password(password),
        remote_dir,
        false,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires scripts/test-remote-recovery.ps1 OpenSSH/SFTP fixture"]
async fn ssh_reader_rebuilds_verifies_and_restores_without_write_permission() {
    let backup = prepare_backup().await;
    let remote_dir = required_env("SSH_REMOTE_DIR");
    let writer_user = required_env("SSH_WRITER_USER");
    let writer_password = required_env("SSH_WRITER_PASSWORD");
    let reader_user = required_env("SSH_READER_USER");
    let reader_password = required_env("SSH_READER_PASSWORD");

    let writer = ssh_writer(
        "ssh-writer",
        writer_user,
        writer_password,
        remote_dir.clone(),
    );
    upload_backup(&writer, &backup).await;

    let host = required_env("SSH_HOST");
    let port = required_env("SSH_PORT")
        .parse::<u16>()
        .expect("SSH_PORT u16");
    let source = SshManifestSource::new(SshReaderCfg {
        host: host.clone(),
        port,
        user: reader_user.clone(),
        host_key_fingerprint: required_env("SSH_HOST_FPR"),
        auth: ssh_password(reader_password.clone()),
        remote_dir: remote_dir.clone(),
    });
    assert_remote_recovery(&source, &backup, "ssh").await;

    let reader_as_writer = ssh_writer(
        "ssh-reader-must-not-write",
        reader_user,
        reader_password,
        remote_dir,
    );
    let denied = upload_resumable(
        &reader_as_writer,
        &backup.manifest_path,
        "reader-write-must-fail.manifest.json",
    )
    .await;
    assert!(
        denied.is_err(),
        "readonly SSH reader가 쓰기에 성공하면 안 됨"
    );
}
