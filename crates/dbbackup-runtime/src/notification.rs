use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dbbackup_catalog::{
    DeliveryClaim, DeliveryClaimRequest, NotificationRepository, RepositoryError,
};
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use thiserror::Error;

use crate::retention_worker::RetentionDeleteCancellationToken;
use crate::RuntimeClock;

const SERVICE_CANCELLED_CODE: &str = "NOTIFICATION_DELIVERY_SERVICE_CANCELLED";

pub struct NotificationDeliveryRequest {
    outbox_id: String,
    attempt_no: i64,
    fencing_token: i64,
    channel_kind: String,
    channel_config_json: String,
    deduplication_key: String,
    payload_json: String,
}

impl NotificationDeliveryRequest {
    fn from_claim(claim: &DeliveryClaim) -> Self {
        Self {
            outbox_id: claim.outbox_id().to_owned(),
            attempt_no: claim.attempt_no(),
            fencing_token: claim.fencing_token(),
            channel_kind: claim.channel_kind().to_owned(),
            channel_config_json: claim.channel_config_json().to_owned(),
            deduplication_key: claim.deduplication_key().to_owned(),
            payload_json: claim.payload_json().to_owned(),
        }
    }

    pub fn outbox_id(&self) -> &str {
        &self.outbox_id
    }

    pub const fn attempt_no(&self) -> i64 {
        self.attempt_no
    }

    pub const fn fencing_token(&self) -> i64 {
        self.fencing_token
    }

    pub fn channel_kind(&self) -> &str {
        &self.channel_kind
    }

    pub fn channel_config_json(&self) -> &str {
        &self.channel_config_json
    }

    pub fn deduplication_key(&self) -> &str {
        &self.deduplication_key
    }

    pub fn payload_json(&self) -> &str {
        &self.payload_json
    }
}

pub struct NotificationDeliveryReceipt {
    response_code: String,
    response_summary: String,
}

impl NotificationDeliveryReceipt {
    pub fn new(
        response_code: impl Into<String>,
        response_summary: impl Into<String>,
    ) -> Result<Self, NotificationDeliveryContractError> {
        let value = Self {
            response_code: response_code.into(),
            response_summary: response_summary.into(),
        };
        if !valid_code(&value.response_code) || !valid_redacted_summary(&value.response_summary) {
            return Err(NotificationDeliveryContractError::InputInvalid);
        }
        Ok(value)
    }
}

pub struct NotificationDeliveryError {
    code: String,
    summary: String,
    retryable: bool,
}

