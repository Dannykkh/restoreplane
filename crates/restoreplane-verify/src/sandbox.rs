// S09 9.4 — 격리 샌드박스(C3-b). 일회용 Docker 컨테이너로만 실제 복원을 수행한다. 신뢰 불가
// 덤프는 호스트 바인드 마운트가 아니라 stdin 파이프로 넣고, 아웃바운드 차단(--network none) +
// 비특권(--cap-drop ALL, no-new-privileges) + 비슈퍼유저 role + PG --no-privileges + MySQL
// --local-infile=0 로 만약의 실행을 봉쇄한다. 실패/패닉 시에도 Drop에서 항상 docker rm -f.
//
// Docker/pg 런타임이 필요하므로 이 모듈을 실제로 구동하는 테스트는 전부 #[ignore]다. 구조는 컴파일된다.

use restoreplane_core::Engine;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::decrypt::PlaintextDump;
use crate::sqlscan::scan_dangerous_sql;

fn engine_str(e: &Engine) -> &'static str {
    match e {
        Engine::Postgresql => "postgresql",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Sqlserver => "sqlserver",
        Engine::Sqlite => "sqlite",
    }
}

/// "PostgreSQL 16.3" / "16.3" / "8.0.36" → 메이저 정수.
pub fn parse_major(server_version: &str) -> anyhow::Result<u32> {
    let digits: String = server_version
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse::<u32>().map_err(|_| {
        anyhow::anyhow!("server_version에서 메이저 버전을 파싱할 수 없음: {server_version}")
    })
}

/// 엔진+메이저 → Docker 이미지 태그(config에서 매핑).
pub struct ImagePolicy {
    pub map: std::collections::BTreeMap<(String, u32), String>,
}

impl ImagePolicy {
    pub fn new() -> Self {
        Self {
            map: std::collections::BTreeMap::new(),
        }
    }
    pub fn with(mut self, engine: &str, major: u32, image: &str) -> Self {
        self.map
            .insert((engine.to_string(), major), image.to_string());
        self
    }
}

impl Default for ImagePolicy {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SandboxSpec {
    pub image: String,
    pub engine: String,
    pub server_major: u32,
    pub container_name: String,
    pub db_name: String,
    pub restricted_role: String,
}

impl SandboxSpec {
    /// 엔진/버전/DB명 + 이미지 정책으로 스펙 구성. server_version이 없으면 이미지 매핑 불가 → Err.
    pub fn build(
        engine: Engine,
        server_version: Option<&str>,
        db_name: &str,
        policy: &ImagePolicy,
    ) -> anyhow::Result<Self> {
        let ver = server_version.ok_or_else(|| {
            anyhow::anyhow!("server_version이 없어 샌드박스 이미지를 선택할 수 없음(L3 skip 대상)")
        })?;
        let major = parse_major(ver)?;
        let eng = engine_str(&engine).to_string();
        let image = policy
            .map
            .get(&(eng.clone(), major))
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "server_version({ver} major={major})에 매칭되는 Docker 이미지가 없음 — \
                 버전 갭은 restore_target_version에 기록됨"
                )
            })?;
        Ok(Self {
            image,
            engine: eng,
            server_major: major,
            container_name: format!("restoreplane-verify-{}", uuid::Uuid::new_v4()),
            db_name: db_name.to_string(),
            restricted_role: "restore_target".to_string(),
        })
    }
}

struct ExecOut {
    success: bool,
    stdout: Vec<u8>,
    #[allow(dead_code)]
    stderr: Vec<u8>,
}

impl ExecOut {
    fn ok_or_ctx(self, ctx: &str) -> anyhow::Result<()> {
        if self.success {
            Ok(())
        } else {
            anyhow::bail!("{ctx}: {}", String::from_utf8_lossy(&self.stderr))
        }
    }
}

pub struct Sandbox {
    docker: std::path::PathBuf,
    spec: SandboxSpec,
    removed: bool,
}

