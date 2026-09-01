use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use uuid::Uuid;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetHandleInformation, SetLastError, BOOL, ERROR_ALREADY_EXISTS,
    FILETIME, HANDLE, HANDLE_FLAGS, HANDLE_FLAG_INHERIT, WAIT_OBJECT_0, WAIT_TIMEOUT, WIN32_ERROR,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    GetFinalPathNameByHandleW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_NAME_NORMALIZED, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess, GetProcessTimes,
    InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
};

use crate::capture::{capture_reader, CapturedStream};
use crate::spec::{
    hash_reader, path_identity, reject_reparse_path, LaunchSpec, SupervisorError,
    MAX_TRUSTED_DEPENDENCY_PATH_UNITS,
};

const TERMINATED_EXIT_CODE: u32 = 0xDBA0_0001;
const MAX_WINDOWS_COMMAND_LINE_UNITS: usize = 32_767;
const MAX_WINDOWS_ENVIRONMENT_UNITS: usize = 32_767;
const WINDOWS_TO_UNIX_EPOCH_100NS: u64 = 116_444_736_000_000_000;
const JOB_TREE_REAP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct ProcessSupervisor;

impl ProcessSupervisor {
    pub fn launch(spec: &LaunchSpec) -> Result<SupervisedProcess, SupervisorError> {
        let launched = launch_process(spec)?;
        let LaunchedProcess {
            process,
            job,
            process_id,
            start_time_100ns,
            job_key,
            stdout,
            stderr,
            dependency_locks,
        } = launched;
        let stdout = match spawn_capture_thread("restoreplane-stdout-drain", stdout, spec) {
            Ok(stdout) => stdout,
            Err(error) => {
                cleanup_failed_capture_launch(&job, dependency_locks);
                return Err(error);
            }
        };
        let stderr = match spawn_capture_thread("restoreplane-stderr-drain", stderr, spec) {
            Ok(stderr) => stderr,
            Err(error) => {
                cleanup_failed_capture_launch(&job, dependency_locks);
                let _ = stdout.join();
                return Err(error);
            }
        };

        Ok(SupervisedProcess {
            process,
            job,
            process_id,
            start_time_100ns,
            job_key,
            output: Some(OutputDrains { stdout, stderr }),
            completed: None,
            dependency_locks: Some(dependency_locks),
        })
    }

    pub fn launch_streaming(
        spec: &LaunchSpec,
    ) -> Result<SupervisedStreamingProcess, SupervisorError> {
        let launched = launch_process(spec)?;
        Ok(SupervisedStreamingProcess {
            process: launched.process,
            job: launched.job,
            process_id: launched.process_id,
            start_time_100ns: launched.start_time_100ns,
            job_key: launched.job_key,
            executable_path: spec.executable.clone(),
            executable_sha256: spec.expected_sha256,
            stdout: Some(launched.stdout),
            stderr: Some(launched.stderr),
            completed: None,
            dependency_locks: Some(launched.dependency_locks),
        })
    }
}

struct LaunchedProcess {
    process: OwnedHandle,
    job: OwnedHandle,
    process_id: u32,
    start_time_100ns: u64,
    job_key: String,
    stdout: File,
    stderr: File,
    dependency_locks: DependencyLocks,
}

