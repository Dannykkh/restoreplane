use std::cell::Cell;
use std::path::{Path, PathBuf};

use dbbackup_core::{Engine, SupportStatus};
use dbbackup_source_sqlserver::{
    candidate_certification, execute_export, BacpacExportRequest, IntegratedEndpoint,
    ObservedSqlPackage, SqlPackageExit, SqlPackageLauncher, SqlPackagePolicy,
    SqlServerArtifactKind, SqlServerProviderError, SQLSERVER_BACPAC_LIMITATIONS,
};

fn trusted_observation(path: &Path) -> ObservedSqlPackage {
    ObservedSqlPackage::new(
        path,
        "170.4.83",
        "a".repeat(64),
        Some("Microsoft Corporation"),
    )
    .expect("valid observation")
}

fn policy() -> SqlPackagePolicy {
    SqlPackagePolicy::exact("170.4.83", "a".repeat(64), "Microsoft Corporation")
        .expect("valid policy")
}

#[test]
fn bacpac_commands_are_typed_secret_free_and_dacpac_never_claims_data_backup() {
    let tool = trusted_observation(Path::new(r"C:\tools\sqlpackage.exe"))
        .verify(&policy())
        .expect("trusted tool");
    let request = BacpacExportRequest::new(
        IntegratedEndpoint::new("sql.internal,1433", "app").expect("endpoint"),
        PathBuf::from(r"C:\spool\app.bacpac"),
    )
    .expect("export request");
    let command = request.command(&tool).expect("export command");
    assert_eq!(command.program(), Path::new(r"C:\tools\sqlpackage.exe"));
    assert!(command.args().iter().any(|arg| arg == "/Action:Export"));
    assert!(command
        .args()
        .iter()
        .any(|arg| arg == "/OverwriteFiles:False"));
    assert!(command
        .args()
        .iter()
        .all(|arg| !arg.to_ascii_lowercase().contains("password")));
    assert_eq!(request.artifact_kind(), SqlServerArtifactKind::Bacpac);
    assert!(!SqlServerArtifactKind::Dacpac.qualifies_as_data_backup());
}

struct CountingLauncher {
    calls: Cell<usize>,
}

impl SqlPackageLauncher for CountingLauncher {
    fn launch(
        &self,
        _program: &Path,
        _args: &[String],
    ) -> Result<SqlPackageExit, SqlServerProviderError> {
        self.calls.set(self.calls.get() + 1);
        Ok(SqlPackageExit::success())
    }
}

#[test]
fn tool_hash_or_version_mismatch_starts_zero_export_processes() {
    let launcher = CountingLauncher {
        calls: Cell::new(0),
    };
    let request = BacpacExportRequest::new(
        IntegratedEndpoint::new("sql.internal,1433", "app").expect("endpoint"),
        PathBuf::from(r"C:\spool\app.bacpac"),
    )
    .expect("request");
    let mismatch = trusted_observation(Path::new(r"C:\tools\sqlpackage.exe"));
    let wrong_policy = SqlPackagePolicy::exact("170.4.83", "b".repeat(64), "Microsoft Corporation")
        .expect("policy");

    let error = execute_export(&mismatch, &wrong_policy, &request, &launcher)
        .expect_err("untrusted binary must fail before launch");
    assert_eq!(error.code(), "SQLSERVER_TOOL_IDENTITY_MISMATCH");
    assert_eq!(launcher.calls.get(), 0);
}

#[test]
fn candidate_matrix_is_never_supported_without_round_trip_evidence() {
    let certification = candidate_certification(
        "0123456789abcdef0123456789abcdef01234567",
        &"c".repeat(64),
        trusted_observation(Path::new(r"C:\tools\sqlpackage.exe")),
        1_785_280_000_000,
    )
    .expect("candidate certification");
    assert_eq!(certification.engine(), Engine::Sqlserver);
    assert_eq!(certification.status(), SupportStatus::Candidate);
    assert!(!certification.is_publishable_supported());
    assert!(SQLSERVER_BACPAC_LIMITATIONS
        .iter()
        .any(|limitation| limitation.code == "SQLSERVER_AGENT_JOBS_EXCLUDED"));
    assert!(SQLSERVER_BACPAC_LIMITATIONS
        .iter()
        .any(|limitation| limitation.code == "SQLSERVER_BACPAC_NOT_ENCRYPTED"));
}
