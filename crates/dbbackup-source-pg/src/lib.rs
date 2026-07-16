// crates/dbbackup-source-pg/src/lib.rs
//! S03 — PostgresSource. BackupSource(dbbackup-pipeline)를 구현한다.
//! plan(): 매 run 서버 버전 재검증(하드 실패 게이트) + DB목록/확장 캡처 + globals/db 파트 조립.
//! spawn(): tokio::process로 pg_dump/pg_dumpall 구동(dump.rs).
//! 런타임엔 실제 PG가 필요하지만, version/matrix/flags 로직은 DB 없이 테스트된다.

pub mod dump;
pub mod version;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dbbackup_pipeline::artifact::DumpArtifact;
use dbbackup_pipeline::source::{
    BackupSource, DumpPartKind, DumpPlan, DumpPlanPart, ExtensionInfo, PlanWarning,
};
use tokio_postgres::Client;
use uuid::Uuid;
use zeroize::Zeroizing;

use version::{probe_client_version, BundledToolMatrix, ToolVersion};

#[derive(thiserror::Error, Debug)]
pub enum PgSourceError {
    #[error("DB 접속 실패({database}): {source}")]
    ConnectionFailed { database: String, source: tokio_postgres::Error },
    #[error("버전 조회 쿼리 실패: {0}")]
    VersionQueryFailed(#[source] tokio_postgres::Error),
    #[error("버전 문자열 파싱 실패: {0:?}")]
    VersionParseFailed(String),
    #[error("카탈로그 조회 실패: {0}")]
    CatalogQueryFailed(#[source] tokio_postgres::Error),
    #[error("서버 메이저 버전 {server_major}을(를) 지원하는 번들 클라이언트가 없음(PG13~17 확인)")]
    NoBundledClientForServer { server_major: u32 },
    #[error("클라이언트 메이저({client_major}) < 서버 메이저({server_major}) — 하드 실패(DT §1.6)")]
    ClientTooOld { client_major: u32, server_major: u32 },
    #[error("덤프 도구 실행 실패({path}): {source}")]
    DumpToolSpawnFailed { path: std::path::PathBuf, source: std::io::Error },
}

pub struct PgConnCfg {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Zeroizing<String>,
    pub admin_database: String,
}

pub struct PostgresSource {
    pub job: String,
    pub alias: String,
    pub engine_alias: String,
    pub conn: PgConnCfg,
    pub registered_databases: Vec<String>,
    pub matrix: BundledToolMatrix,
}

#[derive(Clone, Debug)]
pub struct ClusterVersionProbe {
    pub server: ToolVersion,
    pub client: ToolVersion,
    pub detected_at: DateTime<Utc>,
}

impl PostgresSource {
    async fn connect(&self, database: &str) -> Result<Client, PgSourceError> {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&self.conn.host)
            .port(self.conn.port)
            .user(&self.conn.user)
            .password(self.conn.password.as_str())
            .dbname(database)
            .application_name("dbbackup-probe");
        let (client, connection) = cfg
            .connect(tokio_postgres::NoTls)
            .await
            .map_err(|e| PgSourceError::ConnectionFailed {
                database: database.to_string(),
                source: e,
            })?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!(error = %e, "postgres probe connection terminated");
            }
        });
        Ok(client)
    }
}

async fn probe_server_version(client: &Client) -> Result<ToolVersion, PgSourceError> {
    let row = client
        .query_one("SELECT version(), current_setting('server_version_num')", &[])
        .await
        .map_err(PgSourceError::VersionQueryFailed)?;
    let raw: String = row.get(0);
    let num_str: String = row.get(1);
    let num: u32 = num_str
        .parse()
        .map_err(|_| PgSourceError::VersionParseFailed(num_str.clone()))?;
    Ok(ToolVersion { major: num / 10000, raw })
}

async fn list_live_databases(client: &Client) -> Result<Vec<String>, PgSourceError> {
    let rows = client
        .query(
            "SELECT datname FROM pg_database WHERE datistemplate = false AND datallowconn = true",
            &[],
        )
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    Ok(rows.into_iter().map(|r| r.get(0)).collect())
}

