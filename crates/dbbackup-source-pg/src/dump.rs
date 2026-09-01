// crates/dbbackup-source-pg/src/dump.rs
//! S03 §8 — spawn() 구현. tokio::process로 pg_dump/pg_dumpall 자식 프로세스를 구동한다.
//! R9: 비밀번호는 PGPASSWORD env로만 전달(argv/flags 문자열에 절대 노출 금지).
//! R10: stderr 전량 드레인 + 마지막 N줄만 보존(파이프 백프레셔 데드락 방지).

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dbbackup_core::{BackupScopeSelection, SelectedTable};
use dbbackup_pipeline::artifact::{
    DumpArtifact, DumpExit, DumpStream, NativeChildProcess, NativeDumpStream,
};
use dbbackup_pipeline::source::{DumpPartKind, DumpPlanPart};
#[cfg(windows)]
use dbbackup_runtime::{
    hash_executable, LaunchSpec, ProcessSupervisor, StreamingProcessExit,
    SupervisedStreamingProcess,
};
use tokio::io::AsyncRead;

use crate::PgConnCfg;

const STDERR_TAIL_LINES: usize = 200;

pub fn spawn_part(
    conn: &PgConnCfg,
    part: &DumpPlanPart,
    selection: &BackupScopeSelection,
) -> anyhow::Result<DumpArtifact> {
    let mut args = common_conn_args(conn);
    match &part.kind {
        DumpPartKind::Globals => args.extend(globals_args()),
        DumpPartKind::Database { name } => args.extend(db_args(name, selection)),
    }
    spawn_stream(
        conn,
        &part.dump_tool_path,
        part.dump_tool_sha256,
        &part.dump_tool_dependencies,
        &args,
    )
}

fn spawn_stream(
    conn: &PgConnCfg,
    tool_path: &std::path::Path,
    trusted_sha256: Option<[u8; 32]>,
    trusted_dependencies: &[(std::path::PathBuf, [u8; 32])],
    args: &[String],
) -> anyhow::Result<DumpArtifact> {
    #[cfg(not(windows))]
    {
        let _ = (conn, tool_path, trusted_sha256, trusted_dependencies, args);
        anyhow::bail!("NATIVE_PROCESS_SUPERVISION_REQUIRES_WINDOWS");
    }

    #[cfg(windows)]
    {
        spawn_supervised_stream(conn, tool_path, trusted_sha256, trusted_dependencies, args)
    }
}

#[cfg(windows)]
fn spawn_supervised_stream(
    conn: &PgConnCfg,
    tool_path: &std::path::Path,
    trusted_sha256: Option<[u8; 32]>,
    trusted_dependencies: &[(std::path::PathBuf, [u8; 32])],
    args: &[String],
) -> anyhow::Result<DumpArtifact> {
    let ssl_mode = pg_ssl_mode(&conn.tls_mode)?;
    let expected_sha256 = match trusted_sha256 {
        Some(expected_sha256) => expected_sha256,
        None => hash_executable(tool_path)?,
    };
    let working_directory = tool_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("PostgreSQL dump tool has no parent directory"))?;
    let spec = LaunchSpec::new(
        tool_path.to_path_buf(),
        working_directory.to_path_buf(),
        expected_sha256,
    )?
    .with_args(args.iter().cloned())?
    .with_environment(inherited_environment([
        ("PGPASSWORD", conn.password.as_str()),
        ("PGSSLMODE", ssl_mode),
    ]))?
    .with_trusted_dependencies(trusted_dependencies.iter().cloned())?
    .with_redacted_output_literals([conn.password.as_bytes()])?;
    let mut child = ProcessSupervisor::launch_streaming(&spec)?;
    let process = NativeChildProcess::supervised(
        i64::from(child.id()),
        child.start_time_ms()?,
        child.job_key(),
        child.executable_path().to_path_buf(),
        *child.executable_sha256(),
    )?;
    let stdout = tokio::fs::File::from_std(child.take_stdout()?);
    let stderr = tokio::fs::File::from_std(child.take_stderr()?);
    let abort_requested = Arc::new(AtomicBool::new(false));
    let waiter_abort = Arc::clone(&abort_requested);
    let process_waiter =
        tokio::task::spawn_blocking(move || wait_for_supervised_exit(child, &waiter_abort));
    let mut abort_on_drop = AbortOnDrop::new(Arc::clone(&abort_requested));

    let wait = Box::pin(async move {
        let stderr_task = tokio::spawn(drain_stderr_tail(stderr));
        let exit = process_waiter
            .await
            .map_err(|error| anyhow::anyhow!("PostgreSQL dump waiter failed: {error}"))??;
        abort_on_drop.disarm();
        let stderr_tail = stderr_task.await.unwrap_or_default();
        Ok(DumpExit {
            code: Some(exit.exit_code() as i32),
            success: exit.exit_code() == 0,
            stderr_tail,
        })
    });

    Ok(DumpArtifact::NativeStream(NativeDumpStream {
        process,
        stream: DumpStream {
            reader: Box::new(stdout),
            wait,
            abort: Some(Box::new(move || {
                abort_requested.store(true, Ordering::Release);
            })),
        },
    }))
}

