// crates/restoreplane-source-mysql/src/dump.rs
//! S04 §7 — 자식 프로세스 기동. tokio::process로 mysqldump를 구동한다(source-pg dump.rs와 평행).
//! R10: 비밀번호는 MYSQL_PWD env로만 전달(argv/flags 문자열에 절대 노출 금지 — ps/작업관리자 평문 차단).
//! stderr 전량 드레인 + 마지막 N줄만 보존(파이프 백프레셔 데드락 방지).

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::path::Path;

#[cfg(windows)]
use restoreplane_runtime::{
    hash_executable, LaunchSpec, ProcessSupervisor, SupervisedStreamingProcess,
};
use tokio::io::AsyncRead;
use zeroize::Zeroizing;

use crate::binary::ClientKind;

const STDERR_TAIL_LINES: usize = 200;

pub struct MySqlConnCfg {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 값 자체는 S01 Config가 관리. S02 규율과 동일하게 zeroize.
    pub password: Zeroizing<String>,
    pub database: String,
    pub tls_mode: String,
}

/// 연결 인자(비밀번호 제외). dump_flags 매니페스트 표기와 실제 argv에서 공유한다.
fn conn_args(conn: &MySqlConnCfg, client_kind: ClientKind) -> anyhow::Result<Vec<String>> {
    let mut args = vec![
        format!("--host={}", conn.host),
        format!("--port={}", conn.port),
        format!("--user={}", conn.user),
    ];
    args.extend(tls_args(client_kind, &conn.tls_mode)?);
    Ok(args)
}

/// mysqldump 자식 프로세스를 기동한다. 비밀번호는 오직 MYSQL_PWD env로만 전달(R10).
/// dump_args는 flags::build_flags 결과 + DB명. 프로세스 시작 자체는 non-blocking.
pub fn spawn_mysqldump(
    binary: &Path,
    trusted_sha256: Option<[u8; 32]>,
    trusted_dependencies: &[(std::path::PathBuf, [u8; 32])],
    dump_args: &[String],
    conn: &MySqlConnCfg,
    client_kind: ClientKind,
) -> anyhow::Result<SupervisedStreamingProcess> {
    #[cfg(not(windows))]
    {
        let _ = (
            binary,
            trusted_sha256,
            trusted_dependencies,
            dump_args,
            conn,
            client_kind,
        );
        anyhow::bail!("NATIVE_PROCESS_SUPERVISION_REQUIRES_WINDOWS");
    }

    #[cfg(windows)]
    {
        let mut arguments = conn_args(conn, client_kind)?;
        arguments.extend(dump_args.iter().cloned());
        let expected_sha256 = match trusted_sha256 {
            Some(expected_sha256) => expected_sha256,
            None => hash_executable(binary)?,
        };
        let working_directory = binary
            .parent()
            .ok_or_else(|| anyhow::anyhow!("mysqldump has no parent directory"))?;
        let spec = LaunchSpec::new(
            binary.to_path_buf(),
            working_directory.to_path_buf(),
            expected_sha256,
        )?
        .with_args(arguments)?
        .with_environment(inherited_environment([(
            "MYSQL_PWD",
            conn.password.as_str(),
        )]))?
        .with_trusted_dependencies(trusted_dependencies.iter().cloned())?
        .with_redacted_output_literals([conn.password.as_bytes()])?;
        ProcessSupervisor::launch_streaming(&spec).map_err(Into::into)
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

/// Drains stderr continuously while retaining only the newest bounded line tail.
pub async fn drain_stderr_tail(stderr: impl AsyncRead + Unpin) -> String {
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
pub fn render_dump_flags(
    binary: &Path,
    dump_args: &[String],
    conn: &MySqlConnCfg,
    client_kind: ClientKind,
) -> anyhow::Result<String> {
    let tls = tls_args(client_kind, &conn.tls_mode)?.join(" ");
    Ok(format!(
        "{} --host=<redacted> --port=<redacted> --user=<redacted> {} {}",
        binary.display(),
        tls,
        dump_args.join(" ")
    ))
}

pub fn tls_args(client_kind: ClientKind, tls_mode: &str) -> anyhow::Result<Vec<String>> {
    match (client_kind, tls_mode) {
        (ClientKind::OracleMySql, "disabled") => Ok(vec!["--ssl-mode=DISABLED".to_owned()]),
        (ClientKind::OracleMySql, "preferred") => Ok(vec!["--ssl-mode=PREFERRED".to_owned()]),
        (ClientKind::OracleMySql, "required") => Ok(vec!["--ssl-mode=REQUIRED".to_owned()]),
        (ClientKind::MariaDb, "disabled") => Ok(vec!["--disable-ssl".to_owned()]),
        (ClientKind::MariaDb, "preferred") => Ok(Vec::new()),
        (ClientKind::MariaDb, "required") => Ok(vec!["--ssl".to_owned()]),
        (_, "verify_ca" | "verify_full") => anyhow::bail!("MYSQL_TLS_CA_REQUIRED"),
        (_, _) => anyhow::bail!("MYSQL_TLS_MODE_UNSUPPORTED"),
    }
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
            tls_mode: "preferred".into(),
        }
    }

    #[test]
    fn t16_password_never_in_argv_or_flags() {
        // R10: 렌더된 dump_flags에도, conn_args(argv 조립원)에도 비밀번호 리터럴이 없다.
        let dump_args = vec!["--single-transaction".to_string(), "app".to_string()];
        let flags = render_dump_flags(
            Path::new("mysqldump"),
            &dump_args,
            &cfg(),
            ClientKind::OracleMySql,
        )
        .unwrap();
        assert!(!flags.contains("s3cr3t-should-not-leak"));
        assert!(flags.contains("--single-transaction"));
        assert!(flags.contains("<redacted>"));

        let c = cfg();
        let argv = conn_args(&c, ClientKind::OracleMySql).unwrap();
        assert!(argv.iter().all(|a| !a.contains("s3cr3t-should-not-leak")));
    }

    #[test]
    fn tls_modes_are_explicit_for_each_client_family() {
        assert_eq!(
            tls_args(ClientKind::OracleMySql, "required").unwrap(),
            ["--ssl-mode=REQUIRED"]
        );
        assert_eq!(
            tls_args(ClientKind::MariaDb, "disabled").unwrap(),
            ["--disable-ssl"]
        );
        assert_eq!(
            tls_args(ClientKind::MariaDb, "required").unwrap(),
            ["--ssl"]
        );
        assert!(tls_args(ClientKind::OracleMySql, "verify_full").is_err());
    }
}
