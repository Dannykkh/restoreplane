// S09 시나리오 1~2 — 출처 인증 게이트. 미서명/변조 매니페스트는 DEK unwrap·복호 이전에 즉시 거부.

use std::path::Path;

use dbbackup_catalog::{LocalManifestSource, PinnedKeyVerifier};
use dbbackup_core::Engine;
use dbbackup_pipeline::{write_signed_manifest, Manifest, SignedManifest, SpoolCommit, StubSource};
use dbbackup_secrets::generate_signing_keypair;
use dbbackup_verify::load_trusted_inputs;
use ed25519_dalek::SigningKey;

fn write_valid(sk: &SigningKey, dir: &Path) -> String {
    let src = StubSource::ok("job-a", "stub-db", vec![]);
    let part = src.make_part();
    let id = part.backup_id;
    let spool_path = dir.join(format!("{id}.spool"));
    let commit = SpoolCommit {
        spool_path: spool_path.clone(),
        plaintext_blake3: [3; 32],
        ciphertext_blake3: [4; 32],
        plaintext_bytes: 10,
        ciphertext_bytes: 20,
        stderr_tail: String::new(),
    };
    let manifest = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
    let (mpath, _s) = write_signed_manifest(&spool_path, manifest, sk).unwrap();
    mpath.file_name().unwrap().to_string_lossy().into_owned()
}

#[tokio::test]
async fn valid_signed_manifest_is_accepted() {
    let (sk, vk) = generate_signing_keypair();
    let verifier = PinnedKeyVerifier::new(vk);
    let dir = tempfile::tempdir().unwrap();
    let key = write_valid(&sk, dir.path());
    let source = LocalManifestSource::new(dir.path());

    let inputs = load_trusted_inputs(&source, &verifier, &key)
        .await
        .expect("유효 서명은 통과");
    assert_eq!(inputs.db_manifest.job, "job-a");
}

#[tokio::test]
async fn unsigned_manifest_is_rejected_before_decrypt() {
    let (_sk, vk) = generate_signing_keypair();
    let verifier = PinnedKeyVerifier::new(vk);
    let dir = tempfile::tempdir().unwrap();

    // 봉투가 아닌 bare Manifest JSON을 목적지에 둔다(미서명).
    let src = StubSource::ok("job-a", "stub-db", vec![]);
    let part = src.make_part();
    let commit = SpoolCommit {
        spool_path: std::path::PathBuf::from("x.spool"),
        plaintext_blake3: [1; 32],
        ciphertext_blake3: [2; 32],
        plaintext_bytes: 1,
        ciphertext_bytes: 2,
        stderr_tail: String::new(),
    };
    let bare = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
    std::fs::write(
        dir.path().join("x.spool.manifest.json"),
        serde_json::to_vec(&bare).unwrap(),
    )
    .unwrap();

    let source = LocalManifestSource::new(dir.path());
    let res = load_trusted_inputs(&source, &verifier, "x.spool.manifest.json").await;
    assert!(res.is_err(), "미서명 매니페스트는 즉시 거부돼야 함(C3)");
}

#[tokio::test]
async fn tampered_manifest_is_rejected() {
    let (sk, vk) = generate_signing_keypair();
    let verifier = PinnedKeyVerifier::new(vk);
    let dir = tempfile::tempdir().unwrap();
    let key = write_valid(&sk, dir.path());

    // 서명 후 본체 1필드 변조 → 서명 검증 실패.
    let path = dir.path().join(&key);
    let mut signed: SignedManifest =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    signed.manifest.plaintext_bytes += 1;
    std::fs::write(&path, serde_json::to_vec(&signed).unwrap()).unwrap();

    let source = LocalManifestSource::new(dir.path());
    let res = load_trusted_inputs(&source, &verifier, &key).await;
    assert!(res.is_err(), "변조된 매니페스트는 거부돼야 함(C3)");
}