fn launch_process(spec: &LaunchSpec) -> Result<LaunchedProcess, SupervisorError> {
    spec.validate()?;
    let mut executable_lock = open_locked_executable(spec)?;
    if hash_reader(&mut executable_lock)? != spec.expected_sha256 {
        return Err(SupervisorError::ExecutableNotTrusted);
    }
    let dependency_locks = open_locked_dependencies(spec)?;

    let job_key = format!("Local\\RestorePlane.Job.{}", Uuid::new_v4());
    let job = create_kill_on_close_job(&job_key)?;
    let pipes = ChildPipes::new()?;
    let inherited_handles = pipes.inherited_handles();
    let attributes = ProcThreadAttributeList::new(&inherited_handles)?;
    let mut command_line = encode_command_line(spec)?;
    let environment = encode_environment(spec)?;
    let executable = encode_null_terminated(spec.executable.as_os_str())?;
    let working_directory = encode_null_terminated(spec.working_directory.as_os_str())?;
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: size_of::<STARTUPINFOEXW>() as u32,
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: pipes.stdin_child.raw(),
            hStdOutput: pipes.stdout_child.raw(),
            hStdError: pipes.stderr_child.raw(),
            ..Default::default()
        },
        lpAttributeList: attributes.raw(),
    };
    let mut process_information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(executable.as_ptr()),
            PWSTR(command_line.as_mut_ptr()),
            None,
            None,
            BOOL(1),
            CREATE_SUSPENDED
                | CREATE_NO_WINDOW
                | CREATE_UNICODE_ENVIRONMENT
                | EXTENDED_STARTUPINFO_PRESENT,
            Some(environment.as_ptr().cast()),
            PCWSTR(working_directory.as_ptr()),
            (&raw const startup).cast(),
            &mut process_information,
        )
    }
    .map_err(|_| SupervisorError::CreateFailed)?;
    let process = OwnedHandle::new(process_information.hProcess);
    let thread = OwnedHandle::new(process_information.hThread);
    drop(attributes);
    let ChildPipes {
        stdin_child,
        stdin_parent,
        stdout_child,
        stdout_parent,
        stderr_child,
        stderr_parent,
    } = pipes;
    drop(stdin_child);
    drop(stdin_parent);
    drop(stdout_child);
    drop(stderr_child);

    if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) }.is_err() {
        terminate_root_process(&process);
        return Err(SupervisorError::JobFailed);
    }
    let start_time_100ns = process_start_time_100ns(process.raw())?;
    if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
        terminate_root_process(&process);
        return Err(SupervisorError::ResumeFailed);
    }
    drop(thread);
    drop(executable_lock);

    Ok(LaunchedProcess {
        process,
        job,
        process_id: process_information.dwProcessId,
        start_time_100ns,
        job_key,
        stdout: stdout_parent.into_file(),
        stderr: stderr_parent.into_file(),
        dependency_locks,
    })
}

pub struct SupervisedStreamingProcess {
    process: OwnedHandle,
    job: OwnedHandle,
    process_id: u32,
    start_time_100ns: u64,
    job_key: String,
    executable_path: std::path::PathBuf,
    executable_sha256: [u8; 32],
    stdout: Option<File>,
    stderr: Option<File>,
    completed: Option<StreamingProcessExit>,
    dependency_locks: Option<DependencyLocks>,
}

impl SupervisedStreamingProcess {
    pub const fn id(&self) -> u32 {
        self.process_id
    }

    pub const fn start_time_100ns(&self) -> u64 {
        self.start_time_100ns
    }

    pub fn start_time_ms(&self) -> Result<i64, SupervisorError> {
        let unix_100ns = self
            .start_time_100ns
            .checked_sub(WINDOWS_TO_UNIX_EPOCH_100NS)
            .ok_or(SupervisorError::WaitFailed)?;
        i64::try_from(unix_100ns / 10_000).map_err(|_| SupervisorError::WaitFailed)
    }

    pub fn job_key(&self) -> &str {
        &self.job_key
    }

    pub fn executable_path(&self) -> &std::path::Path {
        &self.executable_path
    }

    pub const fn executable_sha256(&self) -> &[u8; 32] {
        &self.executable_sha256
    }

    pub fn take_stdout(&mut self) -> Result<File, SupervisorError> {
        self.stdout.take().ok_or(SupervisorError::OutputDrainFailed)
    }

    pub fn take_stderr(&mut self) -> Result<File, SupervisorError> {
        self.stderr.take().ok_or(SupervisorError::OutputDrainFailed)
    }

    pub fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<StreamingProcessExit>, SupervisorError> {
        if let Some(completed) = &self.completed {
            return Ok(Some(completed.clone()));
        }
        let timeout_ms = timeout.as_millis().min((u32::MAX - 1) as u128) as u32;
        match unsafe { WaitForSingleObject(self.process.raw(), timeout_ms) } {
            WAIT_OBJECT_0 => self.finish().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(SupervisorError::WaitFailed),
        }
    }

    pub fn terminate(&mut self) -> Result<StreamingProcessExit, SupervisorError> {
        if let Some(completed) = &self.completed {
            return Ok(completed.clone());
        }
        terminate_job_tree(&self.job)?;
        match unsafe { WaitForSingleObject(self.process.raw(), u32::MAX) } {
            WAIT_OBJECT_0 => self.finish(),
            _ => Err(SupervisorError::WaitFailed),
        }
    }