/// 중요: pg_extension은 DB별 카탈로그 — 반드시 대상 DB 커넥션에서 조회한다(R6 gotcha).
async fn capture_extensions(db_client: &Client) -> Result<Vec<ExtensionInfo>, PgSourceError> {
    let rows = db_client
        .query("SELECT extname, extversion FROM pg_extension ORDER BY extname", &[])
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    Ok(rows
        .into_iter()
        .map(|r| ExtensionInfo { name: r.get(0), version: r.get(1) })
        .collect())
}

async fn capture_encoding(db_client: &Client) -> Result<(String, String), PgSourceError> {
    let row = db_client
        .query_one(
            "SELECT current_setting('server_encoding'), datcollate \
             FROM pg_database WHERE datname = current_database()",
            &[],
        )
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    Ok((row.get(0), row.get(1)))
}

#[async_trait]
impl BackupSource for PostgresSource {
    async fn plan(&self) -> anyhow::Result<DumpPlan> {
        let admin = self.connect(&self.conn.admin_database).await?;

        // R4/R5: 버전 재검증이 어떤 파트 생성보다 먼저 게이트로 선다.
        let server = probe_server_version(&admin).await?;
        let pg_dump_path = self.matrix.select_pg_dump(server.major)?;
        let pg_dumpall_path = self.matrix.select_pg_dumpall(server.major)?;
        let client = probe_client_version(&pg_dump_path).await?;
        if client.major < server.major {
            return Err(PgSourceError::ClientTooOld {
                client_major: client.major,
                server_major: server.major,
            }
            .into());
        }
        let probe = ClusterVersionProbe { server, client, detected_at: Utc::now() };
        tracing::info!(server = %probe.server.raw, client = %probe.client.raw, "postgres version probe ok");

        // R7: 실제 DB 목록 vs 등록 목록 드리프트 감지.
        let live_dbs = list_live_databases(&admin).await?;
        let mut warnings = Vec::new();
        for db in &live_dbs {
            if !self.registered_databases.iter().any(|r| r == db) {
                warnings.push(PlanWarning::UnregisteredDatabase(db.clone()));
            }
        }

        // R3: globals 파트.
        let globals_backup_id = Uuid::new_v4();
        let globals_part = DumpPlanPart {
            backup_id: globals_backup_id,
            job: self.job.clone(),
            source_alias: self.alias.clone(),
            kind: DumpPartKind::Globals,
            pairs_with_globals_backup_id: None,
            dump_tool: "pg_dumpall",
            dump_tool_path: pg_dumpall_path,
            dump_tool_version: probe.client.raw.clone(),
            dump_format: "plain",
            dump_flags: dump::render_globals_flags(&self.conn),
            extensions: Vec::new(),
            charset: None,
            collation: None,
        };

        // DB별 파트.
        let mut db_parts = Vec::with_capacity(self.registered_databases.len());
        for db in &self.registered_databases {
            if !live_dbs.iter().any(|d| d == db) {
                warnings.push(PlanWarning::RegisteredDatabaseMissing(db.clone()));
                continue;
            }
            let db_client = self.connect(db).await?;
            let extensions = capture_extensions(&db_client).await?;
            let (charset, collation) = capture_encoding(&db_client).await?;
            db_parts.push(DumpPlanPart {
                backup_id: Uuid::new_v4(),
                job: self.job.clone(),
                source_alias: self.alias.clone(),
                kind: DumpPartKind::Database { name: db.clone() },
                pairs_with_globals_backup_id: Some(globals_backup_id),
                dump_tool: "pg_dump",
                dump_tool_path: pg_dump_path.clone(),
                dump_tool_version: probe.client.raw.clone(),
                dump_format: "custom",
                dump_flags: dump::render_db_flags(&self.conn, db),
                extensions,
                charset: Some(charset),
                collation: Some(collation),
            });
        }

        for w in &warnings {
            match w {
                PlanWarning::UnregisteredDatabase(db) => {
                    tracing::warn!(db, "서버에 존재하지만 add-db 미등록 — 백업에서 누락")
                }
                PlanWarning::RegisteredDatabaseMissing(db) => {
                    tracing::warn!(db, "등록됐지만 서버에서 조회 안 됨 — 삭제/오탈자 확인")
                }
            }
        }

        Ok(DumpPlan { globals_part: Some(globals_part), db_parts, warnings })
    }

    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
        dump::spawn_part(&self.conn, part)
    }
}
