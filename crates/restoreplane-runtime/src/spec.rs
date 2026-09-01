use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::Zeroizing;

const MAX_ARGUMENTS: usize = 256;
const MAX_ENVIRONMENT_ENTRIES: usize = 128;
const MAX_VALUE_UNITS: usize = 32_767;
const DEFAULT_OUTPUT_TAIL_BYTES: usize = 64 * 1024;
const MAX_OUTPUT_TAIL_BYTES: usize = 1024 * 1024;
const MAX_REDACTION_LITERALS: usize = 32;
const MAX_REDACTION_LITERAL_BYTES: usize = 4 * 1024;
pub const MAX_TRUSTED_DEPENDENCIES: usize = 512;
pub const MAX_TRUSTED_DEPENDENCY_PATH_UNITS: usize = 32_767;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrustedDependency {
    path: PathBuf,
    expected_sha256: [u8; 32],
}

impl TrustedDependency {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn expected_sha256(&self) -> &[u8; 32] {
        &self.expected_sha256
    }
}

#[derive(Clone)]
pub struct LaunchSpec {
    pub(crate) executable: PathBuf,
    pub(crate) working_directory: PathBuf,
    pub(crate) expected_sha256: [u8; 32],
    pub(crate) arguments: Vec<OsString>,
    pub(crate) environment: BTreeMap<OsString, OsString>,
    pub(crate) output_tail_bytes: usize,
    pub(crate) redacted_output_literals: Vec<Zeroizing<Vec<u8>>>,
    pub(crate) trusted_dependencies: Vec<TrustedDependency>,
}

impl LaunchSpec {
    pub fn new(
        executable: PathBuf,
        working_directory: PathBuf,
        expected_sha256: [u8; 32],
    ) -> Result<Self, SupervisorError> {
        if !executable.is_absolute() || !working_directory.is_absolute() {
            return Err(SupervisorError::SpecInvalid);
        }
        reject_symlink(&executable)?;
        reject_symlink(&working_directory)?;
        let executable = fs::canonicalize(executable).map_err(SupervisorError::Io)?;
        let working_directory = fs::canonicalize(working_directory).map_err(SupervisorError::Io)?;
        if !executable.is_file() || !working_directory.is_dir() {
            return Err(SupervisorError::SpecInvalid);
        }
        let spec = Self {
            executable,
            working_directory,
            expected_sha256,
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            output_tail_bytes: DEFAULT_OUTPUT_TAIL_BYTES,
            redacted_output_literals: Vec::new(),
            trusted_dependencies: Vec::new(),
        };
        spec.validate()?;
        Ok(spec)
    }

    pub fn with_args<I, S>(mut self, arguments: I) -> Result<Self, SupervisorError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self.validate()?;
        Ok(self)
    }

    pub fn with_environment<I, K, V>(mut self, entries: I) -> Result<Self, SupervisorError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.environment.clear();
        for (key, value) in entries {
            if self.environment.insert(key.into(), value.into()).is_some() {
                return Err(SupervisorError::SpecInvalid);
            }
        }
        self.validate()?;
        Ok(self)
    }

    pub fn with_output_tail_bytes(mut self, bytes: usize) -> Result<Self, SupervisorError> {
        self.output_tail_bytes = bytes;
        self.validate()?;
        Ok(self)
    }

    pub fn with_redacted_output_literals<I, B>(
        mut self,
        literals: I,
    ) -> Result<Self, SupervisorError>
    where
        I: IntoIterator<Item = B>,
        B: AsRef<[u8]>,
    {
        self.redacted_output_literals = literals
            .into_iter()
            .map(|literal| Zeroizing::new(literal.as_ref().to_vec()))
            .collect();
        self.validate()?;
        Ok(self)
    }

    pub fn with_trusted_dependencies<I, P>(
        mut self,
        dependencies: I,
    ) -> Result<Self, SupervisorError>
    where
        I: IntoIterator<Item = (P, [u8; 32])>,
        P: Into<PathBuf>,
    {
        let mut trusted_dependencies = Vec::new();
        let mut identities = std::collections::BTreeSet::new();
        for (path, expected_sha256) in dependencies {
            if trusted_dependencies.len() == MAX_TRUSTED_DEPENDENCIES {
                return Err(SupervisorError::SpecInvalid);
            }
            let path = path.into();
            if !path.is_absolute()
                || contains_nul(path.as_os_str())
                || os_units(path.as_os_str()) > MAX_TRUSTED_DEPENDENCY_PATH_UNITS
            {
                return Err(SupervisorError::SpecInvalid);
            }
            reject_reparse_path(&path)?;
            let path = fs::canonicalize(path).map_err(SupervisorError::Io)?;
            reject_reparse_path(&path)?;
            if !path.is_file() || !identities.insert(path_identity(&path)) {
                return Err(SupervisorError::SpecInvalid);
            }
            trusted_dependencies.push(TrustedDependency {
                path,
                expected_sha256,
            });
        }
        self.trusted_dependencies = trusted_dependencies;
        self.validate()?;
        Ok(self)
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn working_directory(&self) -> &Path {
        &self.working_directory
    }

    pub fn expected_sha256(&self) -> &[u8; 32] {
        &self.expected_sha256
    }

    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    pub fn environment(&self) -> &BTreeMap<OsString, OsString> {
        &self.environment
    }

    pub fn trusted_dependencies(&self) -> &[TrustedDependency] {
        &self.trusted_dependencies
    }

    pub(crate) fn validate(&self) -> Result<(), SupervisorError> {
        let mut normalized_environment_keys = std::collections::BTreeSet::new();
        let mut dependency_paths = std::collections::BTreeSet::new();
        let environment_invalid = self.environment.iter().any(|(key, value)| {
            key.is_empty()
                || contains_nul(key)
                || contains_nul(value)
                || os_units(key) > MAX_VALUE_UNITS
                || os_units(value) > MAX_VALUE_UNITS
                || key.to_string_lossy().contains('=')
                || !normalized_environment_keys.insert(key.to_string_lossy().to_lowercase())
        });
        if !self.executable.is_absolute()
            || !self.working_directory.is_absolute()
            || self.arguments.len() > MAX_ARGUMENTS
            || self.environment.len() > MAX_ENVIRONMENT_ENTRIES
            || self.output_tail_bytes == 0
            || self.output_tail_bytes > MAX_OUTPUT_TAIL_BYTES
            || self.redacted_output_literals.len() > MAX_REDACTION_LITERALS
            || self.trusted_dependencies.len() > MAX_TRUSTED_DEPENDENCIES
            || self
                .redacted_output_literals
                .iter()
                .any(|literal| literal.is_empty() || literal.len() > MAX_REDACTION_LITERAL_BYTES)
            || contains_nul(self.executable.as_os_str())
            || contains_nul(self.working_directory.as_os_str())
            || self
                .arguments
                .iter()
                .any(|argument| contains_nul(argument) || os_units(argument) > MAX_VALUE_UNITS)
            || self.trusted_dependencies.iter().any(|dependency| {
                !dependency.path.is_absolute()
                    || contains_nul(dependency.path.as_os_str())
                    || os_units(dependency.path.as_os_str()) > MAX_TRUSTED_DEPENDENCY_PATH_UNITS
                    || !dependency_paths.insert(path_identity(&dependency.path))
            })
            || environment_invalid
        {
            return Err(SupervisorError::SpecInvalid);
        }
        Ok(())
    }
}

