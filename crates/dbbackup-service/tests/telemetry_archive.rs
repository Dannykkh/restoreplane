use std::io::Read;

use dbbackup_catalog::{
    ActivityEventSpec, AttentionObservation, DeliveryClaimRequest, NotificationChannelSpec,
    NotificationRuleSpec, OperationalStore, OutboxSpec,
};
use dbbackup_service::telemetry_archive::TelemetryArchiveService;
use tempfile::TempDir;

#[test]
fn old_activity_and_terminal_outbox_are_archived_without_secrets_or_open_ledgers() {
    let fixture = Fixture::new();
    let activity = fixture.store.activity();
    let sequences = activity
        .append_batch(vec![
            ActivityEventSpec::info(
                "old-free",
                1_000,
                "BACKUP_PROGRESS",
                "backup_run",
                "run-free",
                "old progress",
            ),
            ActivityEventSpec::error(
                "old-open",
                1_100,
                "BACKUP_FAILED",
                "backup_run",
                "run-open",
                "open attention",
            ),
            ActivityEventSpec::error(
                "old-resolved",
                1_200,
                "BACKUP_FAILED",
                "backup_run",
                "run-resolved",
                "resolved attention",
            ),
            ActivityEventSpec::info(
                "recent",
                15_000,
                "BACKUP_SUCCEEDED",
                "backup_run",
                "run-recent",
                "recent event",
            ),
        ])
        .expect("seed activity");
    let open_attention = activity
        .observe_attention(AttentionObservation::run_failure(
            "run-open:BACKUP_FAILED",
            "backup_run",
            "run-open",
            "open failure",
            sequences[1],
            1_100,
        ))
        .expect("open attention");
    let resolved_attention = activity
        .observe_attention(AttentionObservation::run_failure(
            "run-resolved:BACKUP_FAILED",
            "backup_run",
            "run-resolved",
            "resolved failure",
            sequences[2],
            1_200,
        ))
        .expect("resolved attention");
    activity
        .resolve_attention(resolved_attention.id(), "fixed", 1_300)
        .expect("resolve attention");

    let notification = fixture.store.notification();
    fixture.seed_route();
    notification
        .enqueue(OutboxSpec::new(
            "outbox-delivered",
            "rule-a",
            "old-delivered",
            r#"{"summary":"TOP_SECRET_CANARY_7fd4"}"#,
            1_000,
            3,
        ))
        .expect("enqueue delivered fixture");
    let delivered = notification
        .claim_next(DeliveryClaimRequest::new("worker-a", "boot-a", 1_000, 100))
        .expect("claim delivered fixture")
        .expect("delivery claim");
    notification
        .mark_delivered(&delivered, "200", "accepted", 1_050)
        .expect("settle delivered fixture");
    notification
        .enqueue(OutboxSpec::new(
            "outbox-pending",
            "rule-a",
            "old-pending",
            r#"{"summary":"must remain active"}"#,
            1_100,
            3,
        ))
        .expect("enqueue active fixture");

    let service = TelemetryArchiveService::new(
        fixture.store.clone(),
        fixture.temp_dir.path().join("telemetry-archives"),
    )
    .expect("archive service");
    let report = service
        .archive_before(10_000, 100, 20_000)
        .expect("archive old telemetry")
        .expect("eligible telemetry");
    assert_eq!(report.archived_activity_count(), 2);
    assert_eq!(report.archived_outbox_count(), 1);
    assert!(report.path().is_file());
    assert_eq!(report.checksum_blake3().len(), 64);

    let compressed = std::fs::read(report.path()).expect("read archive");
    assert!(!contains(&compressed, b"TOP_SECRET_CANARY_7fd4"));
    let decoder = zstd::Decoder::new(compressed.as_slice()).expect("zstd decoder");
    let mut archive = tar::Archive::new(decoder);
    let mut json = Vec::new();
    let mut entries = archive.entries().expect("archive entries");
    let mut entry = entries.next().expect("one entry").expect("entry");
    assert_eq!(
        entry.path().expect("entry path").to_string_lossy(),
        "telemetry.json"
    );
    entry.read_to_end(&mut json).expect("read telemetry entry");
    assert!(entries.next().is_none());
    assert!(!contains(&json, b"TOP_SECRET_CANARY_7fd4"));
    let document: serde_json::Value = serde_json::from_slice(&json).expect("archive JSON");
    assert_eq!(document["schemaVersion"], 1);
    assert_eq!(document["activity"].as_array().expect("activity").len(), 2);
    assert_eq!(document["outbox"].as_array().expect("outbox").len(), 1);
    assert_eq!(
        document["outbox"][0]["payloadBlake3"]
            .as_str()
            .expect("payload hash")
            .len(),
        64
    );
    assert!(document["outbox"][0].get("payload").is_none());

    let remaining_activity = activity.recent(10).expect("remaining activity");
    let remaining_ids = remaining_activity
        .iter()
        .map(|event| event.id())
        .collect::<Vec<_>>();
    assert_eq!(remaining_ids, vec!["recent", "old-open"]);
    assert!(notification.outbox_state("outbox-delivered").is_err());
    assert_eq!(
        notification
            .outbox_state("outbox-pending")
            .expect("pending outbox remains"),
        "pending"
    );
    assert_eq!(
        activity
            .attention_state(open_attention.id())
            .expect("open attention remains"),
        "open"
    );
    assert_eq!(
        activity
            .attention_state(resolved_attention.id())
            .expect("resolved attention ledger remains"),
        "resolved"
    );
    assert_eq!(
        service.committed_archive_count().expect("archive records"),
        1
    );
    let diagnostics = fixture
        .store
        .diagnostics()
        .collect(20_001)
        .expect("archive diagnostics");
    assert_eq!(diagnostics.activity().archive_count(), 1);
    assert_eq!(diagnostics.activity().latest_archive_at_ms(), Some(20_000));
    assert_eq!(
        fixture
            .store
            .writer_diagnostics()
            .expect("writer diagnostics")
            .auto_vacuum,
        2
    );
    assert!(service
        .archive_before(10_000, 100, 21_000)
        .expect("idempotent archive")
        .is_none());
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

struct Fixture {
    temp_dir: TempDir,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let store = OperationalStore::open(temp_dir.path().join("operational.db"))
            .expect("operational store");
        Self { temp_dir, store }
    }

    fn seed_route(&self) {
        let notification = self.store.notification();
        notification
            .insert_channel(NotificationChannelSpec::webhook(
                "channel-a",
                "operations",
                r#"{"url":"https://example.invalid/hook"}"#,
                100,
            ))
            .expect("channel");
        notification
            .mark_channel_ready("channel-a", 105)
            .expect("channel ready");
        notification
            .insert_rule(NotificationRuleSpec::new(
                "rule-a",
                "backup failures",
                "channel-a",
                r#"{"eventCodes":["BACKUP_FAILED"]}"#,
                "error",
                0,
                110,
            ))
            .expect("rule");
    }
}
