use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dbbackup_application::{
    AccessClass, ApplicationError, ApplicationFailure, ApplicationRequest, ApplicationUseCases,
};
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout, Instant};
use windows::Win32::Foundation::ERROR_PIPE_BUSY;
use zeroize::Zeroizing;

use super::{
    authorize_and_decode, encode_response, identity_from_connected_pipe, request_id_hint,
    AuthorizationPolicy, DispatchError, DurableIpcDispatcher, IpcError, PipeSecurityError,
    PipeSecurityPolicy, ResponseEnvelope, TokenIdentity, FIXED_HEADER_BYTES, MAX_FRAME_BODY_BYTES,
    MAX_RESPONSE_BODY_BYTES,
};

const LENGTH_PREFIX_BYTES: usize = 4;
const MAX_CONCURRENT_CLIENTS: usize = 128;
const MAX_IO_TIMEOUT: Duration = Duration::from_secs(120);
const CLIENT_RETRY_INTERVAL: Duration = Duration::from_millis(10);

pub trait IpcFrameHandler: Send + Sync + 'static {
    fn handle(
        &self,
        identity: TokenIdentity,
        frame: &[u8],
        now_ms: i64,
    ) -> Result<Vec<u8>, PipeTransportError>;
}

pub struct ApplicationFrameHandler<U>
where
    U: ApplicationUseCases,
{
    authorization: AuthorizationPolicy,
    dispatcher: DurableIpcDispatcher<U>,
}

impl<U> ApplicationFrameHandler<U>
where
    U: ApplicationUseCases,
{
    pub const fn new(
        authorization: AuthorizationPolicy,
        dispatcher: DurableIpcDispatcher<U>,
    ) -> Self {
        Self {
            authorization,
            dispatcher,
        }
    }
}

impl<U> IpcFrameHandler for ApplicationFrameHandler<U>
where
    U: ApplicationUseCases + 'static,
{
    fn handle(
        &self,
        identity: TokenIdentity,
        frame: &[u8],
        now_ms: i64,
    ) -> Result<Vec<u8>, PipeTransportError> {
        let request_id = request_id_hint(frame)?;
        let response = match authorize_and_decode(&self.authorization, &identity, frame, now_ms) {
            Ok(authorized) => match self.dispatcher.dispatch(authorized) {
                Ok(response) => response,
                Err(error) => redacted_dispatch_failure(request_id, &error)?,
            },
            Err(error) => ResponseEnvelope::redacted_failure(request_id, error.code(), false)?,
        };
        encode_response(&response).map_err(PipeTransportError::from)
    }
}

pub struct ReadOnlyApplicationFrameHandler<U>
where
    U: ApplicationUseCases,
{
    authorization: AuthorizationPolicy,
    use_cases: Arc<U>,
}

impl<U> ReadOnlyApplicationFrameHandler<U>
where
    U: ApplicationUseCases,
{
    pub const fn new(authorization: AuthorizationPolicy, use_cases: Arc<U>) -> Self {
        Self {
            authorization,
            use_cases,
        }
    }
}

impl<U> IpcFrameHandler for ReadOnlyApplicationFrameHandler<U>
where
    U: ApplicationUseCases + 'static,
{
    fn handle(
        &self,
        identity: TokenIdentity,
        frame: &[u8],
        now_ms: i64,
    ) -> Result<Vec<u8>, PipeTransportError> {
        let request_id = request_id_hint(frame)?;
        let response = match authorize_and_decode(&self.authorization, &identity, frame, now_ms) {
            Ok(authorized)
                if authorized.request().command().descriptor().access() != AccessClass::Read =>
            {
                let error = ApplicationError::Forbidden;
                ResponseEnvelope::failure(request_id, ApplicationFailure::from(&error))?
            }
            Ok(authorized) => {
                let request = authorized.request();
                match self.use_cases.execute(ApplicationRequest {
                    command: request.command(),
                    payload: request.payload().clone(),
                }) {
                    Ok(response) => ResponseEnvelope::success(request_id, response)?,
                    Err(error) => {
                        ResponseEnvelope::failure(request_id, ApplicationFailure::from(&error))?
                    }
                }
            }
            Err(error) => ResponseEnvelope::redacted_failure(request_id, error.code(), false)?,
        };
        encode_response(&response).map_err(PipeTransportError::from)
    }
}

