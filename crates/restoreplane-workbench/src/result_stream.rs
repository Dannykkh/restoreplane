//! Volatile bounded delivery. Reattachment never runs a database statement.
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
use restoreplane_domain::workbench::{Column, Engine, SessionRef, SessionState};
use restoreplane_domain::workbench_metadata::{Completeness, Consistency, Count, Page, ResultPage};
use restoreplane_domain::workbench_stream::{
    Ack, AckResult, ResultEnvelope, ResultEvent, ResultNotice, ResultNoticeKind, StreamIdentity,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const MAX_FRAME_BYTES: usize = 256 * 1024;
pub const GLOBAL_CREDIT_BYTES: usize = 8 * 1024 * 1024;
pub const NATIVE_RESULT_BYTES: usize = 64 * 1024 * 1024;
pub const FRONTEND_RETAINED_BYTES: usize = 32 * 1024 * 1024;
pub type ResultEmitter = dyn Fn(&ResultEnvelope) -> Result<(), WorkbenchError> + Send + Sync;
pub type ResultNoticeEmitter = dyn Fn(&ResultNotice) -> Result<(), WorkbenchError> + Send + Sync;
const CONTROL_BYTES: usize = 1024 * 1024;
const MAX_STREAMS: usize = 64;
const MAX_REPLAY_FRAMES: usize = 128;

pub struct NativeBudget {
    used: AtomicUsize,
    data: AtomicUsize,
    pull: AtomicUsize,
    peak: AtomicUsize,
    control: AtomicUsize,
    credit: AtomicUsize,
    changed: tokio::sync::Notify,
}
impl Default for NativeBudget {
    fn default() -> Self {
        Self {
            used: AtomicUsize::new(0),
            data: AtomicUsize::new(0),
            pull: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            control: AtomicUsize::new(0),
            credit: AtomicUsize::new(0),
            changed: tokio::sync::Notify::new(),
        }
    }
}
pub struct MemoryReservation {
    budget: Arc<NativeBudget>,
    bytes: usize,
    class: BudgetClass,
}
#[derive(Clone, Copy)]
enum BudgetClass {
    Data,
    Control,
    Schema,
    Pull,
}
impl std::fmt::Debug for MemoryReservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryReservation")
            .field("bytes", &self.bytes)
            .finish_non_exhaustive()
    }
}
struct CreditReservation {
    budget: Arc<NativeBudget>,
    bytes: usize,
}
impl Drop for CreditReservation {
    fn drop(&mut self) {
        self.budget.credit.fetch_sub(self.bytes, Ordering::SeqCst);
        self.budget.changed.notify_waiters();
    }
}
impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::SeqCst);
        match self.class {
            BudgetClass::Data => &self.budget.data,
            BudgetClass::Control | BudgetClass::Schema => &self.budget.control,
            BudgetClass::Pull => &self.budget.pull,
        }
        .fetch_sub(self.bytes, Ordering::SeqCst);
        self.budget.changed.notify_waiters();
    }
}
fn increment(counter: &AtomicUsize, bytes: usize, maximum: usize) -> Result<(), WorkbenchError> {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
            used.checked_add(bytes).filter(|next| *next <= maximum)
        })
        .map(|_| ())
        .map_err(|_| WorkbenchError::Limit)
}
impl NativeBudget {
    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Result<MemoryReservation, WorkbenchError> {
        self.reserve_kind(bytes, false)
    }
    fn reserve_kind(
        self: &Arc<Self>,
        bytes: usize,
        control: bool,
    ) -> Result<MemoryReservation, WorkbenchError> {
        self.reserve_class(
            bytes,
            if control {
                BudgetClass::Control
            } else {
                BudgetClass::Data
            },
        )
    }
    fn reserve_class(
        self: &Arc<Self>,
        bytes: usize,
        class: BudgetClass,
    ) -> Result<MemoryReservation, WorkbenchError> {
        let (counter, maximum) = match class {
            BudgetClass::Data => (&self.data, 59 * 1024 * 1024),
            BudgetClass::Control => (&self.control, CONTROL_BYTES),
            BudgetClass::Schema => (&self.control, CONTROL_BYTES - 256 * 1024),
            BudgetClass::Pull => (&self.pull, 4 * 1024 * 1024),
        };
        increment(counter, bytes, maximum)?;
        if let Err(error) = increment(&self.used, bytes, NATIVE_RESULT_BYTES) {
            counter.fetch_sub(bytes, Ordering::SeqCst);
            return Err(error);
        }
        self.peak
            .fetch_max(self.used.load(Ordering::SeqCst), Ordering::SeqCst);
        Ok(MemoryReservation {
            budget: self.clone(),
            bytes,
            class,
        })
    }
    pub fn retained_bytes(&self) -> usize {
        self.used.load(Ordering::SeqCst)
    }
    pub fn outstanding_bytes(&self) -> usize {
        self.credit.load(Ordering::SeqCst)
    }
    pub fn peak_bytes(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
    pub fn data_bytes(&self) -> usize {
        self.data.load(Ordering::SeqCst)
    }
    pub fn pull_bytes(&self) -> usize {
        self.pull.load(Ordering::SeqCst)
    }
}
struct Counter {
    bytes: usize,
    maximum: usize,
}
impl Write for Counter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(data.len())
            .filter(|next| *next <= self.maximum)
            .ok_or_else(|| io::Error::other("WB_LIMIT"))?;
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub fn encoded_size<T: Serialize>(value: &T, maximum: usize) -> Result<usize, WorkbenchError> {
    let mut counter = Counter { bytes: 0, maximum };
    serde_json::to_writer(&mut counter, value).map_err(|_| WorkbenchError::Limit)?;
    Ok(counter.bytes)
}
pub struct RetainedFrame {
    envelope: ResultEnvelope,
    encoded: Vec<u8>,
    _memory: MemoryReservation,
}
impl RetainedFrame {
    pub fn envelope(&self) -> &ResultEnvelope {
        &self.envelope
    }
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }
}
impl Serialize for RetainedFrame {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.envelope.serialize(serializer)
    }
}
pub struct RetainedPage {
    page: ResultPage,
    _memory: MemoryReservation,
}
pub struct FramePull {
    streams: Arc<ResultStreams>,
    context: InvocationContext,
    identity: StreamIdentity,
    frame: Option<Arc<RetainedFrame>>,
    has_more: bool,
    _memory: MemoryReservation,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl Serialize for FramePull {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Data<'a> {
            frames: Vec<&'a ResultEnvelope>,
            has_more: bool,
        }
        Data {
            frames: self.frame.iter().map(|frame| &frame.envelope).collect(),
            has_more: self.has_more,
        }
        .serialize(serializer)
    }
}
impl FramePull {
    /// Called by the deferred IPC serializer only after successful bounded JSON
    /// encoding. Merely constructing/dropping a pull is not a sent frame.
    pub fn mark_sent(&self) -> Result<(), WorkbenchError> {
        let Some(frame) = &self.frame else {
            return Ok(());
        };
        let mut streams = self
            .streams
            .streams
            .lock()
            .map_err(|_| WorkbenchError::Io)?;
        let stream = ResultStreams::stream(&mut streams, &self.context, &self.identity)?;
        if stream.expired {
            return Err(WorkbenchError::ResultExpired);
        }
        let sequence = frame.envelope.sequence.value();
        if sequence > stream.sent {
            if sequence
                != stream
                    .sent
                    .checked_add(1)
                    .ok_or(WorkbenchError::AckInvalid)?
            {
                return Err(WorkbenchError::AckInvalid);
            }
            stream.sent = sequence;
        }
        Ok(())
    }
}
impl RetainedPage {
    pub fn page(&self) -> &ResultPage {
        &self.page
    }
}