    fn finish(&mut self) -> Result<StreamingProcessExit, SupervisorError> {
        terminate_job_tree(&self.job)?;
        let mut exit_code = 0_u32;
        unsafe { GetExitCodeProcess(self.process.raw(), &mut exit_code) }
            .map_err(|_| SupervisorError::WaitFailed)?;
        let exit = StreamingProcessExit {
            process_id: self.process_id,
            start_time_100ns: self.start_time_100ns,
            exit_code,
        };
        self.completed = Some(exit.clone());
        Ok(exit)
    }
}

impl Drop for SupervisedStreamingProcess {
    fn drop(&mut self) {
        if self.completed.is_some() {
            return;
        }
        let job_tree_reaped = terminate_job_tree(&self.job).is_ok();
        if job_tree_reaped
            && unsafe { WaitForSingleObject(self.process.raw(), 5_000) } == WAIT_OBJECT_0
        {
            let _ = self.finish();
        }
        if !job_tree_reaped {
            if let Some(locks) = self.dependency_locks.take() {
                std::mem::forget(locks);
            }
        }
    }
}

impl fmt::Debug for SupervisedStreamingProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupervisedStreamingProcess")
            .field("process_id", &self.process_id)
            .field("start_time_100ns", &self.start_time_100ns)
            .field("job_key", &self.job_key)
            .field("executable_path", &self.executable_path)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamingProcessExit {
    process_id: u32,
    start_time_100ns: u64,
    exit_code: u32,
}

impl StreamingProcessExit {
    pub const fn process_id(&self) -> u32 {
        self.process_id
    }

    pub const fn start_time_100ns(&self) -> u64 {
        self.start_time_100ns
    }

    pub const fn exit_code(&self) -> u32 {
        self.exit_code
    }
}

pub struct SupervisedProcess {
    process: OwnedHandle,
    job: OwnedHandle,
    process_id: u32,
    start_time_100ns: u64,
    job_key: String,
    output: Option<OutputDrains>,
    completed: Option<ProcessExit>,
    dependency_locks: Option<DependencyLocks>,
}

impl SupervisedProcess {
    pub const fn id(&self) -> u32 {
        self.process_id
    }

    pub const fn start_time_100ns(&self) -> u64 {
        self.start_time_100ns
    }

    pub fn job_key(&self) -> &str {
        &self.job_key
    }

    pub fn wait_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ProcessExit>, SupervisorError> {
        if let Some(completed) = &self.completed {
            return Ok(Some(completed.clone()));
        }
        let timeout_ms = timeout.as_millis().min((u32::MAX - 1) as u128) as u32;
        match unsafe { WaitForSingleObject(self.process.raw(), timeout_ms) } {
            WAIT_OBJECT_0 => self.finish().map(Some),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(SupervisorError::WaitFailed),
        }
    }

    pub fn terminate(&mut self) -> Result<ProcessExit, SupervisorError> {
        if let Some(completed) = &self.completed {
            return Ok(completed.clone());
        }
        terminate_job_tree(&self.job)?;
        match unsafe { WaitForSingleObject(self.process.raw(), u32::MAX) } {
            WAIT_OBJECT_0 => self.finish(),
            _ => Err(SupervisorError::WaitFailed),
        }
    }

    fn finish(&mut self) -> Result<ProcessExit, SupervisorError> {
        terminate_job_tree(&self.job)?;
        let mut exit_code = 0_u32;
        unsafe { GetExitCodeProcess(self.process.raw(), &mut exit_code) }
            .map_err(|_| SupervisorError::WaitFailed)?;
        let output = self
            .output
            .take()
            .ok_or(SupervisorError::OutputDrainFailed)?;
        let (stdout, stderr) = output.join()?;
        let exit = ProcessExit {
            process_id: self.process_id,
            start_time_100ns: self.start_time_100ns,
            exit_code,
            stdout,
            stderr,
        };
        self.completed = Some(exit.clone());
        Ok(exit)
    }
}

impl Drop for SupervisedProcess {
    fn drop(&mut self) {
        if self.completed.is_some() {
            return;
        }
        let job_tree_reaped = terminate_job_tree(&self.job).is_ok();
        if job_tree_reaped
            && unsafe { WaitForSingleObject(self.process.raw(), 5_000) } == WAIT_OBJECT_0
        {
            let _ = self.finish();
        }
        if !job_tree_reaped {
            if let Some(locks) = self.dependency_locks.take() {
                std::mem::forget(locks);
            }
        }
    }
}