fn redacted_dispatch_failure(
    request_id: dbbackup_domain::IpcRequestId,
    error: &DispatchError,
) -> Result<ResponseEnvelope, IpcError> {
    ResponseEnvelope::redacted_failure(request_id, error.code(), false)
}

#[derive(Clone, Debug)]
pub struct PipeServerConfig {
    pipe_name: String,
    max_concurrent_clients: usize,
    io_timeout: Duration,
}

impl PipeServerConfig {
    pub fn new(
        pipe_name: impl Into<String>,
        max_concurrent_clients: usize,
        io_timeout: Duration,
    ) -> Result<Self, PipeTransportError> {
        let pipe_name = pipe_name.into();
        if !pipe_name.starts_with(r"\\.\pipe\DbBackup.")
            || pipe_name.len() > 256
            || pipe_name.chars().any(|character| character == '\0')
            || max_concurrent_clients == 0
            || max_concurrent_clients > MAX_CONCURRENT_CLIENTS
            || io_timeout.is_zero()
            || io_timeout > MAX_IO_TIMEOUT
        {
            return Err(PipeTransportError::ConfigInvalid);
        }
        Ok(Self {
            pipe_name,
            max_concurrent_clients,
            io_timeout,
        })
    }
}

pub struct SecurePipeServer<H>
where
    H: IpcFrameHandler,
{
    config: PipeServerConfig,
    security: PipeSecurityPolicy,
    handler: Arc<H>,
}

impl<H> SecurePipeServer<H>
where
    H: IpcFrameHandler,
{
    pub const fn new(
        config: PipeServerConfig,
        security: PipeSecurityPolicy,
        handler: Arc<H>,
    ) -> Self {
        Self {
            config,
            security,
            handler,
        }
    }

    pub async fn serve_until(
        self,
        shutdown: watch::Receiver<bool>,
    ) -> Result<PipeServerReport, PipeTransportError> {
        self.serve_internal(shutdown, None).await
    }

    pub(crate) async fn serve_until_ready(
        self,
        shutdown: watch::Receiver<bool>,
        ready: SyncSender<Result<(), &'static str>>,
    ) -> Result<PipeServerReport, PipeTransportError> {
        self.serve_internal(shutdown, Some(ready)).await
    }

    async fn serve_internal(
        self,
        mut shutdown: watch::Receiver<bool>,
        mut ready: Option<SyncSender<Result<(), &'static str>>>,
    ) -> Result<PipeServerReport, PipeTransportError> {
        let counters = Arc::new(ServerCounters::default());
        let semaphore = Arc::new(Semaphore::new(self.config.max_concurrent_clients));
        let mut tasks = JoinSet::new();
        let mut waiting =
            match super::secure_named_pipe_server(&self.config.pipe_name, &self.security, true) {
                Ok(waiting) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Ok(()));
                    }
                    waiting
                }
                Err(error) => {
                    if let Some(ready) = ready.take() {
                        let _ = ready.send(Err(error.code()));
                    }
                    return Err(error.into());
                }
            };

        loop {
            reap_finished(&mut tasks);
            if *shutdown.borrow() {
                break;
            }
            let permit = tokio::select! {
                changed = shutdown.changed() => {
                    let _ = changed;
                    break;
                }
                permit = Arc::clone(&semaphore).acquire_owned() => {
                    permit.map_err(|_| PipeTransportError::Shutdown)?
                }
            };
            let connected = tokio::select! {
                changed = shutdown.changed() => {
                    let _ = changed;
                    drop(permit);
                    break;
                }
                result = waiting.connect() => result,
            };
            connected.map_err(PipeTransportError::Io)?;

            let connected_server = waiting;
            waiting =
                super::secure_named_pipe_server(&self.config.pipe_name, &self.security, false)?;
            counters.accepted.fetch_add(1, Ordering::Relaxed);
            let connection_counters = Arc::clone(&counters);
            let handler = Arc::clone(&self.handler);
            let io_timeout = self.config.io_timeout;
            tasks.spawn(async move {
                let _permit = permit;
                let active = ActiveConnection::start(Arc::clone(&connection_counters));
                let result = handle_connection(connected_server, handler, io_timeout).await;
                active.finish(result.is_ok());
                result
            });
        }
        drop(waiting);

        let drain_deadline = self.config.io_timeout.saturating_mul(2);
        let drained = timeout(drain_deadline, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_ok();
        let forced_closed = if drained { 0 } else { tasks.len() };
        if !drained {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }

        Ok(PipeServerReport {
            accepted_connections: counters.accepted.load(Ordering::Relaxed),
            completed_connections: counters.completed.load(Ordering::Relaxed),
            failed_connections: counters.failed.load(Ordering::Relaxed),
            peak_active_connections: counters.peak_active.load(Ordering::Relaxed),
            forced_closed_connections: forced_closed,
        })
    }
}

