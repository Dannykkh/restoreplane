// crates/dbbackup-source-mysql/src/meta.rs
//! S04 §Reference Libraries — 런타임 메타 조회 seam. section-S04.md는 sqlx MySqlPool을 제안하지만,
//! "번들 heavy dep 최소화" 제약에 맞춰 sqlx를 도입하지 않고 MySqlMetaConn 트레이트로 추상화한다
//! (섹션 Risk & Rollback가 명시한 교체 가능 seam). 구체 구현 MysqlCliMetaConn은 tokio::process로
//! `mysql` 클라이언트를 구동해 탭 구분 출력을 파싱한다 — 크레이트 전체가 "CLI를 shell한다"는 결과
//! 지향과 일관되고 새 컴파일 의존이 없다. 실제 서버가 필요한 경로라 유닛 테스트는 하지 않는다.

use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::Stdio;
#[cfg(windows)]
use std::time::Duration;

use async_trait::async_trait;
use zeroize::Zeroizing;

use crate::binary::ClientKind;
use crate::dump::tls_args;
use crate::engine_scan::{FETCH_CHARSET_SQL, SCAN_TABLES_SQL};

/// 엔진 스캔/버전 감지/charset 조회에 필요한 최소 계약. sqlx ↔ mysql_async ↔ CLI 교체 지점.
#[async_trait]
pub trait MySqlMetaConn: Send + Sync {
    /// SELECT VERSION() 원문.
    async fn query_version(&self) -> anyhow::Result<String>;
    /// information_schema.tables 스캔 결과 (schema, table, engine) 행 목록.
    async fn scan_tables(&self, db_name: &str) -> anyhow::Result<Vec<(String, String, String)>>;
    /// (charset, collation).
    async fn fetch_charset(&self, db_name: &str) -> anyhow::Result<(String, String)>;
}

/// `mysql` CLI 기반 구현(런타임 전용). 비밀번호는 MYSQL_PWD env로만 전달한다(R10).
pub struct MysqlCliMetaConn {
    pub client: PathBuf, // `mysql` 실행 파일 경로
    /// Production sets this to the package-attested digest. It is checked before MYSQL_PWD
    /// is exposed to every metadata client process.
    pub expected_sha256: Option<[u8; 32]>,
    pub trusted_dependencies: Vec<(PathBuf, [u8; 32])>,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Zeroizing<String>,
    pub client_kind: ClientKind,
    pub tls_mode: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MySqlCatalogProbe {
    pub server_version: String,
    pub server_identity: String,
    pub databases: Vec<String>,
}

impl MysqlCliMetaConn {
    #[cfg(any(not(windows), test))]
    fn verify_client_identity(&self) -> anyhow::Result<()> {
        let Some(expected_sha256) = self.expected_sha256 else {
            return Ok(());
        };
        let actual_sha256 = dbbackup_runtime::hash_executable(&self.client)?;
        if actual_sha256 != expected_sha256 {
            anyhow::bail!("MYSQL_METADATA_CLIENT_TRUST_MISMATCH");
        }
        Ok(())
    }

