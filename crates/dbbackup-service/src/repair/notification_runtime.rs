use std::sync::Arc;
use std::time::Duration;

use dbbackup_catalog::{NotificationRepository, OperationalStore};
use dbbackup_runtime::{
    NotificationDelivery, NotificationDeliveryContractError, NotificationDeliveryWorker,
    RetentionDeleteCancellationToken, RuntimeClock, SystemRuntimeClock,
    WebhookNotificationDelivery,
};
use thiserror::Error;

use crate::host::{ClaimRuntime, HostError, ServiceHealthProbe};

use super::queue_runtime::{ClaimsEnabledProbe, QueueRuntimeHandle, QueueRuntimeStartError};

const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(250);
const WEBHOOK_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const WEBHOOK_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_WAIT_TIMEOUT: Duration = Duration::from_secs(10);
const LEASE_DURATION_MS: i64 = 30_000;
const RETRY_DELAY_MS: i64 = 5_000;

#[derive(Debug, Error)]
pub enum NotificationRuntimeStartError {
    #[error("NOTIFICATION_RUNTIME_INPUT_INVALID")]
    InputInvalid,
    #[error("NOTIFICATION_RUNTIME_ASYNC_INIT")]
    AsyncRuntime(#[source] std::io::Error),
    #[error("NOTIFICATION_RUNTIME_THREAD_START")]
    ThreadStart(#[source] std::io::Error),
}

impl NotificationRuntimeStartError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "NOTIFICATION_RUNTIME_INPUT_INVALID",
            Self::AsyncRuntime(_) => "NOTIFICATION_RUNTIME_ASYNC_INIT",
            Self::ThreadStart(_) => "NOTIFICATION_RUNTIME_THREAD_START",
        }
    }
}

pub struct ServiceNotificationDeliveryRuntimeHandle {
    inner: QueueRuntimeHandle,
}

impl ServiceNotificationDeliveryRuntimeHandle {
    pub fn spawn(
        store: OperationalStore,
        health: ServiceHealthProbe,
    ) -> Result<Self, NotificationRuntimeStartError> {
        if WEBHOOK_TOTAL_TIMEOUT >= STOP_WAIT_TIMEOUT {
            return Err(NotificationRuntimeStartError::InputInvalid);
        }
        let delivery = WebhookNotificationDelivery::with_timeouts(
            WEBHOOK_CONNECT_TIMEOUT,
            WEBHOOK_TOTAL_TIMEOUT,
        )
        .map_err(|_| NotificationRuntimeStartError::InputInvalid)?;
        let claims_enabled: ClaimsEnabledProbe =
            Arc::new(move || health.snapshot().claims_enabled());
        Self::spawn_with_components(
            store.notification(),
            Arc::new(delivery),
            Arc::new(SystemRuntimeClock),
            claims_enabled,
            DEFAULT_POLL_INTERVAL,
            STOP_WAIT_TIMEOUT,
        )
    }

    fn spawn_with_components(
        repository: NotificationRepository,
        delivery: Arc<dyn NotificationDelivery>,
        clock: Arc<dyn RuntimeClock>,
        claims_enabled: ClaimsEnabledProbe,
        poll_interval: Duration,
        stop_wait_timeout: Duration,
    ) -> Result<Self, NotificationRuntimeStartError> {
        let worker = NotificationDeliveryWorker::new(
            repository,
            delivery,
            clock,
            format!("notification-worker-{}", uuid::Uuid::new_v4()),
            uuid::Uuid::new_v4().to_string(),
            LEASE_DURATION_MS,
            RETRY_DELAY_MS,
        )
        .map_err(map_delivery_contract)?;
        let async_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(NotificationRuntimeStartError::AsyncRuntime)?;
        let cancellation = RetentionDeleteCancellationToken::default();
        let thread_cancellation = cancellation.clone();
        let cancel_in_flight = cancellation.clone();
        let inner = QueueRuntimeHandle::spawn(
            "dbbackup-notification-runtime",
            poll_interval,
            stop_wait_timeout,
            claims_enabled,
            Arc::new(move || cancel_in_flight.cancel()),
            move || {
                if let Err(error) =
                    async_runtime.block_on(worker.run_once_with_cancellation(&thread_cancellation))
                {
                    tracing::error!(
                        error_code = error.code(),
                        "notification delivery worker tick failed closed"
                    );
                }
            },
        )
        .map_err(map_queue_start)?;
        Ok(Self { inner })
    }