async fn handle_connection<H>(
    mut server: NamedPipeServer,
    handler: Arc<H>,
    io_timeout: Duration,
) -> Result<(), PipeTransportError>
where
    H: IpcFrameHandler,
{
    let frame = read_request_frame(&mut server, io_timeout).await?;
    let identity = identity_from_connected_pipe(&server)?;
    let response =
        tokio::task::spawn_blocking(move || handler.handle(identity, &frame, unix_time_ms()))
            .await
            .map_err(|_| PipeTransportError::HandlerJoin)??;
    if response.len() > LENGTH_PREFIX_BYTES + MAX_RESPONSE_BODY_BYTES {
        return Err(PipeTransportError::Protocol(IpcError::FrameTooLarge));
    }
    timeout(io_timeout, server.write_all(&response))
        .await
        .map_err(|_| PipeTransportError::Timeout)?
        .map_err(PipeTransportError::Io)?;
    timeout(io_timeout, server.flush())
        .await
        .map_err(|_| PipeTransportError::Timeout)?
        .map_err(PipeTransportError::Io)?;
    timeout(io_timeout, server.shutdown())
        .await
        .map_err(|_| PipeTransportError::Timeout)?
        .map_err(PipeTransportError::Io)
}

async fn read_request_frame(
    server: &mut NamedPipeServer,
    io_timeout: Duration,
) -> Result<Zeroizing<Vec<u8>>, PipeTransportError> {
    let mut prefix = [0_u8; LENGTH_PREFIX_BYTES];
    timeout(io_timeout, server.read_exact(&mut prefix))
        .await
        .map_err(|_| PipeTransportError::Timeout)?
        .map_err(PipeTransportError::Io)?;
    let body_length = u32::from_le_bytes(prefix) as usize;
    if !(FIXED_HEADER_BYTES..=MAX_FRAME_BODY_BYTES).contains(&body_length) {
        return Err(PipeTransportError::Protocol(IpcError::BodyLengthInvalid));
    }
    let mut frame = Zeroizing::new(vec![0_u8; LENGTH_PREFIX_BYTES + body_length]);
    frame[..LENGTH_PREFIX_BYTES].copy_from_slice(&prefix);
    timeout(
        io_timeout,
        server.read_exact(&mut frame[LENGTH_PREFIX_BYTES..]),
    )
    .await
    .map_err(|_| PipeTransportError::Timeout)?
    .map_err(PipeTransportError::Io)?;
    Ok(frame)
}

pub async fn pipe_request(
    pipe_name: &str,
    request_frame: &[u8],
    request_timeout: Duration,
) -> Result<Vec<u8>, PipeTransportError> {
    validate_request_frame(request_frame)?;
    if request_timeout.is_zero() || request_timeout > MAX_IO_TIMEOUT {
        return Err(PipeTransportError::ConfigInvalid);
    }
    let deadline = Instant::now() + request_timeout;
    let mut client = loop {
        match ClientOptions::new().open(pipe_name) {
            Ok(client) => break client,
            Err(error) if retryable_pipe_open_error(&error) && Instant::now() < deadline => {
                sleep(CLIENT_RETRY_INTERVAL).await;
            }
            Err(error) => return Err(PipeTransportError::Io(error)),
        }
    };
    timeout(remaining(deadline)?, client.write_all(request_frame))
        .await
        .map_err(|_| PipeTransportError::Timeout)?
        .map_err(PipeTransportError::Io)?;
    timeout(remaining(deadline)?, client.flush())
        .await
        .map_err(|_| PipeTransportError::Timeout)?
        .map_err(PipeTransportError::Io)?;

    let mut prefix = [0_u8; LENGTH_PREFIX_BYTES];
    timeout(remaining(deadline)?, client.read_exact(&mut prefix))
        .await
        .map_err(|_| PipeTransportError::Timeout)?
        .map_err(PipeTransportError::Io)?;
    let body_length = u32::from_le_bytes(prefix) as usize;
    if body_length > MAX_RESPONSE_BODY_BYTES {
        return Err(PipeTransportError::Protocol(IpcError::FrameTooLarge));
    }
    let mut response = Zeroizing::new(vec![0_u8; LENGTH_PREFIX_BYTES + body_length]);
    response[..LENGTH_PREFIX_BYTES].copy_from_slice(&prefix);
    timeout(
        remaining(deadline)?,
        client.read_exact(&mut response[LENGTH_PREFIX_BYTES..]),
    )
    .await
    .map_err(|_| PipeTransportError::Timeout)?
    .map_err(PipeTransportError::Io)?;
    Ok(response.to_vec())
}