    /// -N(헤더 없음) -B(탭 구분) -e "<sql>"로 조회하고 stdout을 반환한다.
    /// 파라미터 바인딩이 없으므로 db_name은 호출부에서 검증된 식별자만 사용한다(system schema 거부 등).
    async fn run_sql(&self, sql: &str) -> anyhow::Result<String> {
        let tls = tls_args(self.client_kind, &self.tls_mode)?;
        #[cfg(windows)]
        {
            return self.run_sql_supervised(sql, tls).await;
        }

        #[cfg(not(windows))]
        {
            self.verify_client_identity()?;
            let out = tokio::process::Command::new(&self.client)
                .arg(format!("--host={}", self.host))
                .arg(format!("--port={}", self.port))
                .arg(format!("--user={}", self.user))
                .args(tls)
                .args(["-N", "-B", "-e", sql])
                .env("MYSQL_PWD", self.password.as_str())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| {
                    anyhow::anyhow!("mysql 클라이언트 실행 실패({}): {e}", self.client.display())
                })?;
            if !out.status.success() {
                anyhow::bail!(
                    "mysql 메타 조회 실패(exit={:?}): {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    }

    #[cfg(windows)]
    async fn run_sql_supervised(&self, sql: &str, tls: Vec<String>) -> anyhow::Result<String> {
        use std::collections::BTreeMap;
        use std::ffi::OsString;

        use dbbackup_runtime::{hash_executable, LaunchSpec, ProcessSupervisor};

        const MAX_METADATA_OUTPUT_BYTES: usize = 1024 * 1024;
        const METADATA_TIMEOUT: Duration = Duration::from_secs(60);

        let client = self.client.clone();
        let dependencies = self.trusted_dependencies.clone();
        let expected_sha256 = match self.expected_sha256 {
            Some(expected) => expected,
            None => hash_executable(&client)?,
        };
        let host = self.host.clone();
        let port = self.port;
        let user = self.user.clone();
        let password = self.password.clone();
        let sql = sql.to_owned();
        tokio::task::spawn_blocking(move || {
            let working_directory = client
                .parent()
                .ok_or_else(|| anyhow::anyhow!("MYSQL_METADATA_CLIENT_TRUST_MISMATCH"))?;
            let mut args = vec![
                format!("--host={host}"),
                format!("--port={port}"),
                format!("--user={user}"),
            ];
            args.extend(tls);
            args.extend(["-N".to_owned(), "-B".to_owned(), "-e".to_owned(), sql]);

            let mut environment = BTreeMap::<OsString, OsString>::new();
            for key in ["SystemRoot", "WINDIR", "TEMP", "TMP"] {
                if let Some(value) = std::env::var_os(key) {
                    environment.insert(OsString::from(key), value);
                }
            }
            environment.insert(
                OsString::from("MYSQL_PWD"),
                OsString::from(password.as_str()),
            );

            let spec = LaunchSpec::new(
                client.clone(),
                working_directory.to_path_buf(),
                expected_sha256,
            )?
            .with_args(args)?
            .with_environment(environment)?
            .with_trusted_dependencies(dependencies)?
            .with_output_tail_bytes(MAX_METADATA_OUTPUT_BYTES)?
            .with_redacted_output_literals([password.as_bytes()])?;
            let mut process = ProcessSupervisor::launch(&spec)?;
            let Some(exit) = process.wait_timeout(METADATA_TIMEOUT)? else {
                let _ = process.terminate();
                anyhow::bail!("MYSQL_METADATA_TIMEOUT");
            };
            if exit.stdout().truncated() || exit.stderr().truncated() {
                anyhow::bail!("MYSQL_METADATA_OUTPUT_LIMIT_EXCEEDED");
            }
            if exit.exit_code() != 0 {
                anyhow::bail!(
                    "mysql metadata query failed (exit={}): {}",
                    exit.exit_code(),
                    String::from_utf8_lossy(exit.stderr().tail()).trim()
                );
            }
            Ok(String::from_utf8_lossy(exit.stdout().tail()).into_owned())
        })
        .await
        .map_err(|_| anyhow::anyhow!("MYSQL_METADATA_CLIENT_TASK_FAILED"))?
    }

    pub async fn probe_catalog(&self) -> anyhow::Result<MySqlCatalogProbe> {
        let identity_expression = match self.client_kind {
            ClientKind::OracleMySql => "@@GLOBAL.server_uuid",
            ClientKind::MariaDb => "@@GLOBAL.server_uid",
        };
        let header = self
            .run_sql(&format!("SELECT VERSION(), {identity_expression}"))
            .await?;
        let (server_version, server_identity) =
            parse_catalog_probe_header(self.client_kind, &header)?;
        let output = self
            .run_sql(
                "SELECT schema_name \
                 FROM information_schema.schemata \
                 ORDER BY schema_name",
            )
            .await?;
        let databases = output
            .lines()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(MySqlCatalogProbe {
            server_version,
            server_identity,
            databases,
        })
    }

    pub async fn list_backup_tables(
        &self,
        db_name: &str,
    ) -> anyhow::Result<Vec<(String, String, String)>> {
        let output = self
            .run_sql(&inline_db(
                "SELECT table_schema, table_name, COALESCE(engine, '') \
                 FROM information_schema.tables \
                 WHERE table_schema = ? AND table_type = 'BASE TABLE' \
                 ORDER BY table_schema, table_name",
                db_name,
            ))
            .await?;
        Ok(output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| {
                let mut fields = line.split('\t');
                Some((
                    fields.next()?.to_owned(),
                    fields.next()?.to_owned(),
                    fields.next().unwrap_or_default().to_owned(),
                ))
            })
            .collect())
    }
}

fn parse_catalog_probe_header(
    client_kind: ClientKind,
    output: &str,
) -> anyhow::Result<(String, String)> {
    let line = output.trim();
    if line.is_empty() || line.contains(['\r', '\n']) {
        anyhow::bail!("MYSQL_SERVER_IDENTITY_INVALID");
    }
    let mut fields = line.split('\t');
    let version = fields.next().unwrap_or_default().trim();
    let identity = fields.next().unwrap_or_default().trim();
    if version.is_empty() || fields.next().is_some() {
        anyhow::bail!("MYSQL_SERVER_IDENTITY_INVALID");
    }
    let canonical = match client_kind {
        ClientKind::OracleMySql => {
            let parsed = uuid::Uuid::parse_str(identity)
                .map_err(|_| anyhow::anyhow!("MYSQL_SERVER_IDENTITY_INVALID"))?;
            if parsed.is_nil() {
                anyhow::bail!("MYSQL_SERVER_IDENTITY_INVALID");
            }
            format!("mysql-server-uuid:{}", parsed.hyphenated())
        }
        ClientKind::MariaDb => {
            if !(20..=64).contains(&identity.len())
                || !identity
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
            {
                anyhow::bail!("MYSQL_SERVER_IDENTITY_INVALID");
            }
            format!("mariadb-server-uid:{identity}")
        }
    };
    Ok((version.to_owned(), canonical))
}

/// SQL의 `?` 플레이스홀더를 리터럴 식별자로 치환한다(CLI는 prepared 바인딩이 없음).
/// db_name은 호출부(plan)에서 system schema 거부를 이미 통과한 식별자다.
fn inline_db(sql: &str, db_name: &str) -> String {
    sql.replacen('?', &format!("'{}'", db_name.replace('\'', "''")), 1)
}

#[async_trait]
impl MySqlMetaConn for MysqlCliMetaConn {
    async fn query_version(&self) -> anyhow::Result<String> {
        Ok(self.run_sql("SELECT VERSION()").await?.trim().to_string())
    }

    async fn scan_tables(&self, db_name: &str) -> anyhow::Result<Vec<(String, String, String)>> {
        let out = self.run_sql(&inline_db(SCAN_TABLES_SQL, db_name)).await?;
        Ok(out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let mut it = l.split('\t');
                Some((
                    it.next()?.to_string(),
                    it.next()?.to_string(),
                    it.next()?.to_string(),
                ))
            })
            .collect())
    }

