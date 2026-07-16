// Slice-1 CLI 루프 통합 테스트(DB 없음): init → run(봉투) → verify L1 / restore --decrypt-only.
// CLI 커맨드가 감싸는 것과 동일한 라이브러리 코드 경로를 직접 구동한다:
//   Keystore::init/load/unlock_kek/signing_key/wrap_dek/unwrap_dek,
//   pipeline run_artifact + write_signed_manifest_with_wrapset(WrapSet 동봉 + 지속 서명),
//   PinnedKeyVerifier 서명 게이트 + Verifier::l1_file_integrity + Restorer::decrypt_only.
// 실제 DB 대신 StubSource→LocalDest 경로를 쓴다.

use std::path::{Path, PathBuf};

use dbbackup_catalog::{LocalManifestSource, PinnedKeyVerifier};
use dbbackup_core::Engine;
use dbbackup_pipeline::{
    run_artifact, write_signed_manifest_with_wrapset, BackupSource, Manifest, PipelineCfg,
    SignedManifest, StubSource,
};
use dbbackup_secrets::{Credential, Dek, Keystore};
use dbbackup_verify::{
    load_trusted_inputs, ImagePolicy, RestoreTarget, Restorer, Verifier, VerifyResult,
};
use zeroize::Zeroizing;

const TEST_PW: &str = "correct horse battery staple";

fn deterministic_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + 7) as u8).collect()
}

/// init: 고정 테스트 비밀번호로 키스토어를 만들고 디스크에 저장한 뒤 다시 로드해 반환한다.
fn init_keystore(dir: &Path) -> (Keystore, Zeroizing<String>) {
    let pw = Zeroizing::new(TEST_PW.to_string());
    let keyfile = dir.join("dpapi.keyfile");
    let (ks, recovery) = Keystore::init(&pw, keyfile).expect("키스토어 init");
    let ks_path = dir.join("keystore.json");
    ks.save(&ks_path).expect("키스토어 저장");
    // 지속성 증명: 디스크에서 다시 로드한 키스토어로 나머지 흐름을 구동한다.
    let loaded = Keystore::load(&ks_path).expect("키스토어 로드");
    (loaded, recovery)
}

/// run: StubSource 백업 1건을 봉투 경로로 산출한다. 매니페스트 키를 반환.
async fn take_backup(keystore: &Keystore, spool_dir: &Path, payload: &[u8]) -> String {
    // KEK unlock(비번) → 지속 서명키 복구.
    let kek = keystore
        .unlock_kek(&Credential::Password(Zeroizing::new(TEST_PW.to_string())))
        .expect("KEK unlock");
    let signing_key = keystore.signing_key(&kek).expect("서명키 복구");

    let source = StubSource::ok("job-a", "stub-db", payload.to_vec());
    let part = source.plan().await.unwrap().db_parts.remove(0);
    let artifact = source.spawn(&part).unwrap();

    // per-backup DEK → KEK로 감싼 WrapSet(매니페스트 동봉).
    let dek = Dek::generate();
    let wrap_set = keystore.wrap_dek(&kek, &dek, 1).expect("DEK wrap");

    let spool_path = spool_dir.join(format!("{}.spool", part.backup_id));
    let cfg = PipelineCfg::default();
    let commit = run_artifact(artifact, spool_path, dek, &cfg)
        .await
        .expect("파이프라인 성공");

    let manifest = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
    let (mpath, _signed) =
        write_signed_manifest_with_wrapset(&commit.spool_path, manifest, wrap_set, &signing_key)
            .expect("서명 매니페스트");
    mpath.file_name().unwrap().to_string_lossy().into_owned()
}

/// verify 경로: 게이트 통과 → 자격증명으로 DEK 복구 → Verifier L1.
async fn verify_l1(
    keystore: &Keystore,
    spool_dir: &Path,
    key: &str,
    cred: &Credential,
) -> anyhow::Result<VerifyResult> {
    let source = LocalManifestSource::new(spool_dir);
    let verifier = PinnedKeyVerifier::new(keystore.verifying_key().unwrap());
    let inputs = load_trusted_inputs(&source, &verifier, key)?; // 서명 게이트
    let wrap_set = inputs.wrap_set.as_ref().expect("매니페스트에 WrapSet 동봉");
    let kek = keystore.unlock_kek(cred)?;
    let dek = keystore.unwrap_dek(&kek, wrap_set)?;
    let restorer = Restorer {
        source: &source,
        dek,
        docker_bin: PathBuf::from("docker"),
        image_policy: ImagePolicy::default(),
    };
    let verifier_l1 = Verifier { restorer };
    let outcome = verifier_l1.l1_file_integrity(&inputs).await?;
    Ok(outcome.result)
}