impl NotificationDeliveryError {
    pub fn retryable(
        code: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<Self, NotificationDeliveryContractError> {
        Self::new(code, summary, true)
    }

    pub fn terminal(
        code: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<Self, NotificationDeliveryContractError> {
        Self::new(code, summary, false)
    }

    fn new(
        code: impl Into<String>,
        summary: impl Into<String>,
        retryable: bool,
    ) -> Result<Self, NotificationDeliveryContractError> {
        let value = Self {
            code: code.into(),
            summary: summary.into(),
            retryable,
        };
        if !valid_code(&value.code) || !valid_redacted_summary(&value.summary) {
            return Err(NotificationDeliveryContractError::InputInvalid);
        }
        Ok(value)
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    pub const fn is_retryable(&self) -> bool {
        self.retryable
    }
}

#[async_trait]
pub trait NotificationDelivery: Send + Sync {
    async fn deliver(
        &self,
        request: &NotificationDeliveryRequest,
    ) -> Result<NotificationDeliveryReceipt, NotificationDeliveryError>;
}

pub struct WebhookNotificationDelivery {
    client: reqwest::Client,
}

impl WebhookNotificationDelivery {
    pub fn with_timeouts(
        connect_timeout: Duration,
        total_timeout: Duration,
    ) -> Result<Self, NotificationDeliveryContractError> {
        if connect_timeout.is_zero() || total_timeout.is_zero() || connect_timeout > total_timeout {
            return Err(NotificationDeliveryContractError::InputInvalid);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(total_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| NotificationDeliveryContractError::InputInvalid)?;
        Ok(Self { client })
    }
}

#[async_trait]
impl NotificationDelivery for WebhookNotificationDelivery {
    async fn deliver(
        &self,
        request: &NotificationDeliveryRequest,
    ) -> Result<NotificationDeliveryReceipt, NotificationDeliveryError> {
        if request.channel_kind() != "webhook" {
            return Err(delivery_error(
                "CHANNEL_KIND_UNSUPPORTED",
                "notification channel kind is unsupported",
                false,
            ));
        }
        let url = parse_webhook_url(request.channel_config_json())?;
        if request.payload_json().len() > 256 * 1024
            || serde_json::from_str::<serde_json::Value>(request.payload_json())
                .ok()
                .and_then(|value| value.as_object().cloned())
                .is_none()
        {
            return Err(delivery_error(
                "PAYLOAD_INVALID",
                "notification payload is invalid",
                false,
            ));
        }
        let idempotency_key = HeaderValue::from_str(request.deduplication_key()).map_err(|_| {
            delivery_error(
                "DEDUPLICATION_KEY_INVALID",
                "notification deduplication key is invalid",
                false,
            )
        })?;
        let outbox_id = HeaderValue::from_str(request.outbox_id()).map_err(|_| {
            delivery_error(
                "OUTBOX_ID_INVALID",
                "notification outbox identifier is invalid",
                false,
            )
        })?;

        let response = self
            .client
            .post(url)
            .header(CONTENT_TYPE, "application/json")
            .header("Idempotency-Key", idempotency_key)
            .header("X-Dbbackup-Outbox-Id", outbox_id)
            .header("X-Dbbackup-Attempt", request.attempt_no().to_string())
            .header(
                "X-Dbbackup-Fencing-Token",
                request.fencing_token().to_string(),
            )
            .body(request.payload_json().to_owned())
            .send()
            .await
            .map_err(classify_transport_error)?;
        let status = response.status();
        let response_code = format!("HTTP_{}", status.as_u16());
        if status.is_success() {
            return Ok(NotificationDeliveryReceipt {
                response_code,
                response_summary: "endpoint accepted notification".to_owned(),
            });
        }
        let retryable = status.as_u16() == 408
            || status.as_u16() == 425
            || status.as_u16() == 429
            || status.is_server_error();
        Err(delivery_error(
            &response_code,
            if retryable {
                "endpoint temporarily rejected notification"
            } else {
                "endpoint rejected notification"
            },
            retryable,
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotificationWorkerOutcome {
    Idle,
    Delivered {
        outbox_id: String,
    },
    RetryScheduled {
        outbox_id: String,
        available_at_ms: i64,
    },
    FailedTerminal {
        outbox_id: String,
    },
}

pub struct NotificationDeliveryWorker {
    repository: NotificationRepository,
    delivery: Arc<dyn NotificationDelivery>,
    clock: Arc<dyn RuntimeClock>,
    worker_id: String,
    worker_boot_id: String,
    lease_duration_ms: i64,
    retry_delay_ms: i64,
}

impl NotificationDeliveryWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repository: NotificationRepository,
        delivery: Arc<dyn NotificationDelivery>,
        clock: Arc<dyn RuntimeClock>,
        worker_id: impl Into<String>,
        worker_boot_id: impl Into<String>,
        lease_duration_ms: i64,
        retry_delay_ms: i64,
    ) -> Result<Self, NotificationDeliveryContractError> {
        let value = Self {
            repository,
            delivery,
            clock,
            worker_id: worker_id.into(),
            worker_boot_id: worker_boot_id.into(),
            lease_duration_ms,
            retry_delay_ms,
        };
        if value.worker_id.is_empty()
            || value.worker_id.len() > 128
            || value.worker_boot_id.is_empty()
            || value.worker_boot_id.len() > 128
            || value.lease_duration_ms <= 0
            || value.retry_delay_ms <= 0
        {
            return Err(NotificationDeliveryContractError::InputInvalid);
        }
        Ok(value)
    }

    pub async fn run_once(&self) -> Result<NotificationWorkerOutcome, NotificationWorkerError> {
        self.run_once_inner(None).await
    }

    pub async fn run_once_with_cancellation(
        &self,
        cancellation: &RetentionDeleteCancellationToken,
    ) -> Result<NotificationWorkerOutcome, NotificationWorkerError> {
        self.run_once_inner(Some(cancellation)).await
    }

    async fn run_once_inner(
        &self,
        cancellation: Option<&RetentionDeleteCancellationToken>,
    ) -> Result<NotificationWorkerOutcome, NotificationWorkerError> {
        if cancellation.is_some_and(RetentionDeleteCancellationToken::is_cancelled) {
            return Ok(NotificationWorkerOutcome::Idle);
        }
        let claim_at_ms = self.clock.now_ms();
        if claim_at_ms < 0 {
            return Err(NotificationWorkerError::ClockInvalid);
        }
        self.repository.recover_expired_leases(claim_at_ms)?;
        if cancellation.is_some_and(RetentionDeleteCancellationToken::is_cancelled) {
            return Ok(NotificationWorkerOutcome::Idle);
        }
        let Some(claim) = self.repository.claim_next(DeliveryClaimRequest::new(
            &self.worker_id,
            &self.worker_boot_id,
            claim_at_ms,
            self.lease_duration_ms,
        ))?
        else {
            return Ok(NotificationWorkerOutcome::Idle);
        };
        let request = NotificationDeliveryRequest::from_claim(&claim);
        let outbox_id = request.outbox_id.clone();
        let delivery_result = match cancellation {
            Some(cancellation) => {
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(delivery_error(
                        SERVICE_CANCELLED_CODE,
                        "notification delivery cancelled by service shutdown",
                        true,
                    )),
                    result = self.delivery.deliver(&request) => result,
                }
            }
            None => self.delivery.deliver(&request).await,
        };
        let completed_at_ms = self.clock.now_ms();
        if completed_at_ms < claim_at_ms {
            return Err(NotificationWorkerError::ClockInvalid);
        }

        match delivery_result {
            Ok(receipt) => {
                self.repository.mark_delivered(
                    &claim,
                    &receipt.response_code,
                    &receipt.response_summary,
                    completed_at_ms,
                )?;
                Ok(NotificationWorkerOutcome::Delivered { outbox_id })
            }
            Err(error) if error.retryable => {
                let available_at_ms = completed_at_ms
                    .checked_add(self.retry_delay_ms)
                    .ok_or(NotificationWorkerError::ClockInvalid)?;
                self.repository.fail_for_retry(
                    &claim,
                    &error.code,
                    completed_at_ms,
                    available_at_ms,
                )?;
                Ok(NotificationWorkerOutcome::RetryScheduled {
                    outbox_id,
                    available_at_ms,
                })
            }
            Err(error) => {
                self.repository.mark_failed_terminal(
                    &claim,
                    &error.code,
                    &error.summary,
                    completed_at_ms,
                )?;
                Ok(NotificationWorkerOutcome::FailedTerminal { outbox_id })
            }
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NotificationDeliveryContractError {
    #[error("NOTIFICATION_DELIVERY_INPUT_INVALID")]
    InputInvalid,
}

impl NotificationDeliveryContractError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "NOTIFICATION_DELIVERY_INPUT_INVALID",
        }
    }
}

#[derive(Debug, Error)]
pub enum NotificationWorkerError {
    #[error("NOTIFICATION_WORKER_CLOCK_INVALID")]
    ClockInvalid,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
}

impl NotificationWorkerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::ClockInvalid => "NOTIFICATION_WORKER_CLOCK_INVALID",
            Self::Repository(error) => error.code(),
        }
    }
}

fn valid_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.contains(['\0', '\r', '\n'])
        && value.bytes().all(|byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_redacted_summary(value: &str) -> bool {
    if value.len() > 4 * 1024 || value.contains(['\0', '\r', '\n']) {
        return false;
    }
    let lower = value.to_ascii_lowercase();
    ![
        "authorization:",
        "bearer ",
        "password=",
        "passwd=",
        "token=",
        "api_key",
        "apikey",
        "secret=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn parse_webhook_url(config_json: &str) -> Result<reqwest::Url, NotificationDeliveryError> {
    let config: serde_json::Value = serde_json::from_str(config_json).map_err(|_| {
        delivery_error(
            "CHANNEL_CONFIG_INVALID",
            "notification channel configuration is invalid",
            false,
        )
    })?;
    let object = config.as_object().ok_or_else(|| {
        delivery_error(
            "CHANNEL_CONFIG_INVALID",
            "notification channel configuration is invalid",
            false,
        )
    })?;
    if object.keys().any(|key| key != "url") {
        return Err(delivery_error(
            "CHANNEL_CONFIG_INVALID",
            "notification channel configuration is invalid",
            false,
        ));
    }
    let raw_url = object
        .get("url")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            delivery_error(
                "CHANNEL_CONFIG_INVALID",
                "notification channel configuration is invalid",
                false,
            )
        })?;
    let url = reqwest::Url::parse(raw_url).map_err(|_| {
        delivery_error(
            "CHANNEL_CONFIG_INVALID",
            "notification channel endpoint is invalid",
            false,
        )
    })?;
    let loopback_http = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
    if (url.scheme() != "https" && !loopback_http)
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(delivery_error(
            "CHANNEL_CONFIG_INVALID",
            "notification channel endpoint is not allowed",
            false,
        ));
    }
    Ok(url)
}

fn classify_transport_error(error: reqwest::Error) -> NotificationDeliveryError {
    if error.is_builder() {
        delivery_error(
            "HTTP_REQUEST_INVALID",
            "notification request is invalid",
            false,
        )
    } else if error.is_timeout() {
        delivery_error("HTTP_TIMEOUT", "notification request timed out", true)
    } else if error.is_connect() {
        delivery_error("HTTP_CONNECT", "notification endpoint is unavailable", true)
    } else {
        delivery_error("HTTP_TRANSPORT", "notification transport failed", true)
    }
}

fn delivery_error(code: &str, summary: &str, retryable: bool) -> NotificationDeliveryError {
    debug_assert!(valid_code(code));
    debug_assert!(valid_redacted_summary(summary));
    NotificationDeliveryError {
        code: code.to_owned(),
        summary: summary.to_owned(),
        retryable,
    }
}
