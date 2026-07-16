// S08 시나리오 1~3 — 서명 게이트(미서명/변조/유효) + catalog rebuild 재구성(P0-6).
// 파이프라인 Manifest/SignedManifest(ed25519)를 그대로 재사용해 서명/검증한다.

use std::path::{Path, PathBuf};

use dbbackup_catalog::{
    catalog_rebuild, open_reader_conn, verify_bytes, CatalogWriterHandle, LocalManifestSource,
    PinnedKeyVerifier, RebuildArgs,
};
use dbbackup_core::Engine;
use dbbackup_pipeline::{write_signed_manifest, Manifest, SignedManifest, SpoolCommit, StubSource};
use dbbackup_secrets::generate_signing_keypair;
use ed25519_dalek::SigningKey;

/// 결정적 매니페스트 하나를 만들어 dir에 서명 봉투(.manifest.json)로 쓴다. 반환: (manifest, 매니페스트 키파일명).
fn write_manifest(sk: &SigningKey, dir: &Path, blake_seed: u8) -> (Manifest, String) {
    let src = StubSource::ok("job-a", "stub-db", vec![]);
    let part = src.make_part();
    let id = part.backup_id;
    let spool_path = dir.join(format!("{id}.spool"));
    let commit = SpoolCommit {
        spool_path: spool_path.clone(),
        plaintext_blake3: [blake_seed; 32],
        plaintext_bytes: 100,
        ciphertext_bytes: 120,
        stderr_tail: String::new(),
    };
    let manifest = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
    let (mpath, _signed) = write_signed_manifest(&spool_path, manifest.clone(), sk).unwrap();
    let key = mpath.file_name().unwrap().to_string_lossy().into_owned();
    (manifest, key)
}

#[test]
fn signature_gate_accepts_valid_rejects_unsigned_and_tampered() {
    let (sk, vk) = generate_signing_keypair();
    let verifier = PinnedKeyVerifier::new(vk);
    let tmp = tempfile::tempdir().unwrap();

    // 유효 서명 봉투 → 게이트 통과.
    let (_m, key) = write_manifest(&sk, tmp.path(), 1);
    let bytes = std::fs::read(tmp.path().join(&key)).unwrap();
    assert!(verify_bytes(&bytes, &verifier).is_ok(), "유효 서명은 통과해야 함");

    // 미서명(봉투 아닌 bare Manifest) → Unsigned 거부.
    let src = StubSource::ok("job-a", "stub-db", vec![]);
    let part = src.make_part();
    let commit = SpoolCommit {
        spool_path: PathBuf::from("x.spool"),
        plaintext_blake3: [9; 32],
        plaintext_bytes: 1,
        ciphertext_bytes: 2,
        stderr_tail: String::new(),
    };
    let bare = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
    let bare_json = serde_json::to_vec(&bare).unwrap();
    assert!(verify_bytes(&bare_json, &verifier).is_err(), "미서명은 거부해야 함");

    // 변조: 서명 후 본체 필드를 바꾸면 서명 검증 실패.
    let mut signed: SignedManifest = serde_json::from_slice(&bytes).unwrap();
    signed.manifest.plaintext_bytes += 1;
    let tampered = serde_json::to_vec(&signed).unwrap();
    assert!(verify_bytes(&tampered, &verifier).is_err(), "변조된 매니페스트는 거부해야 함");
}

#[test]
fn rebuild_reconstructs_rows_from_manifests_after_catalog_wipe() {
    let (sk, vk) = generate_signing_keypair();
    let verifier = PinnedKeyVerifier::new(vk);
    let dest = tempfile::tempdir().unwrap(); // 목적지(매니페스트 동봉 위치)
    let catalog_home = tempfile::tempdir().unwrap();

    // 정상 백업 N건의 매니페스트를 목적지에 쓴다.
    const N: usize = 5;
    let mut expected_hashes = Vec::new();
    for i in 0..N {
        let (m, _key) = write_manifest(&sk, dest.path(), i as u8 + 1);
        // Manifest.plaintext_blake3는 이미 hex 문자열(HEXLOWER).
        expected_hashes.push(m.plaintext_blake3.clone());
    }
    // 변조 1건 + 미서명 1건을 섞는다(카탈로그에 반영되면 안 됨).
    {
        let (_m, key) = write_manifest(&sk, dest.path(), 200);
        let mut signed: SignedManifest =
            serde_json::from_slice(&std::fs::read(dest.path().join(&key)).unwrap()).unwrap();
        signed.manifest.plaintext_bytes += 7;
        std::fs::write(dest.path().join(&key), serde_json::to_vec(&signed).unwrap()).unwrap();
    }
    {
        let src = StubSource::ok("job-a", "stub-db", vec![]);
        let part = src.make_part();
        let commit = SpoolCommit {
            spool_path: PathBuf::from("bare.spool"),
            plaintext_blake3: [7; 32],
            plaintext_bytes: 1,
            ciphertext_bytes: 2,
            stderr_tail: String::new(),
        };
        let bare = Manifest::from_commit(&part, Engine::Postgresql, &commit, 1);
        std::fs::write(
            dest.path().join("bare.spool.manifest.json"),
            serde_json::to_vec(&bare).unwrap(),
        )
        .unwrap();
    }

    // 카탈로그 완전 소실 시뮬레이션: db 파일이 아예 없는 상태에서 rebuild.
    let db_path = catalog_home.path().join("catalog.db");
    let source = LocalManifestSource::new(dest.path());
    let writer = CatalogWriterHandle::spawn(db_path.clone()).unwrap();
    let report = catalog_rebuild(
        &source,
        &verifier,
        &writer,
        RebuildArgs { dest_name: "local".into(), dest_kind: "local".into(), ..Default::default() },
    )
    .unwrap();

    assert_eq!(report.accepted, N, "유효 서명 N건만 반영");
    assert_eq!(report.rejected_unsigned_or_tampered.len(), 2, "변조 1 + 미서명 1 배제");

    // 재구성된 backups/artifacts row 검증.
    let conn = open_reader_conn(&db_path).unwrap();
    let backups: i64 = conn
        .query_row("SELECT count(*) FROM backups", [], |r| r.get(0))
        .unwrap();
    assert_eq!(backups as usize, N);
    let artifacts: i64 = conn
        .query_row("SELECT count(*) FROM artifacts WHERE status='committed'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(artifacts as usize, N);

    // 필드 값(plaintext_blake3)이 원래 매니페스트와 일치하는지 확인.
    for hex in &expected_hashes {
        let cnt: i64 = conn
            .query_row(
                "SELECT count(*) FROM backups WHERE plaintext_blake3=?1",
                [hex],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cnt, 1, "각 백업 해시가 정확히 1건 재구성돼야 함");
    }
}
