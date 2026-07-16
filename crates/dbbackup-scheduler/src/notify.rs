// src/notify.rs — 알림 추상화(FR7). 기본값(log + Windows Event Log)은 항상 켜져 있고,
// webhook/email은 [notify] config로 옵트인하는 추가 채널이다. 채널 간 실패는 격리된다.
use async_trait::async_trait;
use dbbackup_config::schema::Notify as NotifyConfig;
use dbbackup_core::NotifyChannel;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    Success,
    PartialSuccess,
    Failed,
    SkippedOverlap,
}

#[derive(Debug, Clone)]
pub struct RunEvent {
    pub job: String,
    pub backup_id: Option<String>,
    pub outcome: RunOutcome,
    pub started_at: OffsetDateTime,
    pub finished_at: Option<OffsetDateTime>,
    /// 이미 scrub()을 거친 메시지만 넣는다 — 호출부 책임(redact.rs와 이중 방어).
    pub message: String,
    pub rpo_summary: Option<String>,
}

#[async_trait]
pub trait NotificationSink: Send + Sync {
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()>;
    fn name(&self) -> &'static str;
}

/// EmailPlaceholderSink가 매 run마다 남기는 경고 문구(테스트에서 문자열 자체를 검증).
pub fn email_placeholder_warning(job: &str) -> String {
    format!(
        "notify.channel=email은 아직 구현되지 않음(plan.md Open Questions) — job '{job}'의 이 알림은 \
         전송되지 않았음. log+event 채널만 신뢰할 것"
    )
}

/// 여러 sink에 순차 통지하되, 하나가 실패해도 나머지를 막지 않는다(FR7 채널 격리).
/// 로그+이벤트는 기본값으로 항상 포함되어야 하며, webhook 실패가 이 둘을 방해해서는 안 된다.
pub struct CompositeSink {
    sinks: Vec<Box<dyn NotificationSink>>,
}

impl CompositeSink {
    /// 임의 sink 목록으로 조립(테스트/특수 배선용).
    pub fn from_sinks(sinks: Vec<Box<dyn NotificationSink>>) -> Self {
        CompositeSink { sinks }
    }

    /// 기본 sink(log + Windows Event Log)에 config 채널을 얹는다.
    /// webhook은 S02 SecretManager가 EncField를 평문 URL로 언랩한 뒤 `webhook_url_plaintext`로 주입한다.
    /// (config만으로는 복호화할 수 없으므로 여기서 url을 강제로 다루지 않는다 — 배선 계약을 고정.)
    pub fn default_plus_config(
        notify_cfg: Option<&NotifyConfig>,
        webhook_url_plaintext: Option<String>,
    ) -> Self {
        let mut sinks: Vec<Box<dyn NotificationSink>> =
            vec![Box::new(LogFileSink), Box::new(WindowsEventLogSink::new())];
        if let Some(cfg) = notify_cfg {
            match cfg.channel {
                NotifyChannel::None => {}
                NotifyChannel::Webhook => match webhook_url_plaintext {
                    Some(url) => sinks.push(Box::new(WebhookSink::new(url))),
                    None => {
                        tracing::warn!(
                            "notify.channel=webhook이나 언랩된 webhook_url이 없음 — webhook sink 미등록"
                        );
                    }
                },
                NotifyChannel::Email => {
                    // 조용히 무시하지 않고 매 run마다 명시적으로 경고(설정했는데 알림이 안 가는 상황 방지).
                    sinks.push(Box::new(EmailPlaceholderSink));
                }
            }
        }
        CompositeSink { sinks }
    }

    /// 현재 조립된 sink 이름 목록(테스트에서 라우팅 검증에 사용).
    pub fn sink_names(&self) -> Vec<&'static str> {
        self.sinks.iter().map(|s| s.name()).collect()
    }

    pub async fn notify_all(&self, event: &RunEvent) {
        for sink in &self.sinks {
            if let Err(e) = sink.notify(event).await {
                // 리댁션 이중 방어: 외부 SDK 에러가 시크릿을 담을 수 있으므로 scrub 후 로그.
                tracing::error!(
                    sink = sink.name(),
                    error = %crate::redact::scrub(&e.to_string()),
                    "알림 채널 전송 실패(다른 채널에는 영향 없음)"
                );
            }
        }
    }
}

/// tracing이 이미 파일 기록을 수행(RedactingWriter 경유). 이 sink는 run 종료를 요약 라인으로 한 번 더
/// 명시적으로 남겨 알림 이력을 로그에서 grep하기 쉽게 한다.
pub struct LogFileSink;

#[async_trait]
impl NotificationSink for LogFileSink {
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()> {
        tracing::info!(
            job = %event.job,
            outcome = ?event.outcome,
            backup_id = ?event.backup_id,
            "run 종료 요약: {}",
            event.message
        );
        Ok(())
    }
    fn name(&self) -> &'static str {
        "log"
    }
}

