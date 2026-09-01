// S09 시나리오 5 — L1 파일 무결성. 파이프라인으로 실제 백업(StubSource→인코딩 체인→스풀) 후
// Verifier L1이 통과하고, 스풀 1바이트를 변조하면 L1이 실패하는지 확인(Docker 불필요, 복원 가능성 증명).

use restoreplane_catalog::{LocalManifestSource, PinnedKeyVerifier};
use restoreplane_core::Engine;
use restoreplane_pipeline::{
    run_artifact, write_signed_manifest, BackupSource, Manifest, PipelineCfg, StubSource,
};
use restoreplane_secrets::{generate_signing_keypair, Dek};
use restoreplane_verify::{load_trusted_inputs, ImagePolicy, Restorer, Verifier, VerifyResult};

fn deterministic_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + 7) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn l1_passes_on_valid_backup_and_fails_on_corruption() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    // 1) 실제 백업 파이프라인 구동(다중 STREAM 청크).
    let payload = deterministic_payload(200 * 1024);
    let source_stub = StubSource::ok("job-a", "stub-db", payload.clone());
    let part = source_stub.plan().await.unwrap().db_parts.remove(0);
    let artifact = source_stub.spawn(&part).unwrap();

    let dek = Dek::generate();
    let spool_path = spool_dir.join(format!("{}.spool", part.backup_id));
    let cfg = PipelineCfg::default();
    let commit = run_artifact(artifact, spool_path.clone(), dek.clone(), &cfg)
        .await
        .expect("파이프라인 성공");

    // 2) 서명 매니페스트 동봉.
    let (sk, vk) = generate_signing_keypair();
    let manifest = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
    let (mpath, _s) = write_signed_manifest(&commit.spool_path, manifest, &sk).unwrap();
    let key = mpath.file_name().unwrap().to_string_lossy().into_owned();

    // 3) 게이트 통과 후 L1 검증.
    let source = LocalManifestSource::new(&spool_dir);
    let verifier_gate = PinnedKeyVerifier::new(vk);
    let inputs = load_trusted_inputs(&source, &verifier_gate, &key)
        .await
        .expect("서명 통과");

    let restorer = Restorer {
        source: &source,
        dek: dek.clone(),
        docker_bin: std::path::PathBuf::from("docker"),
        image_policy: ImagePolicy::default(),
    };
    let verifier = Verifier { restorer };

    let ok = verifier.l1_file_integrity(&inputs).await.unwrap();
    assert_eq!(
        ok.result,
        VerifyResult::Pass,
        "정상 백업 L1 통과: {}",
        ok.notes
    );
    assert_eq!(ok.label(), "verified(L1)");

    // 4) 스풀 1바이트 변조 → L1 실패(STREAM 인증 실패 또는 blake3 불일치).
    let mut bytes = std::fs::read(&spool_path).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    std::fs::write(&spool_path, &bytes).unwrap();

    let bad = verifier.l1_file_integrity(&inputs).await.unwrap();
    assert_eq!(
        bad.result,
        VerifyResult::Fail,
        "변조된 스풀은 L1 실패해야 함"
    );
}