impl fmt::Debug for LaunchSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LaunchSpec")
            .field("executable", &self.executable)
            .field("working_directory", &self.working_directory)
            .field("argument_count", &self.arguments.len())
            .field("environment_entry_count", &self.environment.len())
            .field("output_tail_bytes", &self.output_tail_bytes)
            .field("trusted_dependency_count", &self.trusted_dependencies.len())
            .field(
                "redacted_output_literal_count",
                &self.redacted_output_literals.len(),
            )
            .finish()
    }
}

pub fn hash_executable(path: &Path) -> Result<[u8; 32], SupervisorError> {
    if !path.is_absolute() {
        return Err(SupervisorError::SpecInvalid);
    }
    let mut file = File::open(path).map_err(SupervisorError::Io)?;
    hash_reader(&mut file)
}

pub(crate) fn hash_reader(reader: &mut impl Read) -> Result<[u8; 32], SupervisorError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(SupervisorError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("PROCESS_SPEC_INVALID")]
    SpecInvalid,
    #[error("PROCESS_EXECUTABLE_NOT_TRUSTED")]
    ExecutableNotTrusted,
    #[error("PROCESS_DEPENDENCY_NOT_TRUSTED")]
    DependencyNotTrusted,
    #[error("PROCESS_IO")]
    Io(#[source] io::Error),
    #[error("PROCESS_CREATE_FAILED")]
    CreateFailed,
    #[error("PROCESS_JOB_FAILED")]
    JobFailed,
    #[error("PROCESS_PIPE_FAILED")]
    PipeFailed,
    #[error("PROCESS_HANDLE_INHERITANCE_FAILED")]
    InheritanceFailed,
    #[error("PROCESS_RESUME_FAILED")]
    ResumeFailed,
    #[error("PROCESS_WAIT_FAILED")]
    WaitFailed,
    #[error("PROCESS_OUTPUT_DRAIN_FAILED")]
    OutputDrainFailed,
}

impl SupervisorError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::SpecInvalid => "PROCESS_SPEC_INVALID",
            Self::ExecutableNotTrusted => "PROCESS_EXECUTABLE_NOT_TRUSTED",
            Self::DependencyNotTrusted => "PROCESS_DEPENDENCY_NOT_TRUSTED",
            Self::Io(_) => "PROCESS_IO",
            Self::CreateFailed => "PROCESS_CREATE_FAILED",
            Self::JobFailed => "PROCESS_JOB_FAILED",
            Self::PipeFailed => "PROCESS_PIPE_FAILED",
            Self::InheritanceFailed => "PROCESS_HANDLE_INHERITANCE_FAILED",
            Self::ResumeFailed => "PROCESS_RESUME_FAILED",
            Self::WaitFailed => "PROCESS_WAIT_FAILED",
            Self::OutputDrainFailed => "PROCESS_OUTPUT_DRAIN_FAILED",
        }
    }
}

fn reject_symlink(path: &Path) -> Result<(), SupervisorError> {
    let metadata = fs::symlink_metadata(path).map_err(SupervisorError::Io)?;
    if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
        return Err(SupervisorError::SpecInvalid);
    }
    Ok(())
}

pub(crate) fn reject_reparse_path(path: &Path) -> Result<(), SupervisorError> {
    for component_path in path.ancestors() {
        let metadata = fs::symlink_metadata(component_path).map_err(SupervisorError::Io)?;
        if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
            return Err(SupervisorError::SpecInvalid);
        }
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn metadata_is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(windows)]
pub(crate) fn path_identity(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

#[cfg(not(windows))]
pub(crate) fn path_identity(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[cfg(windows)]
fn contains_nul(value: &OsStr) -> bool {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().any(|unit| unit == 0)
}

#[cfg(not(windows))]
fn contains_nul(value: &OsStr) -> bool {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().contains(&0)
}

#[cfg(windows)]
fn os_units(value: &OsStr) -> usize {
    use std::os::windows::ffi::OsStrExt;
    value.encode_wide().count()
}

#[cfg(not(windows))]
fn os_units(value: &OsStr) -> usize {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().len()
}