pub struct EmailPlaceholderSink;

#[async_trait]
impl NotificationSink for EmailPlaceholderSink {
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()> {
        tracing::warn!("{}", email_placeholder_warning(&event.job));
        Ok(())
    }
    fn name(&self) -> &'static str {
        "email(placeholder)"
    }
}

pub struct WebhookSink {
    /// SecretManager로 복호화된 평문 URL. 절대 로그에 출력하지 않는다.
    url: String,
    client: reqwest::Client,
}

impl WebhookSink {
    pub fn new(url: String) -> Self {
        WebhookSink {
            url,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl NotificationSink for WebhookSink {
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "job": event.job,
            "outcome": format!("{:?}", event.outcome),
            "backup_id": event.backup_id,
            "message": event.message, // 호출부가 이미 scrub한 메시지만 여기 도달(FR7)
            "rpo": event.rpo_summary,
        });
        self.client
            .post(&self.url)
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
    fn name(&self) -> &'static str {
        "webhook"
    }
}

// ---- Windows Event Log sink (ReportEventW) ----
#[cfg(windows)]
mod eventlog {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Security::PSID;
    use windows::Win32::System::EventLog::{
        DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE,
        EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
    };

    const SOURCE_NAME: &str = "dbbackup";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub struct EventLogHandle(HANDLE);

    // HANDLE은 !Send/!Sync이지만, 이 핸들은 OnceLock 뒤에서만 접근되고 ReportEventW는 스레드-세이프하다.
    unsafe impl Send for EventLogHandle {}
    unsafe impl Sync for EventLogHandle {}

    impl EventLogHandle {
        pub fn open() -> anyhow::Result<Self> {
            let name = wide(SOURCE_NAME);
            // EventMessageFile 레지스트리 미등록이어도 ReportEventW는 성공한다(Viewer에 디스클레이머만 붙음).
            let h = unsafe { RegisterEventSourceW(PCWSTR::null(), PCWSTR(name.as_ptr()))? };
            Ok(EventLogHandle(h))
        }

        pub fn report(
            &self,
            level: EventLevel,
            event_id: u32,
            message: &str,
        ) -> anyhow::Result<()> {
            let msg_wide = wide(message);
            let strings = [PCWSTR(msg_wide.as_ptr())];
            let ev_type = match level {
                EventLevel::Info => EVENTLOG_INFORMATION_TYPE,
                EventLevel::Warning => EVENTLOG_WARNING_TYPE,
                EventLevel::Error => EVENTLOG_ERROR_TYPE,
            };
            // windows 0.58 ReportEventW: wnumstrings는 lpstrings 슬라이스 길이에서 내부 계산됨.
            // lpusersid는 PSID::default()(널)로, lprawdata는 None으로 넘긴다.
            unsafe {
                ReportEventW(
                    self.0,
                    ev_type,
                    0,
                    event_id,
                    PSID::default(),
                    0,
                    Some(&strings),
                    None,
                )?;
            }
            Ok(())
        }
    }

    impl Drop for EventLogHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = DeregisterEventSource(self.0);
            }
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub enum EventLevel {
        Info,
        Warning,
        Error,
    }
}

pub struct WindowsEventLogSink {
    #[cfg(windows)]
    handle: std::sync::OnceLock<anyhow::Result<eventlog::EventLogHandle>>,
}

impl WindowsEventLogSink {
    pub fn new() -> Self {
        #[cfg(windows)]
        {
            WindowsEventLogSink {
                handle: std::sync::OnceLock::new(),
            }
        }
        #[cfg(not(windows))]
        {
            WindowsEventLogSink {}
        }
    }
}

impl Default for WindowsEventLogSink {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NotificationSink for WindowsEventLogSink {
    #[cfg(windows)]
    async fn notify(&self, event: &RunEvent) -> anyhow::Result<()> {
        let handle = self.handle.get_or_init(eventlog::EventLogHandle::open);
        let handle = handle.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
        let (level, event_id) = match event.outcome {
            RunOutcome::Success => (eventlog::EventLevel::Info, 1000),
            RunOutcome::PartialSuccess => (eventlog::EventLevel::Warning, 1001),
            RunOutcome::Failed => (eventlog::EventLevel::Error, 1002),
            RunOutcome::SkippedOverlap => (eventlog::EventLevel::Warning, 1003),
        };
        // event.message는 이미 scrub()을 거친 문자열만 허용(FR8, 호출부 계약).
        handle.report(
            level,
            event_id,
            &format!("[{}] {}", event.job, event.message),
        )
    }

    #[cfg(not(windows))]
    async fn notify(&self, _event: &RunEvent) -> anyhow::Result<()> {
        tracing::debug!("Windows Event Log는 Windows 대상에서만 동작함(비Windows 빌드는 no-op)");
        Ok(())
    }

    fn name(&self) -> &'static str {
        "windows-event-log"
    }
}
