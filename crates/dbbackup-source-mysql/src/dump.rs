// crates/dbbackup-source-mysql/src/dump.rs
//! S04 §7 — 자식 프로세스 기동. tokio::process로 mysqldump를 구동한다(source-pg dump.rs와 평행).
//! R10: 비밀번호는 MYSQL_PWD env로만 전달(argv/flags 문자열에 절대 노출 금지 — ps/작업관리자 평문 차단).
//! stderr 전량 드레인 + 마지막 N줄만 보존(파이프 백프레셔 데드락 방지).

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;

use tokio::process::{Child, Command};
use zeroize::Zeroizing;

const STDERR_TAIL_LINES: usize = 200;

pub struct MySqlConnCfg {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 값 자체는 S01 Config가 관리. S02 규율과 동일하게 zeroize.
    pub password: Zeroizing<String>,
    pub database: String,
}

/// 연결 인자(비밀번호 제외). dump_flags 매니페스트 표기와 실제 argv에서 공유한다.
fn conn_args(conn: &MySqlConnCfg) -> Vec<String> {
    vec![
        format!("--host={}", conn.host),
        format!("--port={}", conn.port),
        format!("--user={}", conn.user),
    ]
}

/// mysqldump 자식 프로세스를 기동한다. 비밀번호는 오직 MYSQL_PWD env로만 전달(R10).
/// dump_args는 flags::build_flags 결과 + DB명. 프로세스 시작 자체는 non-blocking.
pub fn spawn_mysqldump(
    binary: &Path,
    dump_args: &[String],
    conn: &MySqlConnCfg,
) -> anyhow::Result<Child> {
    let mut cmd = Command::new(binary);
    cmd.args(dump_args)
        .args(conn_args(conn))
        .env("MYSQL_PWD", conn.password.as_str()) // R10: 오직 env
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.spawn()
        .map_err(|e| anyhow::anyhow!("mysqldump 실행 실패({}): {e}", binary.display()))
}

/// stderr를 계속 비우면서(파이프 풀 데드락 방지) 최근 N줄만 유지한다(R10).
pub async fn drain_stderr_tail(stderr: tokio::process::ChildStderr) -> String {
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

/// 매니페스트 dump_flags 필드용 원문. MYSQL_PWD가 애초에 argv에 없어 redaction 불필요(R10의 이득).
/// 연결정보(host/port/user)는 재현성에 불필요하므로 redacted로 남긴다(S11 정합).
pub fn render_dump_flags(binary: &Path, dump_args: &[String]) -> String {
    format!(
        "{} {} --host=<redacted> --port=<redacted> --user=<redacted>",
        binary.display(),
        dump_args.join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> MySqlConnCfg {
        MySqlConnCfg {
            host: "db.internal".into(),
            port: 3306,
            user: "backup".into(),
            password: Zeroizing::new("s3cr3t-should-not-leak".into()),
            database: "app".into(),
        }
    }

    #[test]
    fn t16_password_never_in_argv_or_flags() {
        // R10: 렌더된 dump_flags에도, conn_args(argv 조립원)에도 비밀번호 리터럴이 없다.
        let dump_args = vec!["--single-transaction".to_string(), "app".to_string()];
        let flags = render_dump_flags(Path::new("mysqldump"), &dump_args);
        assert!(!flags.contains("s3cr3t-should-not-leak"));
        assert!(flags.contains("--single-transaction"));
        assert!(flags.contains("<redacted>"));

        let c = cfg();
        let argv = conn_args(&c);
        assert!(argv.iter().all(|a| !a.contains("s3cr3t-should-not-leak")));
    }
}
