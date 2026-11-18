//! Bounded native file movement. File capabilities and jobs stay in this process;
//! source values never enter the privileged service or durable journal metadata.
use crate::result_stream::{MemoryReservation, NativeBudget};
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
use restoreplane_domain::workbench::{Column, TypedCell};
use restoreplane_domain::workbench::{Scope, SessionRef};
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read};
use std::io::{Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

const RECORD_BYTES: usize = 256 * 1024;
const FIELD_BYTES: usize = 64 * 1024;
const SOURCE_COLUMNS: usize = 512;
const JSON_DEPTH: usize = 32;
mod imports;
pub use imports::{ImportCoordinator, ImportPreviewInput, ImportPreviewReply, ImportStartError};

/// Created only by the native dialog boundary. Never Deserialize this type or
/// accept its path from the Workbench JSON command payload.
pub struct NativeSourceSelection {
    file: File,
    display_name: String,
    file_identity: String,
    length: u64,
    _parents: Vec<File>,
}
impl NativeSourceSelection {
    pub fn from_native_dialog(path: &Path) -> Result<Self, WorkbenchError> {
        verify_native_path(path)?;
        let parents = hold_native_directories(path.parent().ok_or(WorkbenchError::InvalidInput)?)?;
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options
                .share_mode(1)
                .custom_flags(0x0020_0000 | 0x0800_0000);
        }
        let file = options.open(path).map_err(|_| WorkbenchError::Io)?;
        let metadata = file.metadata().map_err(|_| WorkbenchError::Io)?;
        if !metadata.is_file() || metadata.len() > i64::MAX as u64 {
            return Err(WorkbenchError::Unsupported);
        }
        let identity = restoreplane_source_sqlite::opened_file_object_id(&file)
            .map_err(|_| WorkbenchError::Io)?;
        Ok(Self {
            file,
            display_name: path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or(WorkbenchError::InvalidInput)?
                .into(),
            file_identity: identity,
            length: metadata.len(),
            _parents: parents,
        })
    }
}
/// Holds the selected parent directory against rename/replacement on Windows.
/// A destination is create-new; no implicit overwrite policy exists.
pub struct NativeDestinationSelection {
    path: PathBuf,
    _parents: Vec<File>,
    _claim: Option<(OwnedSemaphorePermit, MemoryReservation)>,
}
impl NativeDestinationSelection {
    pub fn final_path(&self) -> &Path {
        &self.path
    }
    pub fn from_native_dialog(path: &Path) -> Result<Self, WorkbenchError> {
        Self::from_native_path(path, false)
    }
    /// Trusted journal cleanup only; the caller still verifies the opened
    /// partial file's recorded identity before deleting that handle.
    pub(crate) fn from_recorded_partial(path: &Path) -> Result<Self, WorkbenchError> {
        Self::from_native_path(path, true)
    }
    fn from_native_path(path: &Path, existing: bool) -> Result<Self, WorkbenchError> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(WorkbenchError::InvalidInput);
        }
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains(':'))
        {
            return Err(WorkbenchError::Forbidden);
        }
        let parent = path.parent().ok_or(WorkbenchError::InvalidInput)?;
        verify_native_path(parent)?;
        let guards = hold_native_directories(parent)?;
        if !restoreplane_source_sqlite::opened_handle_is_local_ntfs(
            guards.last().ok_or(WorkbenchError::Unsupported)?,
        )
        .map_err(|_| WorkbenchError::Unsupported)?
        {
            return Err(WorkbenchError::Unsupported);
        }
        let parent = std::fs::canonicalize(parent).map_err(|_| WorkbenchError::Io)?;
        #[cfg(windows)]
        {
            // minimal: local drive destinations only — certify remote filesystems
            // separately before allowing atomic completion claims there.
            let raw = parent.to_string_lossy();
            if raw.starts_with(r"\\?\UNC\") || raw.starts_with(r"\\") && !raw.starts_with(r"\\?\") {
                return Err(WorkbenchError::Unsupported);
            }
        }
        #[cfg(not(windows))]
        {
            return Err(WorkbenchError::Unsupported);
        }
        #[cfg(windows)]
        {
            let selected = parent.join(path.file_name().ok_or(WorkbenchError::InvalidInput)?);
            let exists = selected.try_exists().map_err(|_| WorkbenchError::Io)?;
            if !existing && exists {
                return Err(WorkbenchError::IdempotencyConflict);
            }
            if existing {
                use std::os::windows::fs::MetadataExt;
                if !exists {
                    return Err(WorkbenchError::ResultExpired);
                }
                let metadata =
                    std::fs::symlink_metadata(&selected).map_err(|_| WorkbenchError::Io)?;
                if !metadata.is_file() || metadata.file_attributes() & 0x400 != 0 {
                    return Err(WorkbenchError::Forbidden);
                }
            }
            Ok(Self {
                path: selected,
                _parents: guards,
                _claim: None,
            })
        }
    }
}
fn hold_native_directories(path: &Path) -> Result<Vec<File>, WorkbenchError> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        let parts = path.ancestors().collect::<Vec<_>>();
        if parts.len() > 128 {
            return Err(WorkbenchError::Limit);
        }
        let mut guards = Vec::with_capacity(parts.len());
        for part in parts.into_iter().rev() {
            let file = OpenOptions::new()
                .read(true)
                .share_mode(3)
                .custom_flags(0x0200_0000 | 0x0020_0000)
                .open(part)
                .map_err(|_| WorkbenchError::Io)?;
            let metadata = file.metadata().map_err(|_| WorkbenchError::Io)?;
            if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
                return Err(WorkbenchError::Forbidden);
            }
            guards.push(file);
        }
        Ok(guards)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(Vec::new())
    }
}
fn verify_native_path(path: &Path) -> Result<(), WorkbenchError> {
    if !path.is_absolute() || path.components().any(|p| matches!(p, Component::ParentDir)) {
        return Err(WorkbenchError::InvalidInput);
    }
    if path
        .components()
        .any(|part| matches!(part,Component::Normal(name) if name.to_string_lossy().contains(':')))
    {
        return Err(WorkbenchError::Forbidden);
    }
    #[cfg(windows)]
    {
        use std::path::Prefix;
        if path.components().any(|part|matches!(part,Component::Prefix(prefix) if matches!(prefix.kind(),Prefix::DeviceNS(_)|Prefix::Verbatim(_)))){return Err(WorkbenchError::Forbidden)}
    }
    for part in path.ancestors() {
        let metadata = std::fs::symlink_metadata(part).map_err(|_| WorkbenchError::Io)?;
        if metadata.file_type().is_symlink() {
            return Err(WorkbenchError::Forbidden);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(WorkbenchError::Forbidden);
            }
        }
    }
    Ok(())
}
#[derive(Clone, PartialEq, Eq)]
struct FileScope {
    owner: String,
    window: Uuid,
    session: SessionRef,
    scope: Scope,
    revision: String,
}
impl FileScope {
    fn current(
        runtime: &WorkbenchState,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<Self, WorkbenchError> {
        context.principal().ensure_standard()?;
        let current = runtime.get(context, session)?;
        if current.state == restoreplane_domain::workbench::SessionState::Retiring {
            return Err(WorkbenchError::SessionStale);
        }
        Ok(Self {
            owner: context.principal().owner_id().into(),
            window: context.window_instance_id(),
            session,
            scope: current.scope,
            revision: current.descriptor_revision,
        })
    }
}
struct HeldSource {
    binding: FileScope,
    selection: NativeSourceSelection,
    digest: String,
    in_use: bool,
    revoked: Arc<std::sync::atomic::AtomicBool>,
    _permit: OwnedSemaphorePermit,
    _memory: MemoryReservation,
}
struct HeldDestination {
    binding: FileScope,
    selection: NativeDestinationSelection,
    in_use: bool,
    _permit: OwnedSemaphorePermit,
    _memory: MemoryReservation,
}
struct ScopedSource {
    binding: FileScope,
    value: Mutex<Option<HeldSource>>,
    revoked: Arc<std::sync::atomic::AtomicBool>,
}
pub struct FileCapabilities {
    runtime: Arc<WorkbenchState>,
    sources: Mutex<HashMap<Uuid, Arc<ScopedSource>>>,
    destinations: Mutex<HashMap<Uuid, HeldDestination>>,
    capacity: Arc<Semaphore>,
    budget: Arc<NativeBudget>,
}
pub struct SourceFacts {
    pub display_name: String,
    pub byte_length: u64,
    pub source_digest: String,
    pub file_identity: String,
}
impl FileCapabilities {
    pub fn new(runtime: Arc<WorkbenchState>, budget: Arc<NativeBudget>) -> Self {
        Self {
            runtime,
            sources: Mutex::new(HashMap::new()),
            destinations: Mutex::new(HashMap::new()),
            capacity: Arc::new(Semaphore::new(32)),
            budget,
        }
    }
    /// Pick registers a held handle only; full digest/EOF checks belong to preview.
    pub fn register_source(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        selection: NativeSourceSelection,
    ) -> Result<Uuid, WorkbenchError> {
        let binding = FileScope::current(&self.runtime, context, session)?;
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        let memory = self.budget.reserve(128 * 1024)?;
        let id = Uuid::new_v4();
        if FileScope::current(&self.runtime, context, session)? != binding {
            return Err(WorkbenchError::SessionStale);
        }
        let mut sources = self.sources.lock().map_err(|_| WorkbenchError::Io)?;
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        sources.insert(
            id,
            Arc::new(ScopedSource {
                binding: binding.clone(),
                revoked: revoked.clone(),
                value: Mutex::new(Some(HeldSource {
                    binding,
                    selection,
                    digest: String::new(),
                    in_use: false,
                    revoked,
                    _permit: permit,
                    _memory: memory,
                })),
            }),
        );
        Ok(id)
    }
    pub fn register_destination(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        selection: NativeDestinationSelection,
    ) -> Result<Uuid, WorkbenchError> {
        let binding = FileScope::current(&self.runtime, context, session)?;
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| WorkbenchError::Limit)?;
        let memory = self.budget.reserve(128 * 1024)?;
        let mut destinations = self.destinations.lock().map_err(|_| WorkbenchError::Io)?;
        if destinations.len() >= 32 {
            return Err(WorkbenchError::Limit);
        }
        let id = Uuid::new_v4();
        destinations.insert(
            id,
            HeldDestination {
                binding,
                selection,
                in_use: false,
                _permit: permit,
                _memory: memory,
            },
        );
        Ok(id)
    }
    pub fn release(
        &self,
        context: &InvocationContext,
        handle: Uuid,
    ) -> Result<bool, WorkbenchError> {
        context.principal().ensure_standard()?;
        let owner = context.principal().owner_id();
        let window = context.window_instance_id();
        let mut sources = self.sources.lock().map_err(|_| WorkbenchError::Io)?;
        if let Some(value) = sources.get(&handle).cloned() {
            if value.binding.owner != owner || value.binding.window != window {
                return Err(WorkbenchError::Forbidden);
            }
            value
                .revoked
                .store(true, std::sync::atomic::Ordering::SeqCst);
            sources.remove(&handle);
            return Ok(true);
        }
        let mut destinations = self.destinations.lock().map_err(|_| WorkbenchError::Io)?;
        if let Some(value) = destinations.get(&handle) {
            if value.binding.owner != owner || value.binding.window != window {
                return Err(WorkbenchError::Forbidden);
            }
            if value.in_use {
                return Err(WorkbenchError::TargetBusy);
            }
            return Ok(destinations.remove(&handle).is_some());
        }
        Ok(false)
    }
    pub fn source_facts(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        handle: Uuid,
    ) -> Result<SourceFacts, WorkbenchError> {
        let capability = self.source_capability(context, session, handle)?;
        let held = capability.value.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => WorkbenchError::TargetBusy,
            std::sync::TryLockError::Poisoned(_) => WorkbenchError::Io,
        })?;
        let source = held.as_ref().ok_or(WorkbenchError::ResultExpired)?;
        if source.digest.is_empty() {
            return Err(WorkbenchError::AttemptExpired);
        }
        Ok(SourceFacts {
            display_name: source.selection.display_name.clone(),
            byte_length: source.selection.length,
            source_digest: source.digest.clone(),
            file_identity: source.selection.file_identity.clone(),
        })
    }
    pub fn destination_display(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        handle: Uuid,
    ) -> Result<String, WorkbenchError> {
        let binding = FileScope::current(&self.runtime, context, session)?;
        let destinations = self.destinations.lock().map_err(|_| WorkbenchError::Io)?;
        let destination = destinations
            .get(&handle)
            .ok_or(WorkbenchError::ResultExpired)?;
        if destination.binding != binding {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(destination.selection.path.to_string_lossy().into_owned())
    }
    /// Transfers an owner/window/session-bound, create-new destination to one
    /// native job. Its capacity and memory reservation survive until job drop.
    pub fn take_destination(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        handle: Uuid,
    ) -> Result<NativeDestinationSelection, WorkbenchError> {
        let binding = FileScope::current(&self.runtime, context, session)?;
        let mut destinations = self.destinations.lock().map_err(|_| WorkbenchError::Io)?;
        let destination = destinations
            .get(&handle)
            .ok_or(WorkbenchError::ResultExpired)?;
        if destination.binding != binding {
            return Err(WorkbenchError::Forbidden);
        }
        if destination.in_use {
            return Err(WorkbenchError::TargetBusy);
        }
        let held = destinations
            .remove(&handle)
            .ok_or(WorkbenchError::ResultExpired)?;
        let mut selection = held.selection;
        selection._claim = Some((held._permit, held._memory));
        Ok(selection)
    }
    pub fn retire_window(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        context.principal().ensure_standard()?;
        self.sources
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, v| {
                let keep = v.binding.owner != context.principal().owner_id()
                    || v.binding.window != context.window_instance_id();
                if !keep {
                    v.revoked.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                keep
            });
        self.destinations
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, v| {
                v.binding.owner != context.principal().owner_id()
                    || v.binding.window != context.window_instance_id()
                    || v.in_use
            });
        Ok(())
    }
    pub fn retire_session(
        &self,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<(), WorkbenchError> {
        context.principal().ensure_standard()?;
        self.sources
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, value| {
                let keep = value.binding.owner != context.principal().owner_id()
                    || value.binding.window != context.window_instance_id()
                    || value.binding.session != session;
                if !keep {
                    value
                        .revoked
                        .store(true, std::sync::atomic::Ordering::SeqCst)
                }
                keep
            });
        self.destinations
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, value| {
                value.binding.owner != context.principal().owner_id()
                    || value.binding.window != context.window_instance_id()
                    || value.binding.session != session
            });
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceScalar {
    Null,
    Bool(bool),
    Text(String),
    Number(String),
    Json(String),
    Typed(TypedCell),
}
pub struct JsonRecord {
    pub byte_start: u64,
    pub byte_end: u64,
    pub row_index: u64,
    pub names: Option<Vec<String>>,
    pub fields: Vec<SourceScalar>,
}
/// One record at a time; JSON numbers are read as RawValue lexemes, never f64.
/// The caller reserves shared native memory before creating this reader.
pub struct JsonRecordReader<R: Read> {
    input: BufReader<R>,
    position: u64,
    row: u64,
    first: bool,
    finished: bool,
    failed: bool,
    typed: bool,
    columns: Option<Vec<Column>>,
    row_names: Option<Vec<String>>,
    row_width: Option<usize>,
}
impl<R: Read> JsonRecordReader<R> {
    pub fn new(reader: R) -> Result<Self, WorkbenchError> {
        let mut this = Self {
            input: BufReader::with_capacity(16 * 1024, reader),
            position: 0,
            row: 0,
            first: true,
            finished: false,
            failed: false,
            typed: false,
            columns: None,
            row_names: None,
            row_width: None,
        };
        match this.significant()? {
            Some(b'[') => {
                this.byte()?;
            }
            Some(b'{') => {
                this.byte()?;
                this.key("columns")?;
                let raw = this.value()?;
                let columns: Vec<Column> =
                    serde_json::from_slice(&raw).map_err(|_| WorkbenchError::InvalidInput)?;
                if columns.is_empty()
                    || columns.len() > SOURCE_COLUMNS
                    || columns.iter().any(|column| column.name.len() > FIELD_BYTES)
                {
                    return Err(WorkbenchError::Limit);
                }
                this.expect(b',')?;
                this.key("rows")?;
                this.expect(b'[')?;
                this.typed = true;
                this.columns = Some(columns);
            }
            _ => return Err(WorkbenchError::InvalidInput),
        }
        Ok(this)
    }
    pub fn columns(&self) -> Option<&[Column]> {
        self.columns.as_deref()
    }
    pub fn next_record(&mut self) -> Result<Option<JsonRecord>, WorkbenchError> {
        if self.failed {
            return Err(WorkbenchError::InvalidInput);
        }
        let result = self.read_record();
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn read_record(&mut self) -> Result<Option<JsonRecord>, WorkbenchError> {
        if self.finished {
            return Ok(None);
        }
        let next = self.significant()?.ok_or(WorkbenchError::InvalidInput)?;
        if next == b']' {
            self.byte()?;
            if self.typed {
                self.expect(b',')?;
                self.key("complete")?;
                let raw = self.value()?;
                if raw != b"true" {
                    return Err(WorkbenchError::InvalidInput);
                }
                self.expect(b'}')?;
            }
            if self.significant()?.is_some() {
                return Err(WorkbenchError::InvalidInput);
            }
            self.finished = true;
            return Ok(None);
        }
        if !self.first {
            self.expect(b',')?;
            if self.significant()? == Some(b']') {
                return Err(WorkbenchError::InvalidInput);
            }
        }
        self.significant()?;
        let start = self.position;
        let raw = self.value()?;
        let end = self.position;
        let RawRecord {
            mut names,
            mut values,
        } = serde_json::from_slice::<RawRecord>(&raw).map_err(|_| WorkbenchError::InvalidInput)?;
        if values.len() > SOURCE_COLUMNS {
            return Err(WorkbenchError::Limit);
        }
        if values.is_empty() {
            return Err(WorkbenchError::InvalidInput);
        }
        if self.first {
            self.row_names = names.clone();
            self.row_width = Some(values.len());
        } else {
            if self.row_width != Some(values.len()) || self.row_names.is_some() != names.is_some() {
                return Err(WorkbenchError::InvalidInput);
            }
            if let (Some(expected), Some(actual)) = (&self.row_names, &names) {
                let mut mapped = actual
                    .iter()
                    .cloned()
                    .zip(values)
                    .collect::<std::collections::BTreeMap<_, _>>();
                values = expected
                    .iter()
                    .map(|name| mapped.remove(name).ok_or(WorkbenchError::InvalidInput))
                    .collect::<Result<Vec<_>, _>>()?;
                names = Some(expected.clone());
            }
        }
        if self.typed
            && (names.is_some()
                || self
                    .columns
                    .as_ref()
                    .is_none_or(|columns| columns.len() != values.len()))
        {
            return Err(WorkbenchError::InvalidInput);
        }
        let fields = values
            .into_iter()
            .map(|value| source_scalar(&value, self.typed))
            .collect::<Result<Vec<_>, _>>()?;
        let row = self.row;
        self.row = self.row.checked_add(1).ok_or(WorkbenchError::Limit)?;
        self.first = false;
        Ok(Some(JsonRecord {
            byte_start: start,
            byte_end: end,
            row_index: row,
            names,
            fields,
        }))
    }
    pub fn complete(&self) -> bool {
        self.finished
    }
    fn byte(&mut self) -> Result<Option<u8>, WorkbenchError> {
        let value = self
            .input
            .fill_buf()
            .map_err(|_| WorkbenchError::Io)?
            .first()
            .copied();
        if value.is_some() {
            self.input.consume(1);
            self.position = self.position.checked_add(1).ok_or(WorkbenchError::Limit)?;
        }
        Ok(value)
    }
    fn peek(&mut self) -> Result<Option<u8>, WorkbenchError> {
        Ok(self
            .input
            .fill_buf()
            .map_err(|_| WorkbenchError::Io)?
            .first()
            .copied())
    }
    fn significant(&mut self) -> Result<Option<u8>, WorkbenchError> {
        while self.peek()?.is_some_and(|b| b.is_ascii_whitespace()) {
            self.byte()?;
        }
        self.peek()
    }
    fn expect(&mut self, value: u8) -> Result<(), WorkbenchError> {
        self.significant()?;
        if self.byte()? != Some(value) {
            return Err(WorkbenchError::InvalidInput);
        }
        Ok(())
    }
    fn key(&mut self, expected: &str) -> Result<(), WorkbenchError> {
        let raw = self.value()?;
        let key: String = serde_json::from_slice(&raw).map_err(|_| WorkbenchError::InvalidInput)?;
        if key != expected {
            return Err(WorkbenchError::InvalidInput);
        }
        self.expect(b':')
    }
    fn value(&mut self) -> Result<Vec<u8>, WorkbenchError> {
        self.significant()?;
        let first = self.peek()?.ok_or(WorkbenchError::InvalidInput)?;
        let mut bytes = Vec::with_capacity(1024);
        let mut depth = 0usize;
        let mut string = false;
        let mut escape = false;
        loop {
            let Some(byte) = self.peek()? else {
                if depth != 0 || string {
                    return Err(WorkbenchError::InvalidInput);
                }
                break;
            };
            if !string
                && depth == 0
                && !bytes.is_empty()
                && (byte.is_ascii_whitespace() || matches!(byte, b',' | b']' | b'}' | b':'))
            {
                break;
            }
            if bytes.len() >= RECORD_BYTES {
                return Err(WorkbenchError::Limit);
            }
            self.byte()?;
            bytes.push(byte);
            if string {
                if escape {
                    escape = false
                } else if byte == b'\\' {
                    escape = true
                } else if byte == b'"' {
                    string = false;
                    if depth == 0 {
                        break;
                    }
                }
            } else {
                match byte {
                    b'"' => string = true,
                    b'[' | b'{' => {
                        depth += 1;
                        if depth > JSON_DEPTH {
                            return Err(WorkbenchError::Limit);
                        }
                    }
                    b']' | b'}' => {
                        depth = depth.checked_sub(1).ok_or(WorkbenchError::InvalidInput)?;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        if bytes.is_empty() || (!matches!(first, b'[' | b'{' | b'"') && string) {
            return Err(WorkbenchError::InvalidInput);
        }
        // Validate syntax after the lexical depth/size gates, retaining numbers.
        let _: Box<RawValue> =
            serde_json::from_slice(&bytes).map_err(|_| WorkbenchError::InvalidInput)?;
        Ok(bytes)
    }
}
struct RawRecord {
    names: Option<Vec<String>>,
    values: Vec<Box<RawValue>>,
}
impl<'de> Deserialize<'de> for RawRecord {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RecordVisitor;
        impl<'de> Visitor<'de> for RecordVisitor {
            type Value = RawRecord;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("bounded JSON row array or object")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<RawRecord, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = seq.next_element()? {
                    if values.len() >= SOURCE_COLUMNS {
                        return Err(serde::de::Error::custom("column limit"));
                    }
                    values.push(value);
                }
                Ok(RawRecord {
                    names: None,
                    values,
                })
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RawRecord, A::Error> {
                let mut keys = BTreeSet::new();
                let mut names = Vec::new();
                let mut values = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    if names.len() >= SOURCE_COLUMNS
                        || key.len() > FIELD_BYTES
                        || !keys.insert(key.clone())
                    {
                        return Err(serde::de::Error::custom("column or duplicate key limit"));
                    }
                    names.push(key);
                    values.push(map.next_value()?);
                }
                Ok(RawRecord {
                    names: Some(names),
                    values,
                })
            }
        }
        deserializer.deserialize_any(RecordVisitor)
    }
}
fn source_scalar(raw: &RawValue, typed: bool) -> Result<SourceScalar, WorkbenchError> {
    let value = raw.get();
    if typed {
        // Escapes may expand a legal decoded 64KiB cell. The enclosing record
        // keeps its 256KiB cap; decoded cell bounds are checked separately.
        if value.len() > RECORD_BYTES {
            return Err(WorkbenchError::Limit);
        }
        let cell: TypedCell =
            serde_json::from_str(value).map_err(|_| WorkbenchError::InvalidInput)?;
        crate::types::validate_cells(std::slice::from_ref(&cell))?;
        if matches!(
            cell,
            TypedCell::Large { .. } | TypedCell::Unsupported { .. }
        ) {
            return Err(WorkbenchError::Unsupported);
        }
        return Ok(SourceScalar::Typed(cell));
    }
    let scalar = match value.as_bytes().first().copied() {
        Some(b'n') => SourceScalar::Null,
        Some(b't') => SourceScalar::Bool(true),
        Some(b'f') => SourceScalar::Bool(false),
        Some(b'"') => SourceScalar::Text(
            serde_json::from_str(value).map_err(|_| WorkbenchError::InvalidInput)?,
        ),
        Some(b'[' | b'{') => SourceScalar::Json(value.to_owned()),
        Some(b'-' | b'0'..=b'9') => SourceScalar::Number(value.to_owned()),
        _ => return Err(WorkbenchError::InvalidInput),
    };
    let size = match &scalar {
        SourceScalar::Text(value) | SourceScalar::Json(value) | SourceScalar::Number(value) => {
            value.len()
        }
        _ => 0,
    };
    if size > FIELD_BYTES {
        return Err(WorkbenchError::Limit);
    }
    Ok(scalar)
}