#[cfg(windows)]
fn wait_for_supervised_exit(
    mut child: SupervisedStreamingProcess,
    abort_requested: &AtomicBool,
) -> Result<StreamingProcessExit, dbbackup_runtime::SupervisorError> {
    loop {
        if abort_requested.load(Ordering::Acquire) {
            return child.terminate();
        }
        if let Some(exit) = child.wait_timeout(Duration::from_millis(50))? {
            return Ok(exit);
        }
    }
}

#[cfg(windows)]
fn inherited_environment<const N: usize>(
    overrides: [(&str, &str); N],
) -> Vec<(OsString, OsString)> {
    let mut environment = BTreeMap::new();
    for (key, value) in std::env::vars_os() {
        let normalized = key.to_string_lossy().to_lowercase();
        if !normalized.contains('=') {
            environment.insert(normalized, (key, value));
        }
    }
    for (key, value) in overrides {
        environment.insert(
            key.to_ascii_lowercase(),
            (OsString::from(key), OsString::from(value)),
        );
    }
    environment.into_values().collect()
}

struct AbortOnDrop {
    requested: Arc<AtomicBool>,
    armed: bool,
}

impl AbortOnDrop {
    fn new(requested: Arc<AtomicBool>) -> Self {
        Self {
            requested,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.requested.store(true, Ordering::Release);
        }
    }
}

fn pg_ssl_mode(tls_mode: &str) -> anyhow::Result<&'static str> {
    match tls_mode {
        "disabled" => Ok("disable"),
        "preferred" => Ok("prefer"),
        "required" => Ok("require"),
        "verify_ca" => Ok("verify-ca"),
        "verify_full" => Ok("verify-full"),
        other => Err(anyhow::anyhow!("unsupported PostgreSQL TLS mode: {other}")),
    }
}

async fn drain_stderr_tail(stderr: impl AsyncRead + Unpin) -> String {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    let mut ring: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
    while let Ok(Some(line)) = lines.next_line().await {
        if ring.len() == STDERR_TAIL_LINES {
            ring.pop_front();
        }
        ring.push_back(line);
    }
    ring.into_iter().collect::<Vec<_>>().join("\n")
}

fn common_conn_args(conn: &PgConnCfg) -> Vec<String> {
    vec![
        "-h".into(),
        conn.host.clone(),
        "-p".into(),
        conn.port.to_string(),
        "-U".into(),
        conn.user.clone(),
        "--no-password".into(), // PGPASSWORD 없으면 즉시 실패(무인 안전장치)
    ]
}

