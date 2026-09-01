use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dbbackup_catalog::{
    rusqlite::Connection, DeliveryClaimRequest, NotificationChannelSpec, NotificationRuleSpec,
    OperationalStore, OutboxSpec,
};
use dbbackup_runtime::{
    NotificationDelivery, NotificationDeliveryError, NotificationDeliveryReceipt,
    NotificationDeliveryRequest, NotificationDeliveryWorker, NotificationWorkerOutcome,
    RetentionDeleteCancellationToken, RuntimeClock, WebhookNotificationDelivery,
};
use tempfile::TempDir;

#[test]
fn delivery_diagnostics_reject_common_secret_shapes() {
    for summary in [
        "Authorization: Bearer private",
        "password=private",
        "token=private",
        "api_key=private",
    ] {
        assert!(
            NotificationDeliveryError::terminal("HTTP_401", summary).is_err(),
            "summary must be redacted before persistence"
        );
        assert!(
            NotificationDeliveryReceipt::new("401", summary).is_err(),
            "receipt must be redacted before persistence"
        );
    }
}

#[tokio::test]
async fn successful_delivery_commits_outbox_and_passes_stable_deduplication_key() {
    let fixture = Fixture::new();
    fixture.enqueue("outbox-a", 3);
    let sender = Arc::new(FakeSender::success());
    let worker = fixture.worker(sender.clone(), [1_000, 1_050]);

    let outcome = worker.run_once().await.expect("delivery worker");
    assert_eq!(
        outcome,
        NotificationWorkerOutcome::Delivered {
            outbox_id: "outbox-a".to_owned()
        }
    );
    assert_eq!(
        fixture
            .store
            .notification()
            .outbox_state("outbox-a")
            .expect("outbox state"),
        "delivered"
    );
    let requests = sender.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].deduplication_key, "run-a:failed");
    assert_eq!(requests[0].channel_kind, "webhook");
    assert_eq!(
        requests[0].payload_json,
        r#"{"event":"BACKUP_FAILED","runId":"run-a"}"#
    );
}

#[tokio::test]
async fn retryable_notification_failure_is_an_outbox_result_not_worker_failure() {
    let fixture = Fixture::new();
    fixture.enqueue("outbox-a", 3);
    let sender = Arc::new(FakeSender::retryable());
    let worker = fixture.worker(sender, [1_000, 1_050]);

    let outcome = worker
        .run_once()
        .await
        .expect("delivery failure must settle durably");
    assert_eq!(
        outcome,
        NotificationWorkerOutcome::RetryScheduled {
            outbox_id: "outbox-a".to_owned(),
            available_at_ms: 1_150,
        }
    );
    assert_eq!(
        fixture
            .store
            .notification()
            .outbox_state("outbox-a")
            .expect("outbox state"),
        "pending"
    );
    let connection = Connection::open(&fixture.db_path).expect("notification attention reader");
    let attention: (String, String, String, i64) = connection
        .query_row(
            "SELECT category, state, entity_id,
                    (SELECT COUNT(*) FROM activity_events
                     WHERE event_code = 'NOTIFICATION_DELIVERY_RETRY'
                       AND entity_id = 'outbox-a')
             FROM attention_states
             WHERE fingerprint = 'notification-delivery:outbox-a'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("notification failure attention");
    assert_eq!(
        attention,
        (
            "notification_failure".to_owned(),
            "open".to_owned(),
            "outbox-a".to_owned(),
            1
        )
    );
    drop(connection);

    let success = fixture.worker(Arc::new(FakeSender::success()), [1_150, 1_200]);
    assert!(matches!(
        success.run_once().await.expect("retry delivery"),
        NotificationWorkerOutcome::Delivered { .. }
    ));
    let connection = Connection::open(&fixture.db_path).expect("resolved attention reader");
    let resolved: String = connection
        .query_row(
            "SELECT state FROM attention_states
             WHERE fingerprint = 'notification-delivery:outbox-a'",
            [],
            |row| row.get(0),
        )
        .expect("resolved notification attention");
    assert_eq!(resolved, "resolved");
}

#[tokio::test]
async fn service_cancellation_interrupts_delivery_and_requeues_with_stable_error() {
    let fixture = Fixture::new();
    fixture.enqueue("outbox-cancel", 3);
    let worker = fixture.worker(Arc::new(HangingSender), [1_000, 1_050]);
    let cancellation = RetentionDeleteCancellationToken::default();
    let trigger = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(25)).await;
        trigger.cancel();
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(1),
        worker.run_once_with_cancellation(&cancellation),
    )
    .await
    .expect("service cancellation must bound notification delivery")
    .expect("settle cancelled notification");

    assert_eq!(
        outcome,
        NotificationWorkerOutcome::RetryScheduled {
            outbox_id: "outbox-cancel".to_owned(),
            available_at_ms: 1_150,
        }
    );
    let error_code: String = Connection::open(&fixture.db_path)
        .expect("notification attempt reader")
        .query_row(
            "SELECT response_code
             FROM notification_delivery_attempts
             WHERE outbox_id = 'outbox-cancel'",
            [],
            |row| row.get(0),
        )
        .expect("cancelled attempt error");
    assert_eq!(error_code, "NOTIFICATION_DELIVERY_SERVICE_CANCELLED");
}

