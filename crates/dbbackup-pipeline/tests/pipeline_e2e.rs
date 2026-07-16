// crates/dbbackup-pipeline/tests/pipeline_e2e.rs
//! DB 없이 전체 백업 경로를 구동하는 end-to-end 테스트.
//! StubSource → 인코딩 체인(blake3 tee → zstd → chacha20poly1305 STREAM) → LocalDest 스풀 →
//! 서명 매니페스트 → 팬아웃. Commit Protocol의 실패 경로(절단-as-success 방지)도 검증한다.

use std::sync::Arc;

use dbbackup_pipeline::{
    assert_job_immutability, decrypt_and_decompress, fan_out, run_artifact, write_signed_manifest,
    BackupSource, Destination, LocalDest, Manifest, PipelineCfg, RequiredImmutability, StubSource,
};
use dbbackup_secrets::{generate_signing_keypair, Dek};

/// 청크 경계를 반드시 넘기는 결정적 payload(STREAM_CHUNK=64KiB 초과).
fn deterministic_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + 7) as u8).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_pipeline_stub_to_localdest_roundtrips_and_manifest_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    // 1) StubSource(DB 없음)로 결정적 바이트 방출.
    let payload = deterministic_payload(200 * 1024); // 다중 STREAM 청크
    let source = StubSource::ok("job-a", "stub-db", payload.clone());
    let part = source.plan().await.unwrap().db_parts.remove(0);
    let artifact = source.spawn(&part).unwrap();

    // 2) 백업별 DEK로 파이프라인 구동 → 스풀 커밋.
    let dek = Dek::generate();
    let spool_path = spool_dir.join(format!("{}.spool", part.backup_id));
    let cfg = PipelineCfg::default();
    let commit = run_artifact(artifact, spool_path.clone(), dek.clone(), &cfg)
        .await
        .expect("파이프라인 성공");

    // 스풀 파일 존재.
    assert!(commit.spool_path.exists(), "스풀 파일이 존재해야 함");
    assert_eq!(commit.plaintext_bytes, payload.len() as u64);
    assert!(commit.ciphertext_bytes > 0);

    // 3) 복호 + zstd decode가 원본 바이트를 재현.
    let recovered = decrypt_and_decompress(&commit.spool_path, &dek).unwrap();
    assert_eq!(recovered, payload, "라운드트립이 원본 평문을 재현해야 함");

    // 4) 평문 blake3가 커밋 해시와 일치.
    let expected = blake3::hash(&payload);
    assert_eq!(&commit.plaintext_blake3, expected.as_bytes());

    // 5) 서명 매니페스트 생성 + 검증.
    let (sk, vk) = generate_signing_keypair();
    let manifest = Manifest::from_commit(&part, dbbackup_core::Engine::Postgresql, &commit, 1);
    let (manifest_path, signed) = write_signed_manifest(&commit.spool_path, manifest, &sk).unwrap();
    assert!(
        manifest_path.exists(),
        "매니페스트 JSON이 스풀 옆에 있어야 함"
    );
    signed.verify(&vk).expect("서명 검증 통과");

    // 변조 감지: 매니페스트 필드를 바꾸면 검증 실패.
    let mut tampered = signed.clone();
    tampered.manifest.plaintext_bytes += 1;
    assert!(
        tampered.verify(&vk).is_err(),
        "변조된 매니페스트는 검증 실패해야 함"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_protocol_nonzero_exit_discards_spool() {
    // 자식이 EOF에 도달하더라도 exit≠0이면 스풀을 남기면 안 된다(truncation-as-success 방지, R2).
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let payload = deterministic_payload(128 * 1024);
    let source = StubSource::failing("job-fail", "stub-db", payload);
    let part = source.plan().await.unwrap().db_parts.remove(0);
    let artifact = source.spawn(&part).unwrap();

    let dek = Dek::generate();
    let spool_path = spool_dir.join("fail.spool");
    let cfg = PipelineCfg::default();
    let res = run_artifact(artifact, spool_path.clone(), dek, &cfg).await;

    assert!(res.is_err(), "실패 소스는 커밋되면 안 됨");
    assert!(
        !spool_path.exists(),
        "실패 시 스풀 파일이 남으면 안 됨(커밋 미확정)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fanout_partial_success_one_unwritable_dest() {
    // 목적지 2개 중 하나는 root가 파일이라 쓸 수 없다 → 부분 성공 벡터.
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let payload = deterministic_payload(100 * 1024);
    let source = StubSource::ok("job-fan", "stub-db", payload);
    let part = source.plan().await.unwrap().db_parts.remove(0);
    let artifact = source.spawn(&part).unwrap();

    let dek = Dek::generate();
    let spool_path = spool_dir.join("fan.spool");
    let cfg = PipelineCfg {
        upload_retries: 1,
        ..PipelineCfg::default()
    };
    let commit = run_artifact(artifact, spool_path, dek, &cfg).await.unwrap();

    // 정상 목적지.
    let ok_dir = tmp.path().join("dest_ok");
    std::fs::create_dir_all(&ok_dir).unwrap();
    let ok_dest = Arc::new(LocalDest::new("local-ok", ok_dir.clone())) as Arc<dyn Destination>;

    // 쓸 수 없는 목적지: root 경로에 파일이 존재 → create_dir_all 실패.
    let bad_root = tmp.path().join("dest_bad");
    std::fs::write(&bad_root, b"i am a file, not a dir").unwrap();
    let bad_dest = Arc::new(LocalDest::new("local-bad", bad_root.clone())) as Arc<dyn Destination>;

    let dests = vec![ok_dest, bad_dest];

    // Job 불변성 assert: LocalDest는 None이므로 required=None은 통과, ObjectLock은 실패.
    assert!(assert_job_immutability(RequiredImmutability::None, &dests).is_ok());
    assert!(assert_job_immutability(RequiredImmutability::ObjectLock, &dests).is_err());

    let outcomes = fan_out(&commit, dests, &cfg).await;
    assert_eq!(outcomes.len(), 2);
    let ok_count = outcomes.iter().filter(|o| o.result.is_ok()).count();
    let err_count = outcomes.iter().filter(|o| o.result.is_err()).count();
    assert_eq!(ok_count, 1, "정상 목적지 1개는 성공해야 함");
    assert_eq!(err_count, 1, "쓸 수 없는 목적지 1개는 실패해야 함");

    // 성공한 목적지에 실제 객체가 존재(부분 객체가 아니라 최종 커밋된 파일).
    let landed = ok_dir.join("fan.spool");
    assert!(landed.exists(), "성공 목적지에 최종 객체가 있어야 함");
    // 부분 파일(.part)은 남지 않아야 함.
    assert!(
        !ok_dir.join("fan.part").exists(),
        "커밋 후 .part 임시파일이 남으면 안 됨"
    );
}