// 1) init → run → verify L1 성공(서명 검증 + DEK unwrap + blake3 일치).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn init_run_verify_l1_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let (keystore, _recovery) = init_keystore(tmp.path());
    let payload = deterministic_payload(200 * 1024);
    let key = take_backup(&keystore, &spool_dir, &payload).await;

    let cred = Credential::Password(Zeroizing::new(TEST_PW.to_string()));
    let result = verify_l1(&keystore, &spool_dir, &key, &cred)
        .await
        .expect("L1 검증 실행");
    assert_eq!(result, VerifyResult::Pass, "정상 백업은 L1 통과");
}

// 2) 잘못된 비밀번호 → KEK unlock 실패(패닉 없이 깔끔한 Err, verify 실패로 수렴).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_password_fails_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let (keystore, _recovery) = init_keystore(tmp.path());
    let payload = deterministic_payload(100 * 1024);
    let key = take_backup(&keystore, &spool_dir, &payload).await;

    let bad = Credential::Password(Zeroizing::new("totally-wrong-password".to_string()));
    let res = verify_l1(&keystore, &spool_dir, &key, &bad).await;
    assert!(
        res.is_err(),
        "잘못된 비밀번호는 DEK unwrap 실패로 Err(패닉 없음)"
    );
}

// 3) 복구코드 경로가 동일 DEK를 복구해 L1을 통과시킨다.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_code_unwraps_same_dek() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let (keystore, recovery) = init_keystore(tmp.path());
    let payload = deterministic_payload(150 * 1024);
    let key = take_backup(&keystore, &spool_dir, &payload).await;

    let cred = Credential::Recovery(recovery);
    let result = verify_l1(&keystore, &spool_dir, &key, &cred)
        .await
        .expect("복구코드 L1 검증 실행");
    assert_eq!(
        result,
        VerifyResult::Pass,
        "복구코드 경로도 동일 DEK로 L1 통과"
    );
}

// 4) 변조된 매니페스트 → 서명 게이트가 DEK unwrap·복호 이전에 거부.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tampered_manifest_rejected_before_decrypt() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let (keystore, _recovery) = init_keystore(tmp.path());
    let payload = deterministic_payload(80 * 1024);
    let key = take_backup(&keystore, &spool_dir, &payload).await;

    // 서명 후 본체 1필드 변조 → 서명 검증 실패.
    let path = spool_dir.join(&key);
    let mut signed: SignedManifest =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    signed.manifest.plaintext_bytes += 1;
    std::fs::write(&path, serde_json::to_vec(&signed).unwrap()).unwrap();

    let source = LocalManifestSource::new(&spool_dir);
    let verifier = PinnedKeyVerifier::new(keystore.verifying_key().unwrap());
    let res = load_trusted_inputs(&source, &verifier, &key);
    assert!(
        res.is_err(),
        "변조 매니페스트는 게이트에서 거부(복호 이전, C3)"
    );
}

// 5) restore --decrypt-only가 원본 StubSource 바이트를 재현.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_decrypt_only_reproduces_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let (keystore, _recovery) = init_keystore(tmp.path());
    let payload = deterministic_payload(200 * 1024);
    let key = take_backup(&keystore, &spool_dir, &payload).await;

    let source = LocalManifestSource::new(&spool_dir);
    let verifier = PinnedKeyVerifier::new(keystore.verifying_key().unwrap());
    let inputs = load_trusted_inputs(&source, &verifier, &key).expect("게이트 통과");

    let kek = keystore
        .unlock_kek(&Credential::Password(Zeroizing::new(TEST_PW.to_string())))
        .unwrap();
    let dek = keystore
        .unwrap_dek(&kek, inputs.wrap_set.as_ref().unwrap())
        .unwrap();

    let out_path = tmp.path().join("restored.dump");
    let restorer = Restorer {
        source: &source,
        dek,
        docker_bin: PathBuf::from("docker"),
        image_policy: ImagePolicy::default(),
    };
    restorer
        .restore(&inputs, RestoreTarget::DecryptOnly(out_path.clone()))
        .await
        .expect("decrypt-only 복원");

    let restored = std::fs::read(&out_path).unwrap();
    assert_eq!(restored, payload, "복호 산출물이 원본 평문을 재현해야 함");
}