fn retryable_pipe_open_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
    ) || error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32)
}

fn validate_request_frame(frame: &[u8]) -> Result<(), PipeTransportError> {
    if frame.len() < LENGTH_PREFIX_BYTES {
        return Err(PipeTransportError::Protocol(IpcError::FrameTruncated));
    }
    let body_length = u32::from_le_bytes(
        frame[..LENGTH_PREFIX_BYTES]
            .try_into()
            .map_err(|_| PipeTransportError::Protocol(IpcError::FrameTruncated))?,
    ) as usize;
    if !(FIXED_HEADER_BYTES..=MAX_FRAME_BODY_BYTES).contains(&body_length)
        || frame.len() != LENGTH_PREFIX_BYTES + body_length
    {
        return Err(PipeTransportError::Protocol(IpcError::FrameLengthInvalid));
    }
    Ok(())
}

fn remaining(deadline: Instant) -> Result<Duration, PipeTransportError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(PipeTransportError::Timeout)
}

fn reap_finished(tasks: &mut JoinSet<Result<(), PipeTransportError>>) {
    while tasks.try_join_next().is_some() {}
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[derive(Default)]
struct ServerCounters {
    accepted: AtomicUsize,
    completed: AtomicUsize,
    failed: AtomicUsize,
    active: AtomicUsize,
    peak_active: AtomicUsize,
}

struct ActiveConnection {
    counters: Arc<ServerCounters>,
    successful: bool,
}

impl ActiveConnection {
    fn start(counters: Arc<ServerCounters>) -> Self {
        let active = counters.active.fetch_add(1, Ordering::Relaxed) + 1;
        let mut peak = counters.peak_active.load(Ordering::Relaxed);
        while active > peak {
            match counters.peak_active.compare_exchange_weak(
                peak,
                active,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => peak = observed,
            }
        }
        Self {
            counters,
            successful: false,
        }
    }

    fn finish(mut self, successful: bool) {
        self.successful = successful;
    }
}

impl Drop for ActiveConnection {
    fn drop(&mut self) {
        self.counters.active.fetch_sub(1, Ordering::Relaxed);
        if self.successful {
            self.counters.completed.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipeServerReport {
    accepted_connections: usize,
    completed_connections: usize,
    failed_connections: usize,
    peak_active_connections: usize,
    forced_closed_connections: usize,
}

impl PipeServerReport {
    pub const fn accepted_connections(&self) -> usize {
        self.accepted_connections
    }

    pub const fn completed_connections(&self) -> usize {
        self.completed_connections
    }

    pub const fn failed_connections(&self) -> usize {
        self.failed_connections
    }

    pub const fn peak_active_connections(&self) -> usize {
        self.peak_active_connections
    }

    pub const fn forced_closed_connections(&self) -> usize {
        self.forced_closed_connections
    }
}

#[derive(Debug, Error)]
pub enum PipeTransportError {
    #[error("IPC_PIPE_CONFIG_INVALID")]
    ConfigInvalid,
    #[error("{0}")]
    Security(#[from] PipeSecurityError),
    #[error("IPC_PIPE_IO")]
    Io(#[source] std::io::Error),
    #[error("IPC_PIPE_TIMEOUT")]
    Timeout,
    #[error("{0}")]
    Protocol(#[from] IpcError),
    #[error("IPC_PIPE_HANDLER_JOIN_FAILED")]
    HandlerJoin,
    #[error("IPC_PIPE_SHUTDOWN")]
    Shutdown,
}

impl PipeTransportError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid => "IPC_PIPE_CONFIG_INVALID",
            Self::Security(error) => error.code(),
            Self::Io(_) => "IPC_PIPE_IO",
            Self::Timeout => "IPC_PIPE_TIMEOUT",
            Self::Protocol(error) => error.code(),
            Self::HandlerJoin => "IPC_PIPE_HANDLER_JOIN_FAILED",
            Self::Shutdown => "IPC_PIPE_SHUTDOWN",
        }
    }
}
