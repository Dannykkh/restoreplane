use std::fs::File;
use std::io::Read;

use dbbackup_service::support_bundle::{
    SupportBundleBuilder, SupportBundleError, SupportBundleInput,
};
use serde_json::json;
use tempfile::TempDir;

const CANARY: &str = "support-canary-do-not-export-7af1";

#[test]
fn bundle_contains_only_allowlisted_redacted_documents_and_a_verified_manifest() {
    let temp = TempDir::new().expect("temp directory");
    let input = SupportBundleInput::new(
        "build-123",
        "revision-abc",
        json!({
            "queue": {"queued": 2},
            "databasePassword": "must-not-survive",
            "nested": {"apiToken": "also-secret"}
        }),
        json!({"rows": [{"engine": "sqlite", "supportStatus": "supported"}]}),
        json!([{"eventCode": "BACKUP_SUCCEEDED", "severity": "info"}]),
        json!({"executionAuthority": "service", "rawStderr": "never-export"}),
    )
    .expect("bounded input");
    let report = SupportBundleBuilder::new(temp.path().to_path_buf())
        .expect("builder")
        .with_canary_for_verification(CANARY)
        .expect("canary")
        .create(&input, 1_700_000_000_000)
        .expect("support bundle");

    assert!(report.path().exists());
    assert_eq!(report.entry_count(), 5);
    assert_eq!(report.redaction_version(), 1);
    assert_eq!(report.checksum_blake3().len(), 64);

    let documents = unpack_documents(report.path());
    assert_eq!(
        documents.keys().cloned().collect::<Vec<_>>(),
        [
            "activity.json",
            "config-metadata.json",
            "diagnostics.json",
            "engine-matrix.json",
            "manifest.json",
        ]
    );
    let joined = documents.values().cloned().collect::<Vec<_>>().join("\n");
    assert!(!joined.contains("must-not-survive"));
    assert!(!joined.contains("also-secret"));
    assert!(!joined.contains("never-export"));
    assert!(!joined.contains(CANARY));
    assert!(joined.contains("[REDACTED]"));
    assert!(joined.contains("\"excluded\""));
    assert!(joined.contains("\"checksumsBlake3\""));
}

#[test]
fn canary_detection_refuses_success_and_leaves_no_partial_archive() {
    let temp = TempDir::new().expect("temp directory");
    let input = SupportBundleInput::new(
        "build-123",
        "revision-abc",
        json!({"innocentLookingField": CANARY}),
        json!({}),
        json!([]),
        json!({}),
    )
    .expect("input");
    let error = SupportBundleBuilder::new(temp.path().to_path_buf())
        .expect("builder")
        .with_canary_for_verification(CANARY)
        .expect("canary")
        .create(&input, 1_700_000_000_000)
        .expect_err("canary must block export");

    assert_eq!(error, SupportBundleError::CanaryDetected);
    assert_eq!(
        std::fs::read_dir(temp.path())
            .expect("read bundle directory")
            .count(),
        0
    );
    assert!(!error.to_string().contains(CANARY));
}

#[test]
fn bundle_input_and_archive_are_bounded() {
    let temp = TempDir::new().expect("temp directory");
    let oversized = "x".repeat(4 * 1024 * 1024 + 1);
    let error = SupportBundleInput::new(
        "build-123",
        "revision-abc",
        json!({"payload": oversized}),
        json!({}),
        json!([]),
        json!({}),
    )
    .expect_err("oversized document");
    assert_eq!(error, SupportBundleError::InputTooLarge);
    assert_eq!(
        std::fs::read_dir(temp.path())
            .expect("read empty directory")
            .count(),
        0
    );
}

fn unpack_documents(path: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    let file = File::open(path).expect("open support bundle");
    let decoder = zstd::Decoder::new(file).expect("zstd decoder");
    let mut archive = tar::Archive::new(decoder);
    let mut documents = std::collections::BTreeMap::new();
    for entry in archive.entries().expect("archive entries") {
        let mut entry = entry.expect("archive entry");
        let path = entry
            .path()
            .expect("entry path")
            .to_string_lossy()
            .to_string();
        let mut contents = String::new();
        entry
            .read_to_string(&mut contents)
            .expect("UTF-8 JSON entry");
        documents.insert(path, contents);
    }
    documents
}