struct Stream {
    expired: bool,
    owner: String,
    identity: StreamIdentity,
    schema_version: String,
    engine: Engine,
    columns: Vec<Column>,
    consistency: Consistency,
    emitter: Option<Arc<ResultEmitter>>,
    notice_emitter: Option<Arc<ResultNoticeEmitter>>,
    pull_slot: Arc<tokio::sync::Semaphore>,
    next_row: u64,
    schema_sent: bool,
    execution_epoch: u64,
    frames: VecDeque<Arc<RetainedFrame>>,
    terminal: Option<Arc<RetainedFrame>>,
    charges: BTreeMap<u64, CreditReservation>,
    next: u64,
    sent: u64,
    acked: u64,
    last_credit: Instant,
    touched: Instant,
    budget: Arc<NativeBudget>,
    _metadata: MemoryReservation,
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.budget.changed.notify_waiters();
    }
}
pub struct ResultStreams {
    consumer_timeout: Duration,
    runtime: Arc<WorkbenchState>,
    budget: Arc<NativeBudget>,
    streams: Mutex<HashMap<Uuid, Stream>>,
    _inflight: MemoryReservation,
    pull_mode: bool,
}
pub struct StreamSpec {
    pub query_run_id: Uuid,
    pub result_set_id: Uuid,
    pub schema_version: String,
    pub columns: Vec<Column>,
    pub consistency: Consistency,
}
impl ResultStreams {
    pub fn retire_session(
        &self,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<(), WorkbenchError> {
        self.retire_scope(context, Some(session))
    }
    pub fn retire_window(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        self.retire_scope(context, None)
    }
    fn retire_scope(
        &self,
        context: &InvocationContext,
        session: Option<SessionRef>,
    ) -> Result<(), WorkbenchError> {
        if context.principal() != self.runtime.principal() {
            return Err(WorkbenchError::Forbidden);
        }
        self.streams
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, stream| {
                !(stream.owner == context.principal().owner_id()
                    && stream.identity.window_id == context.window_instance_id()
                    && session.is_none_or(|session| {
                        stream.identity.session_id == session.session_id
                            && stream.identity.connection_generation
                                == session.connection_generation
                    }))
            });
        Ok(())
    }
    pub fn new(runtime: Arc<WorkbenchState>) -> Result<Self, WorkbenchError> {
        Self::new_mode(runtime, false)
    }
    pub fn new_pull(runtime: Arc<WorkbenchState>) -> Result<Self, WorkbenchError> {
        Self::new_mode(runtime, true)
    }
    pub fn new_pull_with_timeout(
        runtime: Arc<WorkbenchState>,
        timeout: Duration,
    ) -> Result<Self, WorkbenchError> {
        if timeout.is_zero() || timeout > Duration::from_secs(30) {
            return Err(WorkbenchError::InvalidInput);
        }
        let mut streams = Self::new_pull(runtime)?;
        streams.consumer_timeout = timeout;
        Ok(streams)
    }
    fn new_mode(runtime: Arc<WorkbenchState>, pull_mode: bool) -> Result<Self, WorkbenchError> {
        let budget = Arc::new(NativeBudget::default());
        // Four driver frames/decoded rows, bounded before their protocol decoder.
        let inflight = budget.reserve(8 * 1024 * 1024)?;
        Ok(Self {
            consumer_timeout: Duration::from_secs(30),
            runtime,
            budget,
            streams: Mutex::new(HashMap::new()),
            _inflight: inflight,
            pull_mode,
        })
    }
    pub fn budget(&self) -> Arc<NativeBudget> {
        self.budget.clone()
    }
    pub fn create(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        spec: StreamSpec,
    ) -> Result<StreamIdentity, WorkbenchError> {
        let StreamSpec {
            query_run_id,
            result_set_id,
            schema_version,
            columns,
            consistency,
        } = spec;
        let view = self.runtime.get(context, session)?;
        let engine = self.runtime.engine(context, session)?;
        if view.state == SessionState::Retiring
            || columns.len() > 512
            || schema_version.len() != 64
            || query_run_id.is_nil()
            || result_set_id.is_nil()
        {
            return Err(WorkbenchError::InvalidInput);
        }
        let metadata_size = encoded_size(&columns, MAX_FRAME_BYTES)?
            .checked_mul(3)
            .ok_or(WorkbenchError::Limit)?;
        let metadata = self.budget.reserve(metadata_size + 1024)?;
        let identity = StreamIdentity {
            window_id: context.window_instance_id(),
            workspace_id: view.scope.workspace_id.as_str().into(),
            database_target_id: view.scope.database_target_id.as_str().into(),
            session_id: session.session_id,
            connection_generation: session.connection_generation,
            query_run_id,
            result_set_id,
            stream_generation: 1.into(),
        };
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        streams.retain(|_, stream| stream.touched.elapsed() < Duration::from_secs(120));
        if streams.len() >= MAX_STREAMS {
            let candidate = streams
                .iter()
                .filter(|(_, stream)| {
                    stream.expired
                        || stream.terminal.as_ref().is_some_and(|terminal| {
                            terminal.envelope.sequence.value() <= stream.acked
                        })
                })
                .min_by_key(|(_, stream)| (!stream.expired, stream.touched))
                .map(|(id, _)| *id);
            if let Some(id) = candidate {
                streams.remove(&id);
            }
        }
        if streams.len() >= MAX_STREAMS || streams.contains_key(&result_set_id) {
            return Err(WorkbenchError::Limit);
        }
        streams.insert(
            result_set_id,
            Stream {
                expired: false,
                owner: context.principal().owner_id().into(),
                identity: identity.clone(),
                schema_version,
                engine,
                columns,
                consistency,
                emitter: None,
                notice_emitter: None,
                pull_slot: Arc::new(tokio::sync::Semaphore::new(1)),
                next_row: 0,
                schema_sent: false,
                execution_epoch: view.execution_epoch.value(),
                frames: VecDeque::new(),
                terminal: None,
                charges: BTreeMap::new(),
                next: 1,
                sent: 0,
                acked: 0,
                last_credit: Instant::now(),
                touched: Instant::now(),
                budget: self.budget.clone(),
                _metadata: metadata,
            },
        );
        Ok(identity)
    }
    fn stream<'a>(
        streams: &'a mut HashMap<Uuid, Stream>,
        context: &InvocationContext,
        identity: &StreamIdentity,
    ) -> Result<&'a mut Stream, WorkbenchError> {
        context.principal().ensure_standard()?;
        let stream = streams
            .get_mut(&identity.result_set_id)
            .ok_or(WorkbenchError::ResultExpired)?;
        if stream.owner != context.principal().owner_id()
            || context.window_instance_id() != identity.window_id
            || stream.identity != *identity
        {
            return Err(WorkbenchError::Forbidden);
        }
        if stream.touched.elapsed() > Duration::from_secs(120) {
            return Err(WorkbenchError::ResultExpired);
        }
        stream.touched = Instant::now();
        Ok(stream)
    }
    /// Native provenance remains attached to a valid retained result. Shown
    /// export does not guess an engine from display text or require a new query.
    pub fn source_engine(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
    ) -> Result<Engine, WorkbenchError> {
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        let stream = Self::stream(&mut streams, context, identity)?;
        if stream.expired {
            return Err(WorkbenchError::ResultExpired);
        }
        Ok(stream.engine)
    }
    pub(crate) fn cache_evictable(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
    ) -> Result<bool, WorkbenchError> {
        context.principal().ensure_standard()?;
        let streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        let Some(stream) = streams.get(&identity.result_set_id) else {
            return Ok(true);
        };
        if stream.owner != context.principal().owner_id()
            || context.window_instance_id() != identity.window_id
            || stream.identity != *identity
        {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(stream.expired
            || stream.touched.elapsed() >= Duration::from_secs(120)
            || stream
                .terminal
                .as_ref()
                .is_some_and(|terminal| terminal.envelope.sequence.value() <= stream.acked))
    }
    pub fn source_completeness(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
    ) -> Result<Option<restoreplane_domain::workbench_stream::Completion>, WorkbenchError> {
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        let stream = Self::stream(&mut streams, context, identity)?;
        if stream.expired {
            return Err(WorkbenchError::ResultExpired);
        }
        Ok(stream
            .terminal
            .as_ref()
            .and_then(|terminal| match &terminal.envelope.event {
                ResultEvent::Terminal { completeness, .. } => Some(*completeness),
                _ => None,
            }))
    }
    pub fn try_publish<F>(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        event: ResultEvent,
        emit: &mut F,
    ) -> Result<Arc<RetainedFrame>, WorkbenchError>
    where
        F: FnMut(&ResultEnvelope) -> Result<(), WorkbenchError>,
    {
        validate_event(&event)?;
        let control = !matches!(event, ResultEvent::Rows { .. });
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        if self.budget.retained_bytes() > 48 * 1024 * 1024 {
            for stream in streams.values_mut() {
                while stream
                    .frames
                    .front()
                    .is_some_and(|frame| frame.envelope.sequence.value() <= stream.acked)
                {
                    stream.frames.pop_front();
                }
            }
        }
        let stream = Self::stream(&mut streams, context, identity)?;
        if stream.terminal.is_some()
            || stream.expired
            || (!self.pull_mode && stream.sent + 1 != stream.next)
        {
            return Err(WorkbenchError::ResultExpired);
        }
        match &event {
            ResultEvent::Rows { start_row, rows } => {
                if !stream.schema_sent
                    || start_row.value() != stream.next_row
                    || rows
                        .iter()
                        .any(|row| row.cells.len() != stream.columns.len())
                {
                    return Err(WorkbenchError::InvalidInput);
                }
            }
            ResultEvent::Schema { columns, .. }
                if stream.schema_sent
                    || columns.len() != stream.columns.len()
                    || columns
                        .iter()
                        .zip(&stream.columns)
                        .any(|(a, b)| a.column_id != b.column_id) =>
            {
                return Err(WorkbenchError::InvalidInput);
            }
            _ => {}
        }
        while stream.frames.len() >= MAX_REPLAY_FRAMES {
            if stream
                .frames
                .front()
                .is_some_and(|frame| frame.envelope.sequence.value() <= stream.acked)
            {
                stream.frames.pop_front();
            } else {
                return Err(WorkbenchError::Limit);
            }
        }
        let mut envelope = ResultEnvelope {
            protocol_version: 1,
            identity: identity.clone(),
            sequence: stream.next.into(),
            encoded_bytes: 0,
            event,
        };
        for _ in 0..4 {
            let bytes = encoded_size(&envelope, MAX_FRAME_BYTES - 1024)?;
            if bytes == envelope.encoded_bytes {
                break;
            }
            envelope.encoded_bytes = bytes;
        }
        if encoded_size(&envelope, MAX_FRAME_BYTES - 1024)? != envelope.encoded_bytes {
            return Err(WorkbenchError::Limit);
        }
        let bytes = envelope.encoded_bytes;
        let credit = if !control {
            increment(&self.budget.credit, bytes, GLOBAL_CREDIT_BYTES)?;
            Some(CreditReservation {
                budget: self.budget.clone(),
                bytes,
            })
        } else {
            None
        };
        let decoded = match &envelope.event {
            ResultEvent::Schema { columns, .. } => {
                crate::types::columns_memory(columns, columns.capacity())
            }
            ResultEvent::Rows { rows, .. } => crate::types::rows_memory(rows, rows.capacity()),
            ResultEvent::Terminal { error_code, .. } => {
                error_code.as_ref().map_or(0, String::capacity)
            }
        };
        let reservation = self.budget.reserve_class(
            decoded
                .checked_add(
                    bytes
                        .checked_mul(if self.pull_mode { 1 } else { 4 })
                        .ok_or(WorkbenchError::Limit)?,
                )
                .and_then(|value| value.checked_add(1024))
                .ok_or(WorkbenchError::Limit)?,
            if matches!(&envelope.event, ResultEvent::Schema { .. }) {
                BudgetClass::Schema
            } else if control {
                BudgetClass::Control
            } else {
                BudgetClass::Data
            },
        )?;
        // Count without allocation, reserve decoded+encoded+IPC copies, serialize, enqueue.
        let encoded = serde_json::to_vec(&envelope).map_err(|_| WorkbenchError::Io)?;
        let frame = Arc::new(RetainedFrame {
            envelope,
            encoded,
            _memory: reservation,
        });
        match &frame.envelope.event {
            ResultEvent::Schema { .. } => stream.schema_sent = true,
            ResultEvent::Rows { rows, .. } => {
                stream.next_row = stream
                    .next_row
                    .checked_add(rows.len() as u64)
                    .ok_or(WorkbenchError::Limit)?
            }
            _ => {}
        }
        stream.next = stream.next.checked_add(1).ok_or(WorkbenchError::Limit)?;
        if let Some(credit) = credit {
            stream
                .charges
                .insert(frame.envelope.sequence.value(), credit);
        }
        if matches!(frame.envelope.event, ResultEvent::Terminal { .. }) {
            stream.terminal = Some(frame.clone());
        }
        stream.frames.push_back(frame.clone());
        if self.pull_mode {
            if let Some(emitter) = &stream.notice_emitter {
                let notice = notice(&stream.identity, stream.next - 1);
                encoded_size(&notice, 1023)?;
                emitter(&notice)?;
            }
            return Ok(frame);
        }
        emit(&frame.envelope)?;
        stream.sent = frame.envelope.sequence.value();
        Ok(frame)
    }
    pub async fn publish<F>(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        event: ResultEvent,
        emit: &mut F,
    ) -> Result<Arc<RetainedFrame>, WorkbenchError>
    where
        F: FnMut(&ResultEnvelope) -> Result<(), WorkbenchError> + Send,
    {
        validate_event(&event)?;
        encoded_size(&event, MAX_FRAME_BYTES - 1024)?;
        let deadline = tokio::time::Instant::now() + self.consumer_timeout;
        loop {
            let changed = self.budget.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.try_publish(context, identity, event.clone(), emit) {
                Err(WorkbenchError::Limit) => tokio::time::timeout_at(deadline, changed)
                    .await
                    .map_err(|_| WorkbenchError::ResultExpired)?,
                result => return result,
            }
        }
    }
    pub fn set_emitter(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        emitter: Arc<ResultEmitter>,
    ) -> Result<(), WorkbenchError> {
        if self.pull_mode {
            return Err(WorkbenchError::Unsupported);
        }
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        Self::stream(&mut streams, context, identity)?.emitter = Some(emitter);
        Ok(())
    }
    pub async fn publish_registered(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        event: ResultEvent,
    ) -> Result<Arc<RetainedFrame>, WorkbenchError> {
        if self.pull_mode {
            return self
                .publish(context, identity, event, &mut |_| {
                    Err(WorkbenchError::Unsupported)
                })
                .await;
        }
        let emitter = {
            let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
            Self::stream(&mut streams, context, identity)?
                .emitter
                .clone()
                .ok_or(WorkbenchError::ResultExpired)?
        };
        self.publish(context, identity, event, &mut |envelope| emitter(envelope))
            .await
    }
    pub fn set_notice_emitter(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        emitter: Arc<ResultNoticeEmitter>,
    ) -> Result<(), WorkbenchError> {
        if !self.pull_mode {
            return Err(WorkbenchError::Unsupported);
        }
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        Self::stream(&mut streams, context, identity)?.notice_emitter = Some(emitter);
        Ok(())
    }
    pub fn attach_pull(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        after: Option<u64>,
        emitter: Arc<ResultNoticeEmitter>,
    ) -> Result<Option<Arc<RetainedFrame>>, WorkbenchError> {
        if !self.pull_mode {
            return Err(WorkbenchError::Unsupported);
        }
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        let stream = Self::stream(&mut streams, context, identity)?;
        if stream.expired {
            if after.unwrap_or(0) > stream.sent {
                return Err(WorkbenchError::AckInvalid);
            }
            return Ok(stream.terminal.clone());
        }
        check_after(stream, after.unwrap_or(0))?;
        let notice = notice(identity, stream.next - 1);
        encoded_size(&notice, 1023)?;
        emitter(&notice)?;
        stream.notice_emitter = Some(emitter);
        Ok(stream.terminal.clone())
    }
    pub fn frames(
        self: &Arc<Self>,
        context: &InvocationContext,
        identity: &StreamIdentity,
        after: Option<u64>,
    ) -> Result<FramePull, WorkbenchError> {
        if !self.pull_mode {
            return Err(WorkbenchError::Unsupported);
        }
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        let stream = Self::stream(&mut streams, context, identity)?;
        let after = after.unwrap_or(0);
        check_after(stream, after)?;
        let permit = stream
            .pull_slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        let frame = stream
            .frames
            .iter()
            .find(|frame| frame.envelope.sequence.value() > after)
            .cloned();
        let has_more = frame
            .as_ref()
            .is_some_and(|frame| frame.envelope.sequence.value() + 1 < stream.next);
        let memory = self.budget.reserve_class(
            frame
                .as_ref()
                .map_or(1024, |frame| frame.envelope.encoded_bytes * 2 + 1024),
            BudgetClass::Pull,
        )?;
        Ok(FramePull {
            streams: self.clone(),
            context: context.clone(),
            identity: identity.clone(),
            frame,
            has_more,
            _memory: memory,
            _permit: permit,
        })
    }
    pub fn ack(&self, context: &InvocationContext, ack: &Ack) -> Result<AckResult, WorkbenchError> {
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        let stream = Self::stream(&mut streams, context, &ack.identity)?;
        let high = ack.highest_contiguous_sequence.value();
        if high > stream.sent {
            return Err(WorkbenchError::AckInvalid);
        }
        let mut released = 0usize;
        if high > stream.acked {
            let sequences = stream
                .charges
                .range(..=high)
                .map(|(sequence, _)| *sequence)
                .collect::<Vec<_>>();
            for sequence in sequences {
                released = released
                    .checked_add(
                        stream
                            .charges
                            .remove(&sequence)
                            .ok_or(WorkbenchError::AckInvalid)?
                            .bytes,
                    )
                    .ok_or(WorkbenchError::AckInvalid)?;
            }
            stream.acked = high;
            stream.last_credit = Instant::now();
            self.budget.changed.notify_waiters();
        }
        Ok(AckResult {
            accepted_through: stream.acked.into(),
            released_bytes: (released as u64).into(),
            available_bytes: ((GLOBAL_CREDIT_BYTES - self.budget.outstanding_bytes()) as u64)
                .into(),
        })
    }
    pub fn attach<F>(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        after: Option<u64>,
        emit: &mut F,
    ) -> Result<Option<Arc<RetainedFrame>>, WorkbenchError>
    where
        F: FnMut(&ResultEnvelope) -> Result<(), WorkbenchError>,
    {
        if self.pull_mode {
            return Err(WorkbenchError::Unsupported);
        }
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        let stream = Self::stream(&mut streams, context, identity)?;
        let after = after.unwrap_or(0);
        if after > stream.sent {
            return Err(WorkbenchError::AckInvalid);
        }
        if stream
            .frames
            .front()
            .is_some_and(|frame| frame.envelope.sequence.value() > after.saturating_add(1))
        {
            return Err(WorkbenchError::ResultExpired);
        }
        for frame in &stream.frames {
            let sequence = frame.envelope.sequence.value();
            if sequence > after {
                emit(&frame.envelope)?;
                if sequence == stream.sent + 1 {
                    stream.sent = sequence;
                }
            }
        }
        Ok(stream.terminal.clone())
    }
    pub fn page(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
        start: u64,
        limit: usize,
    ) -> Result<RetainedPage, WorkbenchError> {
        if limit == 0 || limit > 200 {
            return Err(WorkbenchError::InvalidInput);
        }
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        let stream = Self::stream(&mut streams, context, identity)?;
        let end = start
            .checked_add(limit as u64)
            .ok_or(WorkbenchError::Limit)?;
        let mut selected = Vec::new();
        let mut expected = start;
        for frame in &stream.frames {
            if let ResultEvent::Rows { start_row, rows } = &frame.envelope.event {
                for (index, row) in rows.iter().enumerate() {
                    let ordinal = start_row.value() + index as u64;
                    if ordinal >= start && ordinal < end {
                        if ordinal != expected {
                            return Err(WorkbenchError::ResultExpired);
                        }
                        expected += 1;
                        selected.push(row);
                    }
                }
            }
        }
        if selected.is_empty()
            && !(start == 0 && stream.next_row == 0 && stream.schema_sent && !stream.expired)
        {
            return Err(WorkbenchError::ResultExpired);
        }
        let charge = encoded_size(&selected, 512 * 1024)?
            .checked_mul(3)
            .ok_or(WorkbenchError::Limit)?;
        let memory = self
            .budget
            .reserve(charge + encoded_size(&stream.columns, MAX_FRAME_BYTES)? * 2)?;
        let page = ResultPage {
            page: Page {
                items: selected.into_iter().cloned().collect(),
                next_cursor: None,
                completeness: if stream.terminal.as_ref().is_some_and(|terminal| {
                    matches!(
                        &terminal.envelope.event,
                        ResultEvent::Terminal {
                            completeness:
                                restoreplane_domain::workbench_stream::Completion::Complete,
                            ..
                        }
                    )
                }) && expected >= stream.next_row
                {
                    Completeness::Complete
                } else {
                    Completeness::Limited
                },
                count: Count::Unknown,
                as_of_ms: crate::types::now_ms().into(),
            },
            result_set_id: identity.result_set_id,
            columns: stream.columns.clone(),
            consistency: stream.consistency.clone(),
            schema_version: stream.schema_version.clone(),
        };
        encoded_size(&page, 512 * 1024)?;
        Ok(RetainedPage {
            page,
            _memory: memory,
        })
    }
    pub fn retire(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
    ) -> Result<(), WorkbenchError> {
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        Self::stream(&mut streams, context, identity)?;
        streams.remove(&identity.result_set_id);
        Ok(())
    }
    pub fn is_retained(
        &self,
        context: &InvocationContext,
        identity: &StreamIdentity,
    ) -> Result<bool, WorkbenchError> {
        let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
        Ok(!Self::stream(&mut streams, context, identity)?.expired)
    }
    /// The coordinator calls this periodically. An old result can request
    /// cancellation only for its original physical execution epoch.
    pub async fn expire_unacknowledged(&self) -> Result<usize, WorkbenchError> {
        let expired = {
            let mut streams = self.streams.lock().map_err(|_| WorkbenchError::Io)?;
            let mut expired = Vec::new();
            for stream in streams.values_mut() {
                if !stream.charges.is_empty()
                    && stream.last_credit.elapsed() > self.consumer_timeout
                {
                    expired.push((stream.identity.clone(), stream.execution_epoch));
                    if stream.terminal.is_none() {
                        let mut envelope = ResultEnvelope {
                            protocol_version: 1,
                            identity: stream.identity.clone(),
                            sequence: stream.next.into(),
                            encoded_bytes: 0,
                            event: ResultEvent::Terminal {
                                completeness:
                                    restoreplane_domain::workbench_stream::Completion::Interrupted,
                                rows_seen: stream.next_row.into(),
                                operation_version: 0.into(),
                                error_code: Some("WB_RESULT_EXPIRED".into()),
                            },
                        };
                        for _ in 0..4 {
                            let bytes = encoded_size(&envelope, 4096)?;
                            if bytes == envelope.encoded_bytes {
                                break;
                            }
                            envelope.encoded_bytes = bytes;
                        }
                        let reservation = self.budget.reserve_class(
                            envelope.encoded_bytes * 2 + 1024,
                            BudgetClass::Control,
                        )?;
                        let encoded =
                            serde_json::to_vec(&envelope).map_err(|_| WorkbenchError::Io)?;
                        stream.terminal = Some(Arc::new(RetainedFrame {
                            envelope,
                            encoded,
                            _memory: reservation,
                        }));
                        // Status only: never advance sent, ACK or row sequencing.
                    }
                    stream.charges.clear();
                    stream.frames.clear();
                    stream.expired = true;
                }
            }
            expired
        };
        for (identity, epoch) in &expired {
            let context =
                InvocationContext::new(self.runtime.principal().clone(), identity.window_id);
            let reference = SessionRef {
                session_id: identity.session_id,
                connection_generation: identity.connection_generation,
            };
            let _ = self
                .runtime
                .request_cancel(&context, reference, *epoch)
                .await;
        }
        Ok(expired.len())
    }
}
fn notice(identity: &StreamIdentity, available: u64) -> ResultNotice {
    ResultNotice {
        protocol_version: 1,
        kind: ResultNoticeKind::FramesAvailable,
        identity: identity.clone(),
        available_through_sequence: available.into(),
    }
}
fn check_after(stream: &Stream, after: u64) -> Result<(), WorkbenchError> {
    if stream.expired {
        return Err(WorkbenchError::ResultExpired);
    }
    if after > stream.sent {
        return Err(WorkbenchError::AckInvalid);
    }
    if stream
        .frames
        .front()
        .is_some_and(|frame| frame.envelope.sequence.value() > after.saturating_add(1))
    {
        return Err(WorkbenchError::ResultExpired);
    }
    Ok(())
}
fn validate_event(event: &ResultEvent) -> Result<(), WorkbenchError> {
    match event {
        ResultEvent::Schema { columns, .. } if columns.len() > 512 => Err(WorkbenchError::Limit),
        ResultEvent::Rows { rows, .. } => {
            if rows.len() > 200 {
                return Err(WorkbenchError::Limit);
            }
            for row in rows {
                crate::types::validate_cells(&row.cells)?;
            }
            Ok(())
        }
        ResultEvent::Terminal {
            error_code: Some(code),
            ..
        } if code.len() > 128
            || !code
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_') =>
        {
            Err(WorkbenchError::InvalidInput)
        }
        _ => Ok(()),
    }
}
