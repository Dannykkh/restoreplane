// S09 시나리오 7~9, 11, 16 — L2/L3는 외부 런타임(pg_restore/Docker)이 필요하므로 전부 #[ignore].
// Docker 있는 게이트에서 `cargo test -- --ignored`로 실행한다. 여기서는 컴파일 + 배선만 보장한다.

use dbbackup_catalog::{LocalManifestSource, PinnedKeyVerifier};
use dbbackup_core::Engine;
use dbbackup_pipeline::{
    run_artifact, write_signed_manifest, BackupSource, Manifest, PipelineCfg, StubSource,
};
use dbbackup_secrets::{generate_signing_keypair, Dek};
use dbbackup_verify::{
    load_trusted_inputs, ImagePolicy, RestoreInputs, Restorer, Verifier, VerifyLevel,
};

async fn make_backup(spool_dir: &std::path::Path) -> (RestoreInputs, Dek, LocalManifestSource) {
    let payload = (0..4096u32).map(|i| i as u8).collect::<Vec<u8>>();
    let stub = StubSource::ok("job-a", "stub-db", payload);
    let part = stub.plan().await.unwrap().db_parts.remove(0);
    let artifact = stub.spawn(&part).unwrap();
    let dek = Dek::generate();
    let spool_path = spool_dir.join(format!("{}.spool", part.backup_id));
    let commit = run_artifact(artifact, spool_path, dek.clone(), &PipelineCfg::default())
        .await
        .unwrap();
    let (sk, vk) = generate_signing_keypair();
    let manifest = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
    let (mpath, _s) = write_signed_manifest(&commit.spool_path, manifest, &sk).unwrap();
    let key = mpath.file_name().unwrap().to_string_lossy().into_owned();
    let source = LocalManifestSource::new(spool_dir);
    let inputs = load_trusted_inputs(&source, &PinnedKeyVerifier::new(vk), &key)
        .await
        .unwrap();
    (inputs, dek, source)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires pg_restore binary (custom-format TOC parsing, L2)"]
async fn l2_format_validity_custom() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();
    let (inputs, dek, source) = make_backup(&spool_dir).await;
    let verifier = Verifier {
        restorer: Restorer {
            source: &source,
            dek,
            docker_bin: "docker".into(),
            image_policy: ImagePolicy::default(),
        },
    };
    // StubSource는 임의 바이트라 실제 custom 덤프가 아니므로 pg_restore가 있어도 fail이 정상.
    let out = verifier.verify(&inputs, VerifyLevel::L2).await.unwrap();
    println!("L2: {} {}", out.label(), out.notes);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Docker + postgres image (ephemeral sandbox restore, L3)"]
async fn l3_sandbox_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();
    let (mut inputs, dek, source) = make_backup(&spool_dir).await;
    // 이미지 선택을 위해 server_version/policy를 배선(원래는 매니페스트/config에서 옴).
    inputs.server_version = Some("PostgreSQL 16.3".to_string());
    let policy = ImagePolicy::default().with("postgresql", 16, "postgres:16");
    let verifier = Verifier {
        restorer: Restorer {
            source: &source,
            dek,
            docker_bin: "docker".into(),
            image_policy: policy,
        },
    };
    let out = verifier.verify(&inputs, VerifyLevel::L3).await.unwrap();
    println!(
        "L3: {} target={:?}",
        out.label(),
        out.restore_target_version
    );
}