#[tokio::test]
async fn crash_after_external_delivery_reuses_deduplication_key_with_new_fence() {
    let fixture = Fixture::new();
    fixture.enqueue("outbox-a", 3);
    let first = fixture
        .store
        .notification()
        .claim_next(DeliveryClaimRequest::new(
            "crashed-worker",
            "crashed-boot",
            1_000,
            100,
        ))
        .expect("first claim")
        .expect("due delivery");
    assert_eq!(first.fencing_token(), 1);

    let sender = Arc::new(FakeSender::success());
    let worker = fixture.worker(sender.clone(), [1_100, 1_150]);
    let outcome = worker.run_once().await.expect("recover and redeliver");
    assert!(matches!(
        outcome,
        NotificationWorkerOutcome::Delivered { .. }
    ));
    let requests = sender.requests.lock().expect("requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].deduplication_key, first.deduplication_key());
    assert_eq!(
        fixture
            .store
            .notification()
            .delivery_attempt_count("outbox-a")
            .expect("attempt count"),
        2
    );
}

#[tokio::test]
async fn real_webhook_delivery_sends_canonical_payload_and_idempotency_key() {
    let endpoint = HttpFixture::responding("202 Accepted");
    let fixture = Fixture::with_webhook_url(&endpoint.url("/events"));
    fixture.enqueue("outbox-a", 3);
    let delivery = Arc::new(
        WebhookNotificationDelivery::with_timeouts(Duration::from_secs(1), Duration::from_secs(2))
            .expect("webhook transport"),
    );
    let worker = fixture.worker(delivery, [1_000, 1_050]);

    assert_eq!(
        worker.run_once().await.expect("real webhook delivery"),
        NotificationWorkerOutcome::Delivered {
            outbox_id: "outbox-a".to_owned()
        }
    );

    let request = endpoint.request();
    assert!(request.starts_with("POST /events HTTP/1.1\r\n"));
    assert!(request
        .to_ascii_lowercase()
        .contains("\r\nidempotency-key: run-a:failed\r\n"));
    assert!(request
        .to_ascii_lowercase()
        .contains("\r\ncontent-type: application/json\r\n"));
    assert!(request.ends_with(r#"{"event":"BACKUP_FAILED","runId":"run-a"}"#));
}

#[tokio::test]
async fn real_webhook_failure_retries_without_mutating_completed_operation() {
    let endpoint = HttpFixture::responding("503 Service Unavailable");
    let fixture = Fixture::with_webhook_url(&endpoint.url("/events"));
    fixture.enqueue("outbox-a", 3);
    let connection = Connection::open(&fixture.db_path).expect("operation writer");
    connection
        .execute(
            "INSERT INTO operation_runs(
                 id, operation_kind, subject_type, subject_id, request_json,
                 state, result_json, started_at_ms, completed_at_ms,
                 created_at_ms, updated_at_ms
             ) VALUES (
                 'operation-a', 'notification_test', 'fixture', 'fixture-a', '{}',
                 'succeeded', '{\"delivered\":false}', 900, 950, 900, 950
             )",
            [],
        )
        .expect("completed operation fixture");
    drop(connection);
    let delivery = Arc::new(
        WebhookNotificationDelivery::with_timeouts(Duration::from_secs(1), Duration::from_secs(2))
            .expect("webhook transport"),
    );
    let worker = fixture.worker(delivery, [1_000, 1_050]);

    assert_eq!(
        worker.run_once().await.expect("durable webhook failure"),
        NotificationWorkerOutcome::RetryScheduled {
            outbox_id: "outbox-a".to_owned(),
            available_at_ms: 1_150,
        }
    );
    let connection = Connection::open(&fixture.db_path).expect("operation reader");
    let state: String = connection
        .query_row(
            "SELECT state FROM operation_runs WHERE id = 'operation-a'",
            [],
            |row| row.get(0),
        )
        .expect("completed operation");
    assert_eq!(state, "succeeded");
    assert_eq!(
        fixture
            .store
            .notification()
            .outbox_state("outbox-a")
            .expect("outbox state"),
        "pending"
    );
    let request = endpoint.request();
    assert!(request.starts_with("POST /events HTTP/1.1\r\n"));
}

#[tokio::test]
async fn real_webhook_total_timeout_is_finite_and_retryable() {
    let endpoint = HttpFixture::delayed("202 Accepted", Duration::from_secs(1));
    let fixture = Fixture::with_webhook_url(&endpoint.url("/events"));
    fixture.enqueue("outbox-timeout", 3);
    let delivery = Arc::new(
        WebhookNotificationDelivery::with_timeouts(
            Duration::from_millis(50),
            Duration::from_millis(100),
        )
        .expect("webhook transport"),
    );
    let worker = fixture.worker(delivery, [1_000, 1_050]);
    let started = std::time::Instant::now();

    assert_eq!(
        worker.run_once().await.expect("bounded webhook timeout"),
        NotificationWorkerOutcome::RetryScheduled {
            outbox_id: "outbox-timeout".to_owned(),
            available_at_ms: 1_150,
        }
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    let error_code: String = Connection::open(&fixture.db_path)
        .expect("notification attempt reader")
        .query_row(
            "SELECT response_code
             FROM notification_delivery_attempts
             WHERE outbox_id = 'outbox-timeout'",
            [],
            |row| row.get(0),
        )
        .expect("timeout attempt error");
    assert_eq!(error_code, "HTTP_TIMEOUT");
    let request = endpoint.request();
    assert!(request.starts_with("POST /events HTTP/1.1\r\n"));
}

struct RequestSnapshot {
    deduplication_key: String,
    channel_kind: String,
    payload_json: String,
}

enum SenderMode {
    Success,
    Retryable,
}

struct FakeSender {
    mode: SenderMode,
    requests: Mutex<Vec<RequestSnapshot>>,
}

struct HangingSender;

#[async_trait]
impl NotificationDelivery for HangingSender {
    async fn deliver(
        &self,
        _request: &NotificationDeliveryRequest,
    ) -> Result<NotificationDeliveryReceipt, NotificationDeliveryError> {
        std::future::pending().await
    }
}

impl FakeSender {
    fn success() -> Self {
        Self {
            mode: SenderMode::Success,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn retryable() -> Self {
        Self {
            mode: SenderMode::Retryable,
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl NotificationDelivery for FakeSender {
    async fn deliver(
        &self,
        request: &NotificationDeliveryRequest,
    ) -> Result<NotificationDeliveryReceipt, NotificationDeliveryError> {
        self.requests
            .lock()
            .expect("requests")
            .push(RequestSnapshot {
                deduplication_key: request.deduplication_key().to_owned(),
                channel_kind: request.channel_kind().to_owned(),
                payload_json: request.payload_json().to_owned(),
            });
        match self.mode {
            SenderMode::Success => {
                NotificationDeliveryReceipt::new("202", "accepted").map_err(|error| {
                    NotificationDeliveryError::terminal(error.code(), "invalid receipt")
                        .expect("terminal error")
                })
            }
            SenderMode::Retryable => Err(NotificationDeliveryError::retryable(
                "HTTP_503",
                "endpoint unavailable",
            )
            .expect("retryable error")),
        }
    }
}

struct SequenceClock {
    values: Mutex<VecDeque<i64>>,
}

impl SequenceClock {
    fn new(values: impl IntoIterator<Item = i64>) -> Self {
        Self {
            values: Mutex::new(values.into_iter().collect()),
        }
    }
}

impl RuntimeClock for SequenceClock {
    fn now_ms(&self) -> i64 {
        self.values
            .lock()
            .expect("clock")
            .pop_front()
            .expect("clock value")
    }
}

struct HttpFixture {
    address: std::net::SocketAddr,
    request_rx: std::sync::mpsc::Receiver<String>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HttpFixture {
    fn responding(status: &'static str) -> Self {
        Self::delayed(status, Duration::ZERO)
    }

    fn delayed(status: &'static str, response_delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind webhook fixture");
        let address = listener.local_addr().expect("webhook fixture address");
        let (request_tx, request_rx) = std::sync::mpsc::sync_channel(1);
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept webhook request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("webhook fixture timeout");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1_024];
            let header_end = loop {
                let read = stream.read(&mut buffer).expect("read webhook request");
                assert!(read > 0, "webhook request ended before headers");
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(position) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    break position + 4;
                }
            };
            let headers =
                String::from_utf8(bytes[..header_end].to_vec()).expect("webhook request headers");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().expect("content length"))
                })
                .expect("content-length header");
            while bytes.len() < header_end + content_length {
                let read = stream.read(&mut buffer).expect("read webhook body");
                assert!(read > 0, "webhook request ended before body");
                bytes.extend_from_slice(&buffer[..read]);
            }
            request_tx
                .send(String::from_utf8(bytes).expect("UTF-8 webhook request"))
                .expect("capture webhook request");
            std::thread::sleep(response_delay);
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .expect("write webhook response");
        });
        Self {
            address,
            request_rx,
            thread: Some(thread),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    fn request(mut self) -> String {
        let request = self
            .request_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("captured webhook request");
        self.thread
            .take()
            .expect("webhook fixture thread")
            .join()
            .expect("webhook fixture");
        request
    }
}

struct Fixture {
    _temp_dir: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        Self::with_webhook_url("https://example.invalid/hook")
    }

    fn with_webhook_url(url: &str) -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let db_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("operational store");
        let repository = store.notification();
        repository
            .insert_channel(NotificationChannelSpec::webhook(
                "channel-a",
                "operations",
                serde_json::json!({ "url": url }).to_string(),
                100,
            ))
            .expect("channel");
        repository
            .mark_channel_ready("channel-a", 105)
            .expect("channel ready");
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
            .expect("rule");
        Self {
            _temp_dir: temp_dir,
            db_path,
            store,
        }
    }

    fn enqueue(&self, outbox_id: &str, attempt_limit: i64) {
        self.store
            .notification()
            .enqueue(OutboxSpec::new(
                outbox_id,
                "rule-a",
                "run-a:failed",
                r#"{"event":"BACKUP_FAILED","runId":"run-a"}"#,
                1_000,
                attempt_limit,
            ))
            .expect("enqueue");
    }

    fn worker(
        &self,
        sender: Arc<dyn NotificationDelivery>,
        clock_values: impl IntoIterator<Item = i64>,
    ) -> NotificationDeliveryWorker {
        NotificationDeliveryWorker::new(
            self.store.notification(),
            sender,
            Arc::new(SequenceClock::new(clock_values)),
            "notification-worker",
            "boot-a",
            100,
            100,
        )
        .expect("worker")
    }
}
