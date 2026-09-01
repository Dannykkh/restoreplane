use dbbackup_catalog::ManifestSource;
use dbbackup_dest_s3::S3ManifestSource;
use dbbackup_pipeline::{Destination, DestinationReaderV2, LocalDest};
use opendal::{services::Memory, Operator};

#[tokio::test]
async fn lists_relative_signed_manifest_keys_and_reads_objects() {
    let op = Operator::new(Memory::default()).unwrap();
    op.write("archive/job-a/a.manifest.json", "manifest-a")
        .await
        .unwrap();
    op.write("archive/job-b/b.manifest.json", "manifest-b")
        .await
        .unwrap();
    op.write("archive/job-c/set-manifest.json", "manifest-c")
        .await
        .unwrap();
    op.write("archive/job-a/a.spool", "ciphertext")
        .await
        .unwrap();
    op.write("outside/c.manifest.json", "outside")
        .await
        .unwrap();

    let source = S3ManifestSource::from_operator(op, "archive");

    assert_eq!(
        source.list_manifests(None).await.unwrap(),
        [
            "job-a/a.manifest.json",
            "job-b/b.manifest.json",
            "job-c/set-manifest.json"
        ]
    );
    assert_eq!(
        source.list_manifests(Some("job-a")).await.unwrap(),
        ["job-a/a.manifest.json"]
    );
    assert_eq!(
        source.read_bytes("job-a/a.spool").await.unwrap(),
        b"ciphertext"
    );

    let probe = source
        .probe_object("job-a/a.spool")
        .await
        .expect("streaming probe");
    assert_eq!(probe.bytes, 10);
    assert_eq!(
        probe.blake3,
        blake3::hash(b"ciphertext").to_hex().to_string()
    );
    assert!(
        source.read_bytes_bounded("job-a/a.spool", 9).await.is_err(),
        "bounded reader must reject oversized objects before allocation"
    );

    let target = tempfile::tempdir().expect("target");
    let target_dest = LocalDest::new("target", target.path());
    let mut writer = target_dest
        .writer("copied.spool")
        .await
        .expect("target writer");
    let copied = source
        .copy_object_to("job-a/a.spool", &mut writer, 10)
        .await
        .expect("streaming copy");
    writer.commit().await.expect("commit copied object");
    assert_eq!(copied, probe);
    assert_eq!(
        std::fs::read(target.path().join("copied.spool")).expect("copied bytes"),
        b"ciphertext"
    );
}
