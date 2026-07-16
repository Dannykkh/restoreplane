// crates/dbbackup-source-pg/src/dump.rs
//! S03 §8 — spawn() 구현. tokio::process로 pg_dump/pg_dumpall 자식 프로세스를 구동한다.
//! R9: 비밀번호는 PGPASSWORD env로만 전달(argv/flags 문자열에 절대 노출 금지).
//! R10: stderr 전량 드레인 + 마지막 N줄만 보존(파이프 백프레셔 데드락 방지).

use std::collections::VecDeque;
use std::process::Stdio;

use dbbackup_pipeline::artifact::{DumpArtifact, DumpExit, DumpStream};
use dbbackup_pipeline::source::{DumpPartKind, DumpPlanPart};
use tokio::process::Command;

use crate::PgConnCfg;

const STDERR_TAIL_LINES: usize = 200;

pub fn spawn_part(conn: &PgConnCfg, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
    let mut args = common_conn_args(conn);
    match &part.kind {
        DumpPartKind::Globals => args.extend(globals_args()),
        DumpPartKind::Database { name } => args.extend(db_args(name)),
    }
    spawn_stream(conn, &part.dump_tool_path, &args)
}

fn spawn_stream(
    conn: &PgConnCfg,
    tool_path: &std::path::Path,
    args: &[String],
) -> anyhow::Result<DumpArtifact> {
    let mut cmd = Command::new(tool_path);
    cmd.args(args)
        .env("PGPASSWORD", conn.password.as_str()) // R9: 오직 env
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("덤프 도구 실행 실패({}): {e}", tool_path.display()))?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let wait = Box::pin(async move {
        let stderr_tail = drain_stderr_tail(stderr).await; // R10
        let status = child.wait().await?;
        Ok(DumpExit {
            code: status.code(),
            success: status.success(),
            stderr_tail,
        })
    });

    Ok(DumpArtifact::Stream(DumpStream {
        reader: Box::new(stdout),
        wait,
    }))
}

async fn drain_stderr_tail(stderr: tokio::process::ChildStderr) -> String {
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

fn db_args(db: &str) -> Vec<String> {
    vec!["-Fc".into(), "-f".into(), "-".into(), db.into()]
}

fn globals_args() -> Vec<String> {
    vec!["--globals-only".into()]
}

/// dump_flags 매니페스트 필드용 원문. PGPASSWORD가 애초에 args에 없어 redaction 불필요(R9의 이득).
pub fn render_db_flags(conn: &PgConnCfg, db: &str) -> String {
    let mut args = common_conn_args(conn);
    args.extend(db_args(db));
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
    use zeroize::Zeroizing;

    fn cfg() -> PgConnCfg {
        PgConnCfg {
            host: "db.internal".into(),
            port: 5432,
            user: "backup".into(),
            password: Zeroizing::new("s3cr3t-should-not-leak".into()),
            admin_database: "postgres".into(),
        }
    }

    #[test]
    fn t13_flags_never_contain_password() {
        let c = cfg();
        let db = render_db_flags(&c, "app");
        let gl = render_globals_flags(&c);
        assert!(!db.contains("s3cr3t-should-not-leak"));
        assert!(!gl.contains("s3cr3t-should-not-leak"));
        assert!(db.contains("-Fc"));
        assert!(gl.contains("--globals-only"));
    }
}