impl fmt::Debug for SupervisedProcess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupervisedProcess")
            .field("process_id", &self.process_id)
            .field("start_time_100ns", &self.start_time_100ns)
            .field("job_key", &self.job_key)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct ProcessExit {
    process_id: u32,
    start_time_100ns: u64,
    exit_code: u32,
    stdout: CapturedStream,
    stderr: CapturedStream,
}

impl ProcessExit {
    pub const fn process_id(&self) -> u32 {
        self.process_id
    }

    pub const fn start_time_100ns(&self) -> u64 {
        self.start_time_100ns
    }

    pub const fn exit_code(&self) -> u32 {
        self.exit_code
    }

    pub const fn stdout(&self) -> &CapturedStream {
        &self.stdout
    }

    pub const fn stderr(&self) -> &CapturedStream {
        &self.stderr
    }
}

impl fmt::Debug for ProcessExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProcessExit")
            .field("process_id", &self.process_id)
            .field("start_time_100ns", &self.start_time_100ns)
            .field("exit_code", &self.exit_code)
            .field("stdout", &self.stdout)
            .field("stderr", &self.stderr)
            .finish()
    }
}

struct OutputDrains {
    stdout: JoinHandle<io::Result<CapturedStream>>,
    stderr: JoinHandle<io::Result<CapturedStream>>,
}

impl OutputDrains {
    fn join(self) -> Result<(CapturedStream, CapturedStream), SupervisorError> {
        let stdout = join_capture(self.stdout);
        let stderr = join_capture(self.stderr);
        match (stdout, stderr) {
            (Ok(stdout), Ok(stderr)) => Ok((stdout, stderr)),
            (Err(error), _) | (_, Err(error)) => Err(error),
        }
    }
}

fn spawn_capture_thread(
    name: &str,
    reader: File,
    spec: &LaunchSpec,
) -> Result<JoinHandle<io::Result<CapturedStream>>, SupervisorError> {
    let tail_capacity = spec.output_tail_bytes;
    let literals = spec.redacted_output_literals.clone();
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || capture_reader(reader, tail_capacity, literals))
        .map_err(SupervisorError::Io)
}

fn join_capture(
    capture: JoinHandle<io::Result<CapturedStream>>,
) -> Result<CapturedStream, SupervisorError> {
    capture
        .join()
        .map_err(|_| SupervisorError::OutputDrainFailed)?
        .map_err(SupervisorError::Io)
}

struct ChildPipes {
    stdin_child: OwnedHandle,
    stdin_parent: OwnedHandle,
    stdout_child: OwnedHandle,
    stdout_parent: OwnedHandle,
    stderr_child: OwnedHandle,
    stderr_parent: OwnedHandle,
}

impl ChildPipes {
    fn new() -> Result<Self, SupervisorError> {
        let (stdin_child, stdin_parent) = create_inheritable_pipe()?;
        make_non_inheritable(&stdin_parent)?;
        let (stdout_parent, stdout_child) = create_inheritable_pipe()?;
        make_non_inheritable(&stdout_parent)?;
        let (stderr_parent, stderr_child) = create_inheritable_pipe()?;
        make_non_inheritable(&stderr_parent)?;
        Ok(Self {
            stdin_child,
            stdin_parent,
            stdout_child,
            stdout_parent,
            stderr_child,
            stderr_parent,
        })
    }

    fn inherited_handles(&self) -> [HANDLE; 3] {
        [
            self.stdin_child.raw(),
            self.stdout_child.raw(),
            self.stderr_child.raw(),
        ]
    }
}

fn create_inheritable_pipe() -> Result<(OwnedHandle, OwnedHandle), SupervisorError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: BOOL(1),
    };
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe {
        CreatePipe(
            &mut read,
            &mut write,
            Some(&raw const attributes),
            64 * 1024,
        )
    }
    .map_err(|_| SupervisorError::PipeFailed)?;
    Ok((OwnedHandle::new(read), OwnedHandle::new(write)))
}

