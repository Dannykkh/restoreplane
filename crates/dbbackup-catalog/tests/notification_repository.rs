use dbbackup_catalog::{
    DeliveryClaimRequest, NotificationChannelSpec, NotificationRuleSpec, OperationalStore,
    OutboxSpec,
};
use tempfile::TempDir;

#[test]
fn outbox_deduplicates_same_payload_and_rejects_conflicting_payload() {
    let fixture = Fixture::new();
    let repository = fixture.store.notification();
    fixture.seed_route(&repository);

    let first = repository
        .enqueue(OutboxSpec::new(
            "outbox-a",
            "rule-a",
            "run-a:failed",
            r#"{"event":"BACKUP_FAILED","runId":"run-a"}"#,
            1_000,
            3,
        ))
        .expect("enqueue outbox");
    let replay = repository
        .enqueue(OutboxSpec::new(
            "outbox-b",
            "rule-a",
            "run-a:failed",
            r#"{"event":"BACKUP_FAILED","runId":"run-a"}"#,
            1_100,
            3,
        ))
        .expect("deduplicate same payload");
    assert!(first.created());
    assert!(!replay.created());
    assert_eq!(first.id(), replay.id());
    assert_eq!(first.id(), "outbox-a");

    let conflict = repository
        .enqueue(OutboxSpec::new(
            "outbox-c",
            "rule-a",
            "run-a:failed",
            r#"{"event":"BACKUP_FAILED","runId":"another"}"#,
            1_200,
            3,
        ))
        .expect_err("same dedupe key with other payload must fail");
    assert_eq!(conflict.code(), "NOTIFICATION_DEDUPLICATION_CONFLICT");
    assert_eq!(repository.outbox_count().expect("outbox count"), 1);
}

#[test]
fn delivery_retry_increments_fence_and_rejects_stale_completion() {
    let fixture = Fixture::new();
    let repository = fixture.store.notification();
    fixture.seed_route(&repository);
    repository
        .enqueue(OutboxSpec::new(
            "outbox-a",
            "rule-a",
            "run-a:failed",
            r#"{"event":"BACKUP_FAILED"}"#,
            1_000,
            3,
        ))
        .expect("enqueue");

    let first = repository
        .claim_next(DeliveryClaimRequest::new("worker-a", "boot-a", 1_000, 100))
        .expect("claim first")
        .expect("due delivery");
    assert_eq!(first.attempt_no(), 1);
    assert_eq!(first.fencing_token(), 1);
    repository
        .fail_for_retry(&first, "HTTP_503", 1_050, 1_100)
        .expect("schedule retry");

    let second = repository
        .claim_next(DeliveryClaimRequest::new("worker-b", "boot-b", 1_100, 100))
        .expect("claim second")
        .expect("due retry");
    assert_eq!(second.attempt_no(), 2);
    assert_eq!(second.fencing_token(), 2);
    let stale = repository
        .mark_delivered(&first, "200", "stale", 1_110)
        .expect_err("old delivery fence must not commit");
    assert_eq!(stale.code(), "NOTIFICATION_FENCE_STALE");
    repository
        .mark_delivered(&second, "200", "accepted", 1_120)
        .expect("commit current delivery");

    assert_eq!(
        repository.outbox_state("outbox-a").expect("outbox state"),
        "delivered"
    );
    assert_eq!(
        repository
            .delivery_attempt_count("outbox-a")
            .expect("attempt count"),
        2
    );
}

