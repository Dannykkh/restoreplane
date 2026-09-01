//! SQL Server Wave-1 candidate.
//!
//! This crate deliberately exposes only BACPAC export/import with Windows
//! integrated authentication. SQL authentication would put a password on the
//! SqlPackage command line, so it remains an explicit limitation. A DACPAC is
//! never accepted as data-backup evidence.

use std::path::{Path, PathBuf};
use std::process::Command;

use dbbackup_core::{
    CapabilityEvidence, CapabilityOutcome, CertificationIdentity, Engine, NativeToolIdentity,
    ProviderCapability, ProviderCertification, ProviderContractError, ProviderLimitation,
    ToolTrust,
};
use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlServerArtifactKind {
    Bacpac,
    Dacpac,
}

impl SqlServerArtifactKind {
    pub const fn qualifies_as_data_backup(self) -> bool {
        matches!(self, Self::Bacpac)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LimitationDefinition {
    pub code: &'static str,
    pub detail: &'static str,
}

pub const SQLSERVER_BACPAC_LIMITATIONS: [LimitationDefinition; 10] = [
    LimitationDefinition {
        code: "SQLSERVER_LOGINS_EXCLUDED",
        detail: "Server logins and their usable passwords are not fully recovered by BACPAC.",
    },
    LimitationDefinition {
        code: "SQLSERVER_SERVER_ROLES_EXCLUDED",
        detail: "Server-level role membership is outside the database-scoped BACPAC.",
    },
    LimitationDefinition {
        code: "SQLSERVER_AGENT_JOBS_EXCLUDED",
        detail: "SQL Server Agent jobs are instance objects and are not included.",
    },
    LimitationDefinition {
        code: "SQLSERVER_PERMISSIONS_LIMITED",
        detail: "Some server and cross-database permissions require separate recovery.",
    },
    LimitationDefinition {
        code: "SQLSERVER_UNSUPPORTED_OBJECTS_LIMITED",
        detail: "The BACPAC model is limited to the Azure SQL Database surface area.",
    },
    LimitationDefinition {
        code: "SQLSERVER_ENCRYPTED_OBJECTS_LIMITED",
        detail: "Externally protected keys and encrypted features require separate key custody.",
    },
    LimitationDefinition {
        code: "SQLSERVER_BACPAC_NOT_ENCRYPTED",
        detail: "BACPAC compression is not encryption; the common encrypted Replica pipeline is required.",
    },
    LimitationDefinition {
        code: "SQLSERVER_EXPORT_CONSISTENCY_REQUIRES_QUIESCE",
        detail: "Transactional consistency requires no writes or a transactionally consistent source copy.",
    },
    LimitationDefinition {
        code: "SQLSERVER_BACPAC_SCALE_CANDIDATE",
        detail: "Microsoft documents best performance below 200 GB; product limits require measured fixture evidence.",
    },
    LimitationDefinition {
        code: "SQLSERVER_SQL_AUTH_ARGV_UNSUPPORTED",
        detail: "Wave 1 accepts integrated authentication only because SqlPackage SQL passwords are command-line parameters.",
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntegratedEndpoint {
    server: String,
    database: String,
}

impl IntegratedEndpoint {
    pub fn new(
        server: impl Into<String>,
        database: impl Into<String>,
    ) -> Result<Self, SqlServerProviderError> {
        let value = Self {
            server: server.into(),
            database: database.into(),
        };
        if !valid_text(&value.server, 1, 1_024)
            || !valid_text(&value.database, 1, 128)
            || value.database.contains(['/', '\\', ':'])
        {
            return Err(SqlServerProviderError::RequestInvalid);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug)]
pub struct ObservedSqlPackage {
    path: PathBuf,
    version: String,
    sha256: String,
    authenticode_publisher: Option<String>,
}

impl ObservedSqlPackage {
    pub fn new(
        path: impl Into<PathBuf>,
        version: impl Into<String>,
        sha256: impl Into<String>,
        authenticode_publisher: Option<impl Into<String>>,
    ) -> Result<Self, SqlServerProviderError> {
        let value = Self {
            path: path.into(),
            version: version.into(),
            sha256: sha256.into(),
            authenticode_publisher: authenticode_publisher.map(Into::into),
        };
        if !value.path.is_absolute()
            || !valid_text(&value.version, 1, 128)
            || !is_sha256(&value.sha256)
            || value
                .authenticode_publisher
                .as_ref()
                .is_some_and(|publisher| !valid_text(publisher, 1, 512))
        {
            return Err(SqlServerProviderError::ToolObservationInvalid);
        }
        Ok(value)
    }

    pub fn verify(
        &self,
        policy: &SqlPackagePolicy,
    ) -> Result<VerifiedSqlPackage, SqlServerProviderError> {
        if self.version != policy.version
            || !self.sha256.eq_ignore_ascii_case(&policy.sha256)
            || self.authenticode_publisher.as_deref() != Some(policy.publisher.as_str())
        {
            return Err(SqlServerProviderError::ToolIdentityMismatch);
        }
        Ok(VerifiedSqlPackage {
            path: self.path.clone(),
            version: self.version.clone(),
            sha256: self.sha256.to_ascii_lowercase(),
            publisher: policy.publisher.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlPackagePolicy {
    version: String,
    sha256: String,
    publisher: String,
}

impl SqlPackagePolicy {
    pub fn exact(
        version: impl Into<String>,
        sha256: impl Into<String>,
        publisher: impl Into<String>,
    ) -> Result<Self, SqlServerProviderError> {
        let value = Self {
            version: version.into(),
            sha256: sha256.into(),
            publisher: publisher.into(),
        };
        if !valid_text(&value.version, 1, 128)
            || !is_sha256(&value.sha256)
            || !valid_text(&value.publisher, 1, 512)
        {
            return Err(SqlServerProviderError::ToolPolicyInvalid);
        }
        Ok(value)
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedSqlPackage {
    path: PathBuf,
    version: String,
    sha256: String,
    publisher: String,
}

impl VerifiedSqlPackage {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn publisher(&self) -> &str {
        &self.publisher
    }
}

#[derive(Clone, Debug)]
pub struct BacpacExportRequest {
    source: IntegratedEndpoint,
    target_file: PathBuf,
    timeout_seconds: u32,
    max_parallelism: u16,
}

impl BacpacExportRequest {
    pub fn new(
        source: IntegratedEndpoint,
        target_file: PathBuf,
    ) -> Result<Self, SqlServerProviderError> {
        if !has_extension(&target_file, "bacpac") {
            return Err(SqlServerProviderError::DacpacNotDataBackup);
        }
        Ok(Self {
            source,
            target_file,
            timeout_seconds: 300,
            max_parallelism: 4,
        })
    }

    pub fn with_bounds(
        mut self,
        timeout_seconds: u32,
        max_parallelism: u16,
    ) -> Result<Self, SqlServerProviderError> {
        if !(1..=86_400).contains(&timeout_seconds) || !(1..=64).contains(&max_parallelism) {
            return Err(SqlServerProviderError::RequestInvalid);
        }
        self.timeout_seconds = timeout_seconds;
        self.max_parallelism = max_parallelism;
        Ok(self)
    }

    pub const fn artifact_kind(&self) -> SqlServerArtifactKind {
        SqlServerArtifactKind::Bacpac
    }

    pub fn command(
        &self,
        tool: &VerifiedSqlPackage,
    ) -> Result<SqlPackageCommand, SqlServerProviderError> {
        if self.target_file.exists() {
            return Err(SqlServerProviderError::OutputExists);
        }
        Ok(SqlPackageCommand {
            program: tool.path.clone(),
            args: vec![
                "/Action:Export".to_owned(),
                format!("/SourceServerName:{}", self.source.server),
                format!("/SourceDatabaseName:{}", self.source.database),
                format!("/TargetFile:{}", self.target_file.display()),
                "/SourceEncryptConnection:True".to_owned(),
                "/SourceTrustServerCertificate:False".to_owned(),
                format!("/SourceTimeout:{}", self.timeout_seconds),
                format!("/MaxParallelism:{}", self.max_parallelism),
                "/OverwriteFiles:False".to_owned(),
                "/Quiet:True".to_owned(),
            ],
        })
    }
}

#[derive(Clone, Debug)]
pub struct BacpacImportRequest {
    source_file: PathBuf,
    target: IntegratedEndpoint,
    timeout_seconds: u32,
    max_parallelism: u16,
}

impl BacpacImportRequest {
    pub fn new(
        source_file: PathBuf,
        target: IntegratedEndpoint,
    ) -> Result<Self, SqlServerProviderError> {
        if !has_extension(&source_file, "bacpac") {
            return Err(SqlServerProviderError::DacpacNotDataBackup);
        }
        Ok(Self {
            source_file,
            target,
            timeout_seconds: 300,
            max_parallelism: 4,
        })
    }

    pub fn command(
        &self,
        tool: &VerifiedSqlPackage,
    ) -> Result<SqlPackageCommand, SqlServerProviderError> {
        if !self.source_file.is_file() {
            return Err(SqlServerProviderError::SourceMissing);
        }
        Ok(SqlPackageCommand {
            program: tool.path.clone(),
            args: vec![
                "/Action:Import".to_owned(),
                format!("/SourceFile:{}", self.source_file.display()),
                format!("/TargetServerName:{}", self.target.server),
                format!("/TargetDatabaseName:{}", self.target.database),
                "/TargetEncryptConnection:True".to_owned(),
                "/TargetTrustServerCertificate:False".to_owned(),
                format!("/TargetTimeout:{}", self.timeout_seconds),
                format!("/MaxParallelism:{}", self.max_parallelism),
                "/Quiet:True".to_owned(),
            ],
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlPackageCommand {
    program: PathBuf,
    args: Vec<String>,
}

impl SqlPackageCommand {
    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn args(&self) -> &[String] {
        &self.args
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SqlPackageExit {
    pub code: Option<i32>,
    pub success: bool,
    pub stderr_tail: String,
}

impl SqlPackageExit {
    pub fn success() -> Self {
        Self {
            code: Some(0),
            success: true,
            stderr_tail: String::new(),
        }
    }
}

pub trait SqlPackageLauncher {
    fn launch(
        &self,
        program: &Path,
        args: &[String],
    ) -> Result<SqlPackageExit, SqlServerProviderError>;
}

pub struct SystemSqlPackageLauncher;

impl SqlPackageLauncher for SystemSqlPackageLauncher {
    fn launch(
        &self,
        program: &Path,
        args: &[String],
    ) -> Result<SqlPackageExit, SqlServerProviderError> {
        let output = Command::new(program)
            .args(args)
            .output()
            .map_err(SqlServerProviderError::ToolLaunch)?;
        Ok(SqlPackageExit {
            code: output.status.code(),
            success: output.status.success(),
            stderr_tail: bounded_tail(&String::from_utf8_lossy(&output.stderr), 200),
        })
    }
}

pub fn execute_export(
    observed: &ObservedSqlPackage,
    policy: &SqlPackagePolicy,
    request: &BacpacExportRequest,
    launcher: &dyn SqlPackageLauncher,
) -> Result<SqlPackageExit, SqlServerProviderError> {
    let tool = observed.verify(policy)?;
    let command = request.command(&tool)?;
    let exit = launcher.launch(command.program(), command.args())?;
    if !exit.success {
        return Err(SqlServerProviderError::ToolFailed {
            code: exit.code,
            stderr_tail: exit.stderr_tail,
        });
    }
    Ok(exit)
}

pub fn execute_import(
    observed: &ObservedSqlPackage,
    policy: &SqlPackagePolicy,
    request: &BacpacImportRequest,
    launcher: &dyn SqlPackageLauncher,
) -> Result<SqlPackageExit, SqlServerProviderError> {
    let tool = observed.verify(policy)?;
    let command = request.command(&tool)?;
    let exit = launcher.launch(command.program(), command.args())?;
    if !exit.success {
        return Err(SqlServerProviderError::ToolFailed {
            code: exit.code,
            stderr_tail: exit.stderr_tail,
        });
    }
    Ok(exit)
}

pub fn candidate_certification(
    source_revision: &str,
    build_id: &str,
    observation: ObservedSqlPackage,
    recorded_at_ms: i64,
) -> Result<ProviderCertification, SqlServerProviderError> {
    let trust = observation
        .authenticode_publisher
        .clone()
        .map(|publisher| ToolTrust::AuthenticodeValid { publisher })
        .unwrap_or(ToolTrust::Unverified);
    let tool = NativeToolIdentity::new(
        "sqlpackage",
        observation.path.display().to_string(),
        observation.version,
        observation.sha256,
        trust,
    )?;
    let identity = CertificationIdentity::new(
        source_revision,
        build_id,
        "windows",
        std::env::consts::ARCH,
        "sqlserver-bacpac-roundtrip-not-run",
        recorded_at_ms,
    )?;
    let limitations = SQLSERVER_BACPAC_LIMITATIONS
        .iter()
        .map(|item| ProviderLimitation::new(item.code, item.detail))
        .collect::<Result<Vec<_>, _>>()?;
    let capabilities = vec![CapabilityEvidence::new(
        ProviderCapability::ToolPreflight,
        CapabilityOutcome::Passed,
        "crates/dbbackup-source-sqlserver/tests/candidate_contract.rs",
        None,
    )?];
    ProviderCertification::evaluate(
        identity,
        Engine::Sqlserver,
        "SQL Server version/edition unverified",
        "fixture-unavailable",
        vec![tool],
        capabilities,
        limitations,
    )
    .map_err(SqlServerProviderError::from)
}

#[derive(Debug, Error)]
pub enum SqlServerProviderError {
    #[error("SQL Server request is invalid")]
    RequestInvalid,
    #[error("SqlPackage observation is invalid")]
    ToolObservationInvalid,
    #[error("SqlPackage policy is invalid")]
    ToolPolicyInvalid,
    #[error("SqlPackage identity does not match the trusted policy")]
    ToolIdentityMismatch,
    #[error("DACPAC is schema deployment evidence, not a data backup")]
    DacpacNotDataBackup,
    #[error("BACPAC output already exists")]
    OutputExists,
    #[error("BACPAC source does not exist")]
    SourceMissing,
    #[error("SqlPackage could not start: {0}")]
    ToolLaunch(#[source] std::io::Error),
    #[error("SqlPackage failed with code {code:?}: {stderr_tail}")]
    ToolFailed {
        code: Option<i32>,
        stderr_tail: String,
    },
    #[error("provider contract is invalid: {0}")]
    Provider(#[from] ProviderContractError),
}

impl SqlServerProviderError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::RequestInvalid => "SQLSERVER_REQUEST_INVALID",
            Self::ToolObservationInvalid => "SQLSERVER_TOOL_OBSERVATION_INVALID",
            Self::ToolPolicyInvalid => "SQLSERVER_TOOL_POLICY_INVALID",
            Self::ToolIdentityMismatch => "SQLSERVER_TOOL_IDENTITY_MISMATCH",
            Self::DacpacNotDataBackup => "SQLSERVER_DACPAC_NOT_DATA_BACKUP",
            Self::OutputExists => "SQLSERVER_BACPAC_OUTPUT_EXISTS",
            Self::SourceMissing => "SQLSERVER_BACPAC_SOURCE_MISSING",
            Self::ToolLaunch(_) => "SQLSERVER_TOOL_LAUNCH_FAILED",
            Self::ToolFailed { .. } => "SQLSERVER_TOOL_FAILED",
            Self::Provider(error) => error.code(),
        }
    }
}

fn valid_text(value: &str, minimum: usize, maximum: usize) -> bool {
    (minimum..=maximum).contains(&value.len()) && !value.contains(['\0', '\r', '\n'])
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn has_extension(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn bounded_tail(value: &str, lines: usize) -> String {
    let rows = value.lines().collect::<Vec<_>>();
    rows[rows.len().saturating_sub(lines)..].join("\n")
}