fn make_non_inheritable(handle: &OwnedHandle) -> Result<(), SupervisorError> {
    unsafe { SetHandleInformation(handle.raw(), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        .map_err(|_| SupervisorError::InheritanceFailed)
}

struct ProcThreadAttributeList {
    _storage: Vec<usize>,
    raw: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl ProcThreadAttributeList {
    fn new(handles: &[HANDLE]) -> Result<Self, SupervisorError> {
        let mut required_bytes = 0_usize;
        let _ = unsafe {
            InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST(std::ptr::null_mut()),
                1,
                0,
                &mut required_bytes,
            )
        };
        if required_bytes == 0 {
            return Err(SupervisorError::InheritanceFailed);
        }
        let words = required_bytes.div_ceil(size_of::<usize>());
        let mut storage = vec![0_usize; words];
        let raw = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        unsafe { InitializeProcThreadAttributeList(raw, 1, 0, &mut required_bytes) }
            .map_err(|_| SupervisorError::InheritanceFailed)?;
        if unsafe {
            UpdateProcThreadAttribute(
                raw,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                std::mem::size_of_val(handles),
                None,
                None,
            )
        }
        .is_err()
        {
            unsafe { DeleteProcThreadAttributeList(raw) };
            return Err(SupervisorError::InheritanceFailed);
        }
        Ok(Self {
            _storage: storage,
            raw,
        })
    }

    const fn raw(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.raw
    }
}

impl Drop for ProcThreadAttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.raw) };
    }
}

fn terminate_root_process(process: &OwnedHandle) {
    let _ = unsafe { TerminateProcess(process.raw(), TERMINATED_EXIT_CODE) };
    let _ = unsafe { WaitForSingleObject(process.raw(), 5_000) };
}

fn cleanup_failed_capture_launch(job: &OwnedHandle, dependency_locks: DependencyLocks) {
    if terminate_job_tree(job).is_err() {
        std::mem::forget(dependency_locks);
    }
}

fn terminate_job_tree(job: &OwnedHandle) -> Result<(), SupervisorError> {
    if active_job_processes(job)? == 0 {
        return Ok(());
    }
    unsafe { TerminateJobObject(job.raw(), TERMINATED_EXIT_CODE) }
        .map_err(|_| SupervisorError::JobFailed)?;
    let deadline = Instant::now() + JOB_TREE_REAP_TIMEOUT;
    loop {
        if active_job_processes(job)? == 0 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(SupervisorError::JobFailed);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn active_job_processes(job: &OwnedHandle) -> Result<u32, SupervisorError> {
    let mut accounting = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    unsafe {
        QueryInformationJobObject(
            job.raw(),
            JobObjectBasicAccountingInformation,
            (&raw mut accounting).cast(),
            size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            None,
        )
    }
    .map_err(|_| SupervisorError::JobFailed)?;
    Ok(accounting.ActiveProcesses)
}

fn open_locked_executable(spec: &LaunchSpec) -> Result<File, SupervisorError> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(&spec.executable)
        .map_err(SupervisorError::Io)
}

struct DependencyLocks {
    _files: Vec<File>,
    _directories: Vec<File>,
}

fn open_locked_dependencies(spec: &LaunchSpec) -> Result<DependencyLocks, SupervisorError> {
    let mut files = Vec::with_capacity(spec.trusted_dependencies.len());
    let mut directories = Vec::new();
    let mut locked_directory_paths = BTreeSet::new();
    for dependency in &spec.trusted_dependencies {
        reject_reparse_path(dependency.path())?;
        lock_dependency_ancestors(
            dependency.path(),
            &mut locked_directory_paths,
            &mut directories,
        )?;
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(dependency.path())
            .map_err(SupervisorError::Io)?;
        let metadata = file.metadata().map_err(SupervisorError::Io)?;
        if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
            return Err(SupervisorError::SpecInvalid);
        }
        verify_handle_path(&file, dependency.path())?;
        if hash_reader(&mut file)? != *dependency.expected_sha256() {
            return Err(SupervisorError::DependencyNotTrusted);
        }
        files.push(file);
    }
    Ok(DependencyLocks {
        _files: files,
        _directories: directories,
    })
}

fn lock_dependency_ancestors(
    dependency: &std::path::Path,
    locked_paths: &mut BTreeSet<String>,
    locks: &mut Vec<File>,
) -> Result<(), SupervisorError> {
    let mut ancestors: Vec<_> = dependency.ancestors().skip(1).collect();
    ancestors.reverse();
    for directory in ancestors {
        let identity = path_identity(directory);
        if locked_paths.contains(&identity) {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE).0)
            .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
            .open(directory)
            .map_err(SupervisorError::Io)?;
        let metadata = file.metadata().map_err(SupervisorError::Io)?;
        if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
            return Err(SupervisorError::SpecInvalid);
        }
        verify_handle_path(&file, directory)?;
        locked_paths.insert(identity);
        locks.push(file);
    }
    Ok(())
}