#[test]
fn claim_carries_canonical_delivery_payload_and_channel_without_secret_logging_fields() {
    let fixture = Fixture::new();
    let repository = fixture.store.notification();
    fixture.seed_route(&repository);
    repository
        .enqueue(OutboxSpec::new(
            "outbox-a",
            "rule-a",
            "run-a:failed",
            r#"{ "runId": "run-a", "event": "BACKUP_FAILED" }"#,
            1_000,
            3,
        ))
        .expect("enqueue");

    let claim = repository
        .claim_next(DeliveryClaimRequest::new("worker-a", "boot-a", 1_000, 100))
        .expect("claim")
        .expect("due delivery");
    assert_eq!(claim.outbox_id(), "outbox-a");
    assert_eq!(claim.channel_kind(), "webhook");
    assert_eq!(
        claim.channel_config_json(),
        r#"{"url":"https://example.invalid/hook"}"#
    );
    assert_eq!(claim.deduplication_key(), "run-a:failed");
    assert_eq!(
        claim.payload_json(),
        r#"{"event":"BACKUP_FAILED","runId":"run-a"}"#
    );
}

#[test]
fn terminal_delivery_failure_is_not_requeued() {
    let fixture = Fixture::new();
    let repository = fixture.store.notification();
    fixture.seed_route(&repository);
    repository
        .enqueue(OutboxSpec::new(
            "outbox-a",
            "rule-a",
            "run-a:failed",
            r#"{"event":"BACKUP_FAILED"}"#,
            1_000,
            3,
        ))
        .expect("enqueue");
    let claim = repository
        .claim_next(DeliveryClaimRequest::new("worker-a", "boot-a", 1_000, 100))
        .expect("claim")
        .expect("due delivery");

    repository
        .mark_failed_terminal(&claim, "HTTP_400", "invalid endpoint request", 1_050)
        .expect("terminal failure");
    assert_eq!(
        repository.outbox_state("outbox-a").expect("outbox state"),
        "failed_terminal"
    );
    assert!(repository
        .claim_next(DeliveryClaimRequest::new("worker-b", "boot-b", 2_000, 100))
        .expect("claim query")
        .is_none());
}

#[test]
fn expired_delivery_lease_is_requeued_with_same_deduplication_key_and_new_fence() {
    let fixture = Fixture::new();
    let repository = fixture.store.notification();
    fixture.seed_route(&repository);
    repository
        .enqueue(OutboxSpec::new(
            "outbox-a",
            "rule-a",
            "run-a:failed",
            r#"{"event":"BACKUP_FAILED"}"#,
            1_000,
            3,
        ))
        .expect("enqueue");
    let first = repository
        .claim_next(DeliveryClaimRequest::new("worker-a", "boot-a", 1_000, 100))
        .expect("claim")
        .expect("due delivery");

    assert_eq!(
        repository
            .recover_expired_leases(1_100)
            .expect("recover expired lease"),
        1
    );
    let second = repository
        .claim_next(DeliveryClaimRequest::new("worker-b", "boot-b", 1_100, 100))
        .expect("reclaim")
        .expect("requeued delivery");
    assert_eq!(second.attempt_no(), 2);
    assert_eq!(second.fencing_token(), 2);
    assert_eq!(second.deduplication_key(), first.deduplication_key());
    let stale = repository
        .mark_delivered(&first, "200", "late completion", 1_110)
        .expect_err("expired fence must not commit");
    assert_eq!(stale.code(), "NOTIFICATION_FENCE_STALE");
}

struct Fixture {
    _temp_dir: TempDir,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let store = OperationalStore::open(temp_dir.path().join("operational.db"))
            .expect("open operational store");
        Self {
            _temp_dir: temp_dir,
            store,
        }
    }

    fn seed_route(&self, repository: &dbbackup_catalog::NotificationRepository) {
        repository
            .insert_channel(NotificationChannelSpec::webhook(
                "channel-a",
                "operations",
                r#"{"url":"https://example.invalid/hook"}"#,
                100,
            ))
            .expect("insert channel");
        repository
            .mark_channel_ready("channel-a", 105)
            .expect("record successful channel probe");
        repository
            .insert_rule(NotificationRuleSpec::new(
                "rule-a",
                "backup failures",
                "channel-a",
                r#"{"eventCodes":["BACKUP_FAILED"]}"#,
                "error",
                0,
                110,
            ))
            .expect("insert rule");
    }
}
