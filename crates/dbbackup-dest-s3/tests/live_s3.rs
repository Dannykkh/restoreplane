// crates/dbbackup-dest-s3/tests/live_s3.rs
//! S06 라이브/인프라 테스트 — 실제 S3/MinIO(Object Lock 버킷 + append-only/과권한 두 액세스키)가
//! 있어야 결정적으로 재현된다. 무인 CI에서는 인프라가 없으므로 #[ignore]로 게이트한다.
//! 실행: `cargo test -p dbbackup-dest-s3 -- --ignored`  (env: S3_ENDPOINT/S3_REGION/S3_BUCKET/
//! AWS_ACCESS_KEY_ID/AWS_SECRET_ACCESS_KEY 필요).

use dbbackup_dest_s3::{S3Cfg, S3Dest, S3Provider};
use dbbackup_pipeline::{upload_resumable, Destination, ImmutabilityKind};

fn cfg_from_env() -> S3Cfg {
    S3Cfg {
        endpoint: std::env::var("S3_ENDPOINT").unwrap_or_default(),
        region: std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        bucket: std::env::var("S3_BUCKET").expect("S3_BUCKET"),
        prefix: "dbbackup-livetest".into(),
        access_key_id: std::env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID"),
        secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY"),
        object_lock_mode: ImmutabilityKind::ObjectLockGovernance,
        retention_days: 30,
        min_dwell_days: 7,
        checksum_algo: Some("crc32c".into()),
        compliance_opt_in: false,
        provider: S3Provider::MinIO,
    }
}

#[tokio::test]
#[ignore = "needs AWS/MinIO creds + Object Lock 버킷 (S3_* / AWS_* env)"]
async fn s3_putobject_multipart_commit() {
    let dest = S3Dest::new("s3-live", cfg_from_env()).unwrap();
    let tmp = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(tmp.path(), vec![7u8; 20 * 1024 * 1024]).unwrap();
    let receipt = upload_resumable(&dest as &dyn Destination, tmp.path(), "live/putobject.bin")
        .await
        .expect("멀티파트 업로드 + commit 성공해야 함");
    assert!(receipt.bytes > 0);
}

#[tokio::test]
#[ignore = "needs append-only 자격증명으로 잠긴 버킷 — deny-check가 실제 거부되는지 검증(H2)"]
async fn s3_self_test_denies_destructive_ops() {
    // 올바른 append-only 자격 + 잠긴 버킷이면 Ok(모든 파괴동작 DENIED, versioning ON, 기본 보존).
    // 과권한 자격이면 OverPrivileged로, 미잠금이면 BucketNotLocked로 실행 거부돼야 한다.
    let dest = S3Dest::new("s3-live", cfg_from_env()).unwrap();
    let report = dest.self_test().await.expect("append-only+잠긴 버킷이면 자가테스트 통과");
    assert!(report.denied_delete);
    assert!(report.denied_put_retention);
    assert!(report.denied_put_lifecycle);
    assert!(report.versioning_on);
    assert!(report.default_retention_days.is_some());
    assert!(report.over_privileged.is_empty());
}