    pub fn request_stop(&self) {
        self.inner.request_stop();
    }

    pub fn wait_stopped(&self) -> Result<(), HostError> {
        self.inner.wait_stopped()
    }
}

impl ClaimRuntime for ServiceNotificationDeliveryRuntimeHandle {
    fn request_stop(&self) {
        Self::request_stop(self);
    }

    fn wait_stopped(&self) -> Result<(), HostError> {
        Self::wait_stopped(self)
    }
}

fn map_delivery_contract(
    _error: NotificationDeliveryContractError,
) -> NotificationRuntimeStartError {
    NotificationRuntimeStartError::InputInvalid
}

fn map_queue_start(error: QueueRuntimeStartError) -> NotificationRuntimeStartError {
    match error {
        QueueRuntimeStartError::InputInvalid => NotificationRuntimeStartError::InputInvalid,
        QueueRuntimeStartError::ThreadStart(error) => {
            NotificationRuntimeStartError::ThreadStart(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    use async_trait::async_trait;
    use dbbackup_catalog::{
        rusqlite::Connection, NotificationChannelSpec, NotificationRuleSpec, OperationalStore,
        OutboxSpec,
    };
    use dbbackup_runtime::{
        NotificationDelivery, NotificationDeliveryError, NotificationDeliveryReceipt,
        NotificationDeliveryRequest, RuntimeClock,
    };
    use tempfile::TempDir;

    use super::ServiceNotificationDeliveryRuntimeHandle;

    #[test]
    fn notification_handle_ticks_immediately_and_repeats() {
        let fixture = Fixture::new();
        fixture.enqueue("outbox-a");
        fixture.enqueue("outbox-b");
        let handle = ServiceNotificationDeliveryRuntimeHandle::spawn_with_components(
            fixture.store.notification(),
            Arc::new(ImmediateDelivery),
            Arc::new(IncreasingClock::new(1_000)),
            Arc::new(|| true),
            Duration::from_millis(10),
            Duration::from_secs(1),
        )
        .expect("spawn notification runtime");

        wait_until(Duration::from_secs(1), || {
            fixture.state_count("delivered") == 2
        });
        handle.request_stop();
        handle.wait_stopped().expect("join notification runtime");
    }

    #[test]
    fn notification_handle_does_not_claim_when_claims_are_disabled() {
        let fixture = Fixture::new();
        fixture.enqueue("outbox-disabled");
        let handle = ServiceNotificationDeliveryRuntimeHandle::spawn_with_components(
            fixture.store.notification(),
            Arc::new(ImmediateDelivery),
            Arc::new(IncreasingClock::new(1_000)),
            Arc::new(|| false),
            Duration::from_millis(5),
            Duration::from_secs(1),
        )
        .expect("spawn notification runtime");

        std::thread::sleep(Duration::from_millis(40));
        handle.request_stop();
        handle.wait_stopped().expect("join notification runtime");
        assert_eq!(fixture.state_count("pending"), 1);
        assert_eq!(fixture.attempt_count(), 0);
    }

    #[test]
    fn notification_stop_cancels_in_flight_and_never_claims_the_next_item() {
        let fixture = Fixture::new();
        fixture.enqueue("outbox-first");
        fixture.enqueue("outbox-second");
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let handle = ServiceNotificationDeliveryRuntimeHandle::spawn_with_components(
            fixture.store.notification(),
            Arc::new(BlockingDelivery {
                entered: entered_tx,
            }),
            Arc::new(IncreasingClock::new(1_000)),
            Arc::new(|| true),
            Duration::from_millis(5),
            Duration::from_secs(1),
        )
        .expect("spawn notification runtime");
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first delivery entered");

        handle.request_stop();
        handle.wait_stopped().expect("join notification runtime");
        assert_eq!(fixture.state_count("delivered"), 0);
        assert_eq!(fixture.state_count("pending"), 2);
        assert_eq!(fixture.attempt_count(), 1);
        assert_eq!(
            fixture.last_error_code("outbox-first").as_deref(),
            Some("NOTIFICATION_DELIVERY_SERVICE_CANCELLED")
        );
    }

    struct ImmediateDelivery;

    #[async_trait]
    impl NotificationDelivery for ImmediateDelivery {
        async fn deliver(
            &self,
            _request: &NotificationDeliveryRequest,
        ) -> Result<NotificationDeliveryReceipt, NotificationDeliveryError> {
            Ok(NotificationDeliveryReceipt::new("HTTP_204", "accepted").expect("delivery receipt"))
        }
    }

    struct BlockingDelivery {
        entered: mpsc::SyncSender<()>,
    }

    #[async_trait]
    impl NotificationDelivery for BlockingDelivery {
        async fn deliver(
            &self,
            _request: &NotificationDeliveryRequest,
        ) -> Result<NotificationDeliveryReceipt, NotificationDeliveryError> {
            let _ = self.entered.try_send(());
            std::future::pending().await
        }
    }

    struct IncreasingClock {
        now_ms: AtomicI64,
    }

    impl IncreasingClock {
        const fn new(now_ms: i64) -> Self {
            Self {
                now_ms: AtomicI64::new(now_ms),
            }
        }
    }

    impl RuntimeClock for IncreasingClock {
        fn now_ms(&self) -> i64 {
            self.now_ms.fetch_add(1, Ordering::AcqRel)
        }
    }

    struct Fixture {
        _temp: TempDir,
        database_path: std::path::PathBuf,
        store: OperationalStore,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().expect("temp directory");
            let database_path = temp.path().join("operational.db");
            let store = OperationalStore::open(database_path.clone()).expect("operational store");
            let repository = store.notification();
            repository
                .insert_channel(NotificationChannelSpec::webhook(
                    "channel-a",
                    "operations",
                    r#"{"url":"https://example.invalid/hook"}"#,
                    100,
                ))
                .expect("notification channel");
            repository
                .mark_channel_ready("channel-a", 101)
                .expect("ready channel");
            repository
                .insert_rule(NotificationRuleSpec::new(
                    "rule-a",
                    "backup failures",
                    "channel-a",
                    r#"{"eventCodes":["BACKUP_FAILED"]}"#,
                    "error",
                    0,
                    102,
                ))
                .expect("notification rule");
            Self {
                _temp: temp,
                database_path,
                store,
            }
        }

        fn enqueue(&self, outbox_id: &str) {
            self.store
                .notification()
                .enqueue(OutboxSpec::new(
                    outbox_id,
                    "rule-a",
                    format!("{outbox_id}:failed"),
                    r#"{"event":"BACKUP_FAILED","runId":"run-a"}"#,
                    1_000,
                    3,
                ))
                .expect("enqueue notification");
        }

        fn state_count(&self, state: &str) -> i64 {
            Connection::open(&self.database_path)
                .expect("open notification ledger")
                .query_row(
                    "SELECT COUNT(*) FROM notification_outbox WHERE state = ?1",
                    [state],
                    |row| row.get(0),
                )
                .expect("notification state count")
        }

        fn attempt_count(&self) -> i64 {
            Connection::open(&self.database_path)
                .expect("open notification ledger")
                .query_row(
                    "SELECT COUNT(*) FROM notification_delivery_attempts",
                    [],
                    |row| row.get(0),
                )
                .expect("notification attempt count")
        }

        fn last_error_code(&self, outbox_id: &str) -> Option<String> {
            Connection::open(&self.database_path)
                .expect("open notification ledger")
                .query_row(
                    "SELECT last_error_code FROM notification_outbox WHERE id = ?1",
                    [outbox_id],
                    |row| row.get(0),
                )
                .expect("notification last error code")
        }
    }

    fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if predicate() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition was not reached before timeout");
    }
}