    async fn fetch_charset(&self, db_name: &str) -> anyhow::Result<(String, String)> {
        let out = self.run_sql(&inline_db(FETCH_CHARSET_SQL, db_name)).await?;
        let line = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let mut it = line.split('\t');
        let charset = it.next().unwrap_or("").to_string();
        let collation = it.next().unwrap_or("").to_string();
        Ok((charset, collation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_db_substitutes_placeholder_and_escapes_quotes() {
        let sql = inline_db(SCAN_TABLES_SQL, "app");
        assert!(sql.contains("table_schema = 'app'"));
        assert!(!sql.contains("= ?"));
        // 작은따옴표 이스케이프.
        assert!(inline_db("WHERE x = ?", "o'brien").contains("'o''brien'"));
    }

    #[test]
    fn mysql_family_catalog_identity_is_canonical_and_engine_specific() {
        assert_eq!(
            parse_catalog_probe_header(
                ClientKind::OracleMySql,
                "8.4.5\t550e8400-e29b-41d4-a716-446655440000\n"
            )
            .expect("Oracle MySQL server UUID"),
            (
                "8.4.5".to_owned(),
                "mysql-server-uuid:550e8400-e29b-41d4-a716-446655440000".to_owned()
            )
        );
        assert_eq!(
            parse_catalog_probe_header(
                ClientKind::MariaDb,
                "11.8.3-MariaDB\tMDEyMzQ1Njc4OWFiY2RlZg==\n"
            )
            .expect("MariaDB immutable server UID"),
            (
                "11.8.3-MariaDB".to_owned(),
                "mariadb-server-uid:MDEyMzQ1Njc4OWFiY2RlZg==".to_owned()
            )
        );
        assert!(parse_catalog_probe_header(ClientKind::OracleMySql, "8.4.5\t\n").is_err());
        assert!(parse_catalog_probe_header(ClientKind::MariaDb, "11.8\tserver one\n").is_err());
    }

    #[test]
    fn attested_metadata_client_bytes_are_checked_before_command_execution() {
        let temp = tempfile::TempDir::new().expect("client fixture root");
        let client = temp
            .path()
            .join(if cfg!(windows) { "mysql.exe" } else { "mysql" });
        std::fs::write(&client, b"trusted-client").expect("client fixture");
        let client = std::fs::canonicalize(client).expect("canonical client");
        let expected_sha256 = dbbackup_runtime::hash_executable(&client).expect("client hash");
        let connection = MysqlCliMetaConn {
            client: client.clone(),
            expected_sha256: Some(expected_sha256),
            trusted_dependencies: Vec::new(),
            host: "127.0.0.1".to_owned(),
            port: 3306,
            user: "fixture".to_owned(),
            password: Zeroizing::new("secret".to_owned()),
            client_kind: ClientKind::OracleMySql,
            tls_mode: "preferred".to_owned(),
        };
        connection
            .verify_client_identity()
            .expect("unchanged trusted client");

        std::fs::write(&client, b"tampered-client").expect("mutate client fixture");
        assert_eq!(
            connection
                .verify_client_identity()
                .expect_err("tampered client must not receive credentials")
                .to_string(),
            "MYSQL_METADATA_CLIENT_TRUST_MISMATCH"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn metadata_password_is_not_exposed_when_supervisor_rejects_digest() {
        let connection = MysqlCliMetaConn {
            client: std::env::current_exe().expect("test executable"),
            expected_sha256: Some([0; 32]),
            trusted_dependencies: Vec::new(),
            host: "127.0.0.1".to_owned(),
            port: 3306,
            user: "fixture".to_owned(),
            password: Zeroizing::new("must-not-reach-child".to_owned()),
            client_kind: ClientKind::OracleMySql,
            tls_mode: "preferred".to_owned(),
        };
        let error = connection
            .query_version()
            .await
            .expect_err("mismatched package digest must block process resume");
        assert!(error.to_string().contains("PROCESS_EXECUTABLE_NOT_TRUSTED"));
    }
}