impl Sandbox {
    /// 봉쇄 플래그와 함께 컨테이너를 기동한다(C3). 데이터 디렉토리는 tmpfs, 호스트 마운트 없음.
    pub async fn launch(docker: &std::path::Path, spec: SandboxSpec) -> anyhow::Result<Self> {
        let mut cmd = Command::new(docker);
        cmd.arg("run")
            .arg("-d")
            .arg("--rm")
            .args(["--name", &spec.container_name])
            .args(["--network", "none"]) // 아웃바운드 차단
            .args(["--cap-drop", "ALL"])
            .args(["--security-opt", "no-new-privileges"])
            .args(["--pids-limit", "512"])
            .args(["--memory", "2g"]);

        match spec.engine.as_str() {
            "postgresql" => {
                cmd.args(["--tmpfs", "/var/lib/postgresql/data:rw,noexec,nosuid"])
                    .args(["-e", "POSTGRES_PASSWORD=verify_only_ephemeral"])
                    .args(["-e", &format!("POSTGRES_DB={}", spec.db_name)]);
            }
            "mysql" | "mariadb" => {
                cmd.args(["--tmpfs", "/var/lib/mysql:rw,noexec,nosuid"])
                    .args(["-e", "MYSQL_ROOT_PASSWORD=verify_only_ephemeral"])
                    .args(["-e", &format!("MYSQL_DATABASE={}", spec.db_name)]);
            }
            other => anyhow::bail!("알 수 없는 엔진: {other}"),
        }
        cmd.arg(&spec.image);
        if matches!(spec.engine.as_str(), "mysql" | "mariadb") {
            // 서버측 LOAD DATA LOCAL 무력화(FR5).
            cmd.args(["--local-infile=0", "--skip-symbolic-links"]);
        }

        let status = cmd.status().await?;
        anyhow::ensure!(status.success(), "docker run 실패");

        let sb = Self {
            docker: docker.to_path_buf(),
            spec,
            removed: false,
        };
        if matches!(sb.spec.engine.as_str(), "mysql" | "mariadb") {
            sb.prepare_mysql_client_config().await?;
        }
        sb.wait_ready().await?;
        sb.prepare_restricted_role().await?;
        Ok(sb)
    }

    async fn prepare_mysql_client_config(&self) -> anyhow::Result<()> {
        self.exec(
            "sh",
            &[
                "-c",
                "umask 077; echo [client] > /tmp/restoreplane-client.cnf; \
                 echo user=root >> /tmp/restoreplane-client.cnf; \
                 printf password= >> /tmp/restoreplane-client.cnf; \
                 printenv MYSQL_ROOT_PASSWORD >> /tmp/restoreplane-client.cnf",
            ],
        )
        .await?
        .ok_or_ctx("MySQL-family client credential file creation failed")
    }

