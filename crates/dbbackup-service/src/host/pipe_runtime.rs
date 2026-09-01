use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio::sync::watch;

use crate::ipc::{IpcFrameHandler, PipeServerReport, PipeTransportError, SecurePipeServer};

use super::{ClaimRuntime, HostError, HostRuntime, ShutdownReason};

const PIPE_READY_TIMEOUT: Duration = Duration::from_secs(10);

pub struct PipeHostRuntime {
    state: Mutex<PipeRuntimeState>,
    claim_runtimes: Mutex<Vec<Arc<dyn ClaimRuntime>>>,
}

#[derive(Default)]
struct PipeRuntimeState {
    attaching: bool,
    running: Option<RunningPipe>,
    last_report: Option<PipeServerReport>,
}

struct RunningPipe {
    shutdown: watch::Sender<bool>,
    thread: JoinHandle<Result<PipeServerReport, PipeTransportError>>,
}

impl PipeHostRuntime {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(PipeRuntimeState::default()),
            claim_runtimes: Mutex::new(Vec::new()),
        }
    }

    pub fn attach_claim_runtime<T>(&self, runtime: Arc<T>) -> Result<(), HostError>
    where
        T: ClaimRuntime,
    {
        self.claim_runtimes
            .lock()
            .map_err(|_| HostError::InternalState)?
            .push(runtime);
        Ok(())
    }

    pub fn attach<H>(&self, server: SecurePipeServer<H>) -> Result<(), HostError>
    where
        H: IpcFrameHandler,
    {
        {
            let mut state = self.state.lock().map_err(|_| HostError::InternalState)?;
            if state.attaching || state.running.is_some() {
                return Err(HostError::RuntimeFailed);
            }
            state.attaching = true;
            state.last_report = None;
        }

        let (shutdown, shutdown_receiver) = watch::channel(false);
        let (ready, ready_receiver) = mpsc::sync_channel(1);
        let thread = match thread::Builder::new()
            .name("dbbackup-pipe-host".to_owned())
            .spawn(move || {
                let runtime = Builder::new_multi_thread()
                    .worker_threads(2)
                    .enable_all()
                    .build()
                    .map_err(PipeTransportError::Io)?;
                runtime.block_on(server.serve_until_ready(shutdown_receiver, ready))
            }) {
            Ok(thread) => thread,
            Err(_) => {
                self.clear_attaching();
                return Err(HostError::RuntimeFailed);
            }
        };

        match ready_receiver.recv_timeout(PIPE_READY_TIMEOUT) {
            Ok(Ok(())) => {
                let mut state = self.state.lock().map_err(|_| {
                    let _ = shutdown.send(true);
                    HostError::InternalState
                })?;
                state.attaching = false;
                state.running = Some(RunningPipe { shutdown, thread });
                Ok(())
            }
            Ok(Err(_)) | Err(_) => {
                let _ = shutdown.send(true);
                let _ = thread.join();
                self.clear_attaching();
                Err(HostError::RuntimeFailed)
            }
        }
    }

    pub fn last_report(&self) -> Option<PipeServerReport> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .last_report
            .clone()
    }

    fn clear_attaching(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.attaching = false;
        }
    }

    fn take_running(&self) -> Result<Option<RunningPipe>, HostError> {
        let mut state = self.state.lock().map_err(|_| HostError::InternalState)?;
        Ok(state.running.take())
    }

    fn store_report(&self, report: PipeServerReport) -> Result<(), HostError> {
        let mut state = self.state.lock().map_err(|_| HostError::InternalState)?;
        state.last_report = Some(report);
        Ok(())
    }
}

impl Default for PipeHostRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl HostRuntime for PipeHostRuntime {
    fn started(&self, _diagnostics_only: bool) -> Result<(), HostError> {
        Ok(())
    }

    fn stop_accepting_ipc(&self) -> Result<(), HostError> {
        let state = self.state.lock().map_err(|_| HostError::InternalState)?;
        if let Some(running) = &state.running {
            running
                .shutdown
                .send(true)
                .map_err(|_| HostError::RuntimeFailed)?;
        }
        Ok(())
    }

    fn stop_claiming_work(&self) -> Result<(), HostError> {
        let runtimes = self
            .claim_runtimes
            .lock()
            .map_err(|_| HostError::InternalState)?
            .clone();
        for runtime in runtimes {
            runtime.request_stop();
        }
        Ok(())
    }

    fn drain_clients(&self) -> Result<(), HostError> {
        if let Some(running) = self.take_running()? {
            let report = running
                .thread
                .join()
                .map_err(|_| HostError::RuntimeFailed)?
                .map_err(|_| HostError::RuntimeFailed)?;
            self.store_report(report)?;
        }
        let runtimes = self
            .claim_runtimes
            .lock()
            .map_err(|_| HostError::InternalState)?
            .clone();
        for runtime in runtimes {
            runtime.wait_stopped()?;
        }
        Ok(())
    }

    fn checkpointed(&self) -> Result<(), HostError> {
        Ok(())
    }

    fn stopped(&self, _reason: ShutdownReason) {}
}