fn db_args(db: &str, selection: &BackupScopeSelection) -> Vec<String> {
    let mut args = vec!["-Fc".into(), "-f".into(), "-".into()];
    if !selection.is_whole_database() {
        args.push("--strict-names".into());
        args.extend(
            selection
                .tables()
                .iter()
                .map(|table| format!("--table={}", exact_table_pattern(table))),
        );
    }
    args.push(db.into());
    args
}

fn exact_table_pattern(table: &SelectedTable) -> String {
    format!(
        "{}.{}",
        quote_pattern_identifier(table.schema_name()),
        quote_pattern_identifier(table.table_name())
    )
}

fn quote_pattern_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn globals_args() -> Vec<String> {
    vec!["--globals-only".into()]
}

/// dump_flags 매니페스트 필드용 원문. PGPASSWORD가 애초에 args에 없어 redaction 불필요(R9의 이득).
pub fn render_db_flags(conn: &PgConnCfg, db: &str, selection: &BackupScopeSelection) -> String {
    let mut args = common_conn_args(conn);
    args.extend(db_args(db, selection));
    format!("pg_dump {}", args.join(" "))
}

pub fn render_globals_flags(conn: &PgConnCfg) -> String {
    let mut args = common_conn_args(conn);
    args.extend(globals_args());
    format!("pg_dumpall {}", args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbbackup_core::{BackupScopeSelection, SelectedTable};
    use zeroize::Zeroizing;

    fn cfg() -> PgConnCfg {
        PgConnCfg {
            host: "db.internal".into(),
            port: 5432,
            user: "backup".into(),
            password: Zeroizing::new("s3cr3t-should-not-leak".into()),
            admin_database: "postgres".into(),
            tls_mode: "verify_full".into(),
        }
    }

    #[test]
    fn t13_flags_never_contain_password() {
        let c = cfg();
        let db = render_db_flags(&c, "app", &BackupScopeSelection::whole_database());
        let gl = render_globals_flags(&c);
        assert!(!db.contains("s3cr3t-should-not-leak"));
        assert!(!gl.contains("s3cr3t-should-not-leak"));
        assert!(db.contains("-Fc"));
        assert!(gl.contains("--globals-only"));
    }

    #[test]
    fn tls_modes_map_to_libpq_sslmode_without_entering_manifest_flags() {
        assert_eq!(pg_ssl_mode("disabled").expect("disabled"), "disable");
        assert_eq!(pg_ssl_mode("preferred").expect("preferred"), "prefer");
        assert_eq!(pg_ssl_mode("required").expect("required"), "require");
        assert_eq!(pg_ssl_mode("verify_ca").expect("verify ca"), "verify-ca");
        assert_eq!(
            pg_ssl_mode("verify_full").expect("verify full"),
            "verify-full"
        );
        assert!(pg_ssl_mode("trust_everything").is_err());
        assert!(
            !render_db_flags(&cfg(), "app", &BackupScopeSelection::whole_database())
                .contains("verify-full")
        );
    }

    #[test]
    fn explicit_tables_share_one_database_dump_argument_vector() {
        let selection = BackupScopeSelection::explicit_tables(vec![
            SelectedTable::new("public", "wild*?[name]").expect("selected table"),
            SelectedTable::new("audit", "Order.Items").expect("selected table"),
            SelectedTable::new("public", "odd\"name").expect("selected table"),
        ])
        .expect("explicit selection");

        assert_eq!(
            db_args("app", &selection),
            vec![
                "-Fc",
                "-f",
                "-",
                "--strict-names",
                "--table=\"audit\".\"Order.Items\"",
                "--table=\"public\".\"odd\"\"name\"",
                "--table=\"public\".\"wild*?[name]\"",
                "app",
            ]
        );
    }

    #[test]
    fn whole_database_dump_has_no_table_filter() {
        let args = db_args("app", &BackupScopeSelection::whole_database());

        assert_eq!(args, vec!["-Fc", "-f", "-", "app"]);
    }
}