    async fn wait_ready(&self) -> anyhow::Result<()> {
        let (bin, args): (&str, Vec<&str>) = match self.spec.engine.as_str() {
            "postgresql" => ("pg_isready", vec!["-q"]),
            engine => (
                admin_tool_for(engine)?,
                vec![
                    "--defaults-extra-file=/tmp/restoreplane-client.cnf",
                    "ping",
                    "--silent",
                ],
            ),
        };
        for _ in 0..60 {
            let ok = self
                .exec(bin, &args)
                .await
                .map(|o| o.success)
                .unwrap_or(false);
            if ok {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        anyhow::bail!("샌드박스 DB가 준비되지 않음(timeout)")
    }

    async fn prepare_restricted_role(&self) -> anyhow::Result<()> {
        if self.spec.engine == "postgresql" {
            // 비슈퍼유저. COPY FROM PROGRAM은 pg_execute_server_program 권한이 필요한데 부여하지 않음.
            let sql = format!(
                "CREATE ROLE {r} LOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB;",
                r = self.spec.restricted_role
            );
            self.exec(
                "psql",
                &["-U", "postgres", "-d", &self.spec.db_name, "-c", &sql],
            )
            .await?
            .ok_or_ctx("restricted role 생성 실패")?;
        }
        Ok(())
    }

    /// PG globals(plain SQL) 적용. 위험 SQL 사전 스캔 후 비대화 psql로만 넘긴다(대화형 psql 금지).
    pub async fn apply_globals_sql(&self, plain: PlaintextDump) -> anyhow::Result<()> {
        let buf = read_stream(plain)?;
        scan_dangerous_sql(&buf)?;
        self.exec_with_stdin(
            "psql",
            &[
                "-v",
                "ON_ERROR_STOP=1",
                "--no-psqlrc",
                "-U",
                "postgres",
                "-d",
                &self.spec.db_name,
            ],
            &buf,
        )
        .await?
        .ok_or_ctx("globals 적용 실패")
    }

    /// DB 덤프 복원. custom은 pg_restore(비대화, 백슬래시 메타 미해석) + --no-privileges + 비슈퍼유저.
    pub async fn restore_db_dump(
        &self,
        plain: PlaintextDump,
        dump_format: &str,
    ) -> anyhow::Result<()> {
        match self.spec.engine.as_str() {
            "postgresql" => match dump_format {
                "custom" => {
                    let args = [
                        "-U",
                        &self.spec.restricted_role,
                        "-d",
                        &self.spec.db_name,
                        "--no-privileges",
                        "--no-owner",
                        "--single-transaction",
                        "--exit-on-error",
                    ];
                    let bytes = read_stream(plain)?;
                    self.exec_with_stdin("pg_restore", &args, &bytes)
                        .await?
                        .ok_or_ctx("pg_restore 실패")
                }
                "plain" => {
                    let bytes = read_stream(plain)?;
                    scan_dangerous_sql(&bytes)?;
                    let args = [
                        "-v",
                        "ON_ERROR_STOP=1",
                        "--no-psqlrc",
                        "-U",
                        &self.spec.restricted_role,
                        "-d",
                        &self.spec.db_name,
                    ];
                    self.exec_with_stdin("psql", &args, &bytes)
                        .await?
                        .ok_or_ctx("psql plain 복원 실패")
                }
                other => anyhow::bail!("알 수 없는 dump_format: {other}"),
            },
            "mysql" | "mariadb" => {
                let bytes = read_stream(plain)?;
                scan_dangerous_sql(&bytes)?;
                let args = [
                    "--defaults-extra-file=/tmp/restoreplane-client.cnf",
                    "--local-infile=0",
                    &self.spec.db_name,
                ];
                self.exec_with_stdin(sql_client_for(&self.spec.engine)?, &args, &bytes)
                    .await?
                    .ok_or_ctx("mysql 복원 실패")
            }
            other => anyhow::bail!("알 수 없는 엔진: {other}"),
        }
    }

    pub async fn server_version(&self) -> anyhow::Result<String> {
        let sql = if self.spec.engine == "postgresql" {
            "SHOW server_version"
        } else {
            "SELECT VERSION()"
        };
        Ok(self.query_scalar(sql).await?.trim().to_string())
    }

    /// 복원본 --schema-only 덤프(정규화 전 원본). asserts.rs가 정규화+해시한다.
    pub async fn dump_schema_only(&self) -> anyhow::Result<String> {
        let out = if self.spec.engine == "postgresql" {
            self.exec(
                "pg_dump",
                &["-s", "-U", "postgres", "-d", &self.spec.db_name],
            )
            .await?
        } else {
            self.exec(
                schema_dump_tool_for(&self.spec.engine)?,
                &[
                    "--defaults-extra-file=/tmp/restoreplane-client.cnf",
                    "--no-data",
                    &self.spec.db_name,
                ],
            )
            .await?
        };
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// 복원본 각 테이블의 실제 row count(정보 스키마 순회). 슬라이스 1은 PG만 간이 구현.
    pub async fn count_rows_per_table(
        &self,
    ) -> anyhow::Result<std::collections::BTreeMap<String, u64>> {
        let mut out = std::collections::BTreeMap::new();
        if self.spec.engine == "postgresql" {
            let tables = self
                .query_scalar(
                    "SELECT string_agg(tablename, ',') FROM pg_tables WHERE schemaname='public'",
                )
                .await?;
            for t in tables.trim().split(',').filter(|s| !s.is_empty()) {
                let c = self
                    .query_scalar(&format!("SELECT count(*) FROM \"{t}\""))
                    .await?;
                out.insert(t.to_string(), c.trim().parse::<u64>().unwrap_or(0));
            }
        } else {
            let tables = self
                .query_scalar(
                    "SELECT GROUP_CONCAT(TABLE_NAME ORDER BY TABLE_NAME) \
                     FROM information_schema.TABLES \
                     WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE'",
                )
                .await?;
            for table in tables.trim().split(',').filter(|name| !name.is_empty()) {
                let quoted = table.replace('`', "``");
                let count = self
                    .query_scalar(&format!("SELECT COUNT(*) FROM `{quoted}`"))
                    .await?;
                out.insert(
                    table.to_owned(),
                    count.trim().parse::<u64>().map_err(|error| {
                        anyhow::anyhow!("invalid restored row count for {table}: {error}")
                    })?,
                );
            }
        }
        Ok(out)
    }

    async fn exec(&self, bin: &str, args: &[&str]) -> anyhow::Result<ExecOut> {
        let mut c = Command::new(&self.docker);
        c.arg("exec")
            .arg(&self.spec.container_name)
            .arg(bin)
            .args(args);
        let out = c.output().await?;
        Ok(ExecOut {
            success: out.status.success(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    async fn exec_with_stdin(
        &self,
        bin: &str,
        args: &[&str],
        stdin: &[u8],
    ) -> anyhow::Result<ExecOut> {
        let mut c = Command::new(&self.docker);
        c.arg("exec")
            .arg("-i")
            .arg(&self.spec.container_name)
            .arg(bin)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = c.spawn()?;
        if let Some(mut si) = child.stdin.take() {
            si.write_all(stdin).await?;
            // stdin drop으로 EOF 전달.
        }
        let out = child.wait_with_output().await?;
        Ok(ExecOut {
            success: out.status.success(),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }

    async fn query_scalar(&self, sql: &str) -> anyhow::Result<String> {
        let out = if self.spec.engine == "postgresql" {
            self.exec(
                "psql",
                &[
                    "-tAX",
                    "-U",
                    "postgres",
                    "-d",
                    &self.spec.db_name,
                    "-c",
                    sql,
                ],
            )
            .await?
        } else {
            self.exec(
                sql_client_for(&self.spec.engine)?,
                &[
                    "--defaults-extra-file=/tmp/restoreplane-client.cnf",
                    "-N",
                    "-B",
                    "-e",
                    sql,
                    &self.spec.db_name,
                ],
            )
            .await?
        };
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if !self.removed {
            // 항상 강제 제거(봉쇄). Drop에서 async 불가하므로 blocking으로.
            let _ = std::process::Command::new(&self.docker)
                .args(["rm", "-f", &self.spec.container_name])
                .status();
        }
    }
}

fn read_stream(p: PlaintextDump) -> anyhow::Result<Vec<u8>> {
    match p {
        PlaintextDump::Bytes(b) => Ok(b),
        PlaintextDump::Directory(_) => anyhow::bail!("스트림이 필요한데 디렉토리 포맷"),
    }
}

fn sql_client_for(engine: &str) -> anyhow::Result<&'static str> {
    match engine {
        "mysql" => Ok("mysql"),
        "mariadb" => Ok("mariadb"),
        other => anyhow::bail!("SQL client is unavailable for engine: {other}"),
    }
}

fn schema_dump_tool_for(engine: &str) -> anyhow::Result<&'static str> {
    match engine {
        "mysql" => Ok("mysqldump"),
        "mariadb" => Ok("mariadb-dump"),
        other => anyhow::bail!("schema dump tool is unavailable for engine: {other}"),
    }
}

fn admin_tool_for(engine: &str) -> anyhow::Result<&'static str> {
    match engine {
        "mysql" => Ok("mysqladmin"),
        "mariadb" => Ok("mariadb-admin"),
        other => anyhow::bail!("admin tool is unavailable for engine: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mariadb_sandbox_never_falls_back_to_mysql_tool_names() {
        assert_eq!(sql_client_for("mariadb").unwrap(), "mariadb");
        assert_eq!(schema_dump_tool_for("mariadb").unwrap(), "mariadb-dump");
        assert_eq!(admin_tool_for("mariadb").unwrap(), "mariadb-admin");
        assert_eq!(sql_client_for("mysql").unwrap(), "mysql");
        assert_eq!(schema_dump_tool_for("mysql").unwrap(), "mysqldump");
        assert_eq!(admin_tool_for("mysql").unwrap(), "mysqladmin");
    }
}
