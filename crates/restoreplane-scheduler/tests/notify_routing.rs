// T18~T19 + 라우팅 — 알림 채널 선택 / 채널 격리 / email placeholder 경고.
use restoreplane_config::schema::Notify as NotifyConfig;
use restoreplane_core::{EncField, NotifyChannel};
use restoreplane_scheduler::notify::{
    email_placeholder_warning, CompositeSink, NotificationSink, RunEvent, RunOutcome, WebhookSink,
};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;

fn event() -> RunEvent {
    RunEvent {
        job: "j".to_string(),
        backup_id: None,
        outcome: RunOutcome::Failed,
        started_at: OffsetDateTime::now_utc(),
        finished_at: None,
        message: "msg".to_string(),
        rpo_summary: None,
    }
}

fn webhook_cfg() -> NotifyConfig {
    NotifyConfig {
        channel: NotifyChannel::Webhook,
        webhook_url: Some(EncField::new(1, b"ciphertext".to_vec())),
        email_to: None,
    }
}

fn email_cfg() -> NotifyConfig {
    NotifyConfig {
        channel: NotifyChannel::Email,
        webhook_url: None,
        email_to: Some("ops@example.com".to_string()),
    }
}

#[test]
fn default_sinks_always_present() {
    let c = CompositeSink::default_plus_config(None, None);
    let names = c.sink_names();
    assert!(names.contains(&"log"));
    assert!(names.contains(&"windows-event-log"));
    assert_eq!(names.len(), 2);
}

#[test]
fn webhook_chosen_when_url_resolved() {
    let c = CompositeSink::default_plus_config(
        Some(&webhook_cfg()),
        Some("https://hooks.example/x".to_string()),
    );
    assert!(c.sink_names().contains(&"webhook"));
    // 평문 URL이 없으면 webhook sink는 등록되지 않는다(조용한 오해석 방지).
    let c2 = CompositeSink::default_plus_config(Some(&webhook_cfg()), None);
    assert!(!c2.sink_names().contains(&"webhook"));
}

#[test]
fn email_routes_to_placeholder() {
    let c = CompositeSink::default_plus_config(Some(&email_cfg()), None);
    assert!(c.sink_names().contains(&"email(placeholder)"));
    // placeholder 경고 문구는 "구현되지 않음"을 명시해 조용한 no-op이 아님을 보증한다.
    let warn = email_placeholder_warning("j");
    assert!(warn.contains("email"));
    assert!(warn.contains("구현되지 않음"));
}

#[tokio::test]
async fn webhook_request_has_a_bounded_total_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_secs(2));
    });

    let sink = WebhookSink::with_timeouts(
        format!("http://{address}"),
        Duration::from_millis(50),
        Duration::from_millis(75),
    )
    .unwrap();
    let started = Instant::now();

    let result = sink.notify(&event()).await;

    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}

// --- 채널 격리(T18): 한 sink가 항상 실패해도 다른 sink는 여전히 호출된다. ---
struct FailingSink;
#[async_trait::async_trait]
impl NotificationSink for FailingSink {
    async fn notify(&self, _e: &RunEvent) -> anyhow::Result<()> {
        anyhow::bail!("의도적 실패(webhook 응답에 token=abc 포함)")
    }
    fn name(&self) -> &'static str {
        "failing"
    }
}

struct CountingSink(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl NotificationSink for CountingSink {
    async fn notify(&self, _e: &RunEvent) -> anyhow::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn name(&self) -> &'static str {
        "counting"
    }
}

#[tokio::test]
async fn failing_sink_does_not_block_others() {
    let counter = Arc::new(AtomicUsize::new(0));
    let c = CompositeSink::from_sinks(vec![
        Box::new(FailingSink),
        Box::new(CountingSink(counter.clone())),
        Box::new(FailingSink),
    ]);
    c.notify_all(&event()).await;
    // FailingSink가 앞뒤로 실패해도 CountingSink는 정확히 1회 호출됨.
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}