fn verify_handle_path(file: &File, expected: &std::path::Path) -> Result<(), SupervisorError> {
    let mut buffer = vec![0_u16; MAX_TRUSTED_DEPENDENCY_PATH_UNITS + 1];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            HANDLE(file.as_raw_handle()),
            &mut buffer,
            FILE_NAME_NORMALIZED,
        )
    } as usize;
    if length == 0 {
        return Err(SupervisorError::Io(io::Error::last_os_error()));
    }
    if length >= buffer.len() {
        return Err(SupervisorError::SpecInvalid);
    }
    let actual = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
    if path_identity(&actual) != path_identity(expected) {
        return Err(SupervisorError::SpecInvalid);
    }
    Ok(())
}

fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn create_kill_on_close_job(job_key: &str) -> Result<OwnedHandle, SupervisorError> {
    let encoded_name = encode_null_terminated(OsStr::new(job_key))?;
    unsafe { SetLastError(WIN32_ERROR(0)) };
    let job = unsafe { CreateJobObjectW(None, PCWSTR(encoded_name.as_ptr())) }
        .map(OwnedHandle::new)
        .map_err(|_| SupervisorError::JobFailed)?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(SupervisorError::JobFailed);
    }
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|_| SupervisorError::JobFailed)?;
    Ok(job)
}

fn process_start_time_100ns(process: HANDLE) -> Result<u64, SupervisorError> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) }
        .map_err(|_| SupervisorError::WaitFailed)?;
    Ok((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn encode_command_line(spec: &LaunchSpec) -> Result<Vec<u16>, SupervisorError> {
    let mut command_line = Vec::new();
    append_windows_argument(&mut command_line, spec.executable.as_os_str());
    for argument in &spec.arguments {
        command_line.push(b' ' as u16);
        append_windows_argument(&mut command_line, argument);
    }
    if command_line.len() + 1 > MAX_WINDOWS_COMMAND_LINE_UNITS {
        return Err(SupervisorError::SpecInvalid);
    }
    command_line.push(0);
    Ok(command_line)
}

fn append_windows_argument(output: &mut Vec<u16>, argument: &OsStr) {
    let units: Vec<u16> = argument.encode_wide().collect();
    let needs_quotes = units.is_empty()
        || units
            .iter()
            .any(|unit| *unit == b' ' as u16 || *unit == b'\t' as u16 || *unit == b'"' as u16);
    if !needs_quotes {
        output.extend_from_slice(&units);
        return;
    }

    output.push(b'"' as u16);
    let mut backslashes = 0_usize;
    for unit in units {
        if unit == b'\\' as u16 {
            backslashes += 1;
        } else if unit == b'"' as u16 {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2 + 1));
            output.push(unit);
            backslashes = 0;
        } else {
            output.extend(std::iter::repeat_n(b'\\' as u16, backslashes));
            output.push(unit);
            backslashes = 0;
        }
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    output.push(b'"' as u16);
}

fn encode_environment(spec: &LaunchSpec) -> Result<Vec<u16>, SupervisorError> {
    let mut entries: Vec<_> = spec.environment.iter().collect();
    entries.sort_by(|(left, _), (right, _)| {
        left.to_string_lossy()
            .to_lowercase()
            .cmp(&right.to_string_lossy().to_lowercase())
    });
    let mut block = Vec::new();
    for (key, value) in entries {
        block.extend(key.encode_wide());
        block.push(b'=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    if block.len() > MAX_WINDOWS_ENVIRONMENT_UNITS {
        return Err(SupervisorError::SpecInvalid);
    }
    Ok(block)
}

fn encode_null_terminated(value: &OsStr) -> Result<Vec<u16>, SupervisorError> {
    let mut encoded: Vec<u16> = value.encode_wide().collect();
    if encoded.contains(&0) || encoded.len() + 1 > MAX_WINDOWS_COMMAND_LINE_UNITS {
        return Err(SupervisorError::SpecInvalid);
    }
    encoded.push(0);
    Ok(encoded)
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    const fn raw(&self) -> HANDLE {
        self.0
    }

    fn into_file(self) -> File {
        let raw = self.0 .0;
        std::mem::forget(self);
        unsafe { File::from_raw_handle(raw) }
    }
}

unsafe impl Send for OwnedHandle {}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}
