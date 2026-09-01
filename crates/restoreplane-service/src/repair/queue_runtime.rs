use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::host::HostError;

pub(super) type ClaimsEnabledProbe = Arc<dyn Fn() -> bool + Send + Sync>;
pub(super) type CancellationHook = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Error)]
pub(super) enum QueueRuntimeStartError {
    #[error("QUEUE_RUNTIME_INPUT_INVALID")]
    InputInvalid,
    #[error("QUEUE_RUNTIME_THREAD_START")]
    ThreadStart(#[source] std::io::Error),
}

pub(super) struct QueueRuntimeHandle {
    stop: mpsc::SyncSender<()>,
    stop_requested: Arc<AtomicBool>,
    cancel_in_flight: CancellationHook,
    stopped: Mutex<Option<mpsc::Receiver<()>>>,
    stop_wait_timeout: Duration,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl QueueRuntimeHandle {
    pub(super) fn spawn(
        thread_name: &str,
        poll_interval: Duration,
        stop_wait_timeout: Duration,
        claims_enabled: ClaimsEnabledProbe,
        cancel_in_flight: CancellationHook,
        mut tick: impl FnMut() + Send + 'static,
    ) -> Result<Self, QueueRuntimeStartError> {
        if thread_name.is_empty()
            || thread_name.len() > 128
            || poll_interval.is_zero()
            || stop_wait_timeout.is_zero()
        {
            return Err(QueueRuntimeStartError::InputInvalid);
        }
        let (stop, stop_receiver) = mpsc::sync_channel(1);
        let (stopped_sender, stopped_receiver) = mpsc::sync_channel(1);
        let stop_requested = Arc::new(AtomicBool::new(false));
        let thread_stop_requested = Arc::clone(&stop_requested);
        let thread = thread::Builder::new()
            .name(thread_name.to_owned())
            .spawn(move || {
                loop {
                    if thread_stop_requested.load(Ordering::Acquire) {
                        break;
                    }
                    if claims_enabled() {
                        if thread_stop_requested.load(Ordering::Acquire) {
                            break;
                        }
                        tick();
                    }
                    if thread_stop_requested.load(Ordering::Acquire) {
                        break;
                    }
                    match stop_receiver.recv_timeout(poll_interval) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                let _ = stopped_sender.send(());
            })
            .map_err(QueueRuntimeStartError::ThreadStart)?;
        Ok(Self {
            stop,
            stop_requested,
            cancel_in_flight,
            stopped: Mutex::new(Some(stopped_receiver)),
            stop_wait_timeout,
            thread: Mutex::new(Some(thread)),
        })
    }

    pub(super) fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::Release);
        (self.cancel_in_flight)();
        let _ = self.stop.try_send(());
    }

    pub(super) fn wait_stopped(&self) -> Result<(), HostError> {
        let wait_started = Instant::now();
        let stopped = self
            .stopped
            .lock()
            .map_err(|_| HostError::InternalState)?
            .take();
        if let Some(stopped) = stopped {
            match stopped.recv_timeout(self.stop_wait_timeout) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.thread
                        .lock()
                        .map_err(|_| HostError::InternalState)?
                        .take();
                    return Err(HostError::RuntimeFailed);
                }
            }
        }
        let thread = self
            .thread
            .lock()
            .map_err(|_| HostError::InternalState)?
            .take();
        if let Some(thread) = thread {
            while !thread.is_finished() {
                let remaining = self
                    .stop_wait_timeout
                    .saturating_sub(wait_started.elapsed());
                if remaining.is_zero() {
                    return Err(HostError::RuntimeFailed);
                }
                thread::sleep(remaining.min(Duration::from_millis(5)));
            }
            thread.join().map_err(|_| HostError::RuntimeFailed)?;
        }
        Ok(())
    }
}

impl Drop for QueueRuntimeHandle {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.wait_stopped();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use super::QueueRuntimeHandle;

    #[test]
    fn immediate_tick_repeats_and_stop_join_prevents_new_ticks() {
        let claims_enabled = Arc::new(AtomicBool::new(true));
        let ticks = Arc::new(AtomicUsize::new(0));
        let tick_count = Arc::clone(&ticks);
        let gate = Arc::clone(&claims_enabled);
        let handle = QueueRuntimeHandle::spawn(
            "queue-runtime-contract",
            Duration::from_millis(10),
            Duration::from_secs(1),
            Arc::new(move || gate.load(Ordering::Acquire)),
            Arc::new(|| {}),
            move || {
                tick_count.fetch_add(1, Ordering::AcqRel);
            },
        )
        .expect("spawn queue runtime");

        wait_until(Duration::from_secs(1), || {
            ticks.load(Ordering::Acquire) >= 2
        });
        handle.request_stop();
        handle.wait_stopped().expect("join queue runtime");
        let stopped_at = ticks.load(Ordering::Acquire);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(ticks.load(Ordering::Acquire), stopped_at);
    }

    #[test]
    fn claims_disabled_is_a_no_op() {
        let ticks = Arc::new(AtomicUsize::new(0));
        let tick_count = Arc::clone(&ticks);
        let handle = QueueRuntimeHandle::spawn(
            "queue-runtime-disabled-contract",
            Duration::from_millis(5),
            Duration::from_secs(1),
            Arc::new(|| false),
            Arc::new(|| {}),
            move || {
                tick_count.fetch_add(1, Ordering::AcqRel);
            },
        )
        .expect("spawn queue runtime");

        std::thread::sleep(Duration::from_millis(40));
        handle.request_stop();
        handle.wait_stopped().expect("join queue runtime");
        assert_eq!(ticks.load(Ordering::Acquire), 0);
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
