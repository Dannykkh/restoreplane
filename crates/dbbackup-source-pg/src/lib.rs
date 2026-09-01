// crates/dbbackup-source-pg/src/lib.rs
//! S03 — PostgresSource. BackupSource(dbbackup-pipeline)를 구현한다.
//! plan(): 매 run 서버 버전 재검증(하드 실패 게이트) + DB목록/확장 캡처 + globals/db 파트 조립.
//! spawn(): tokio::process로 pg_dump/pg_dumpall 구동(dump.rs).
//! 런타임엔 실제 PG가 필요하지만, version/matrix/flags 로직은 DB 없이 테스트된다.

pub mod dump;
pub mod version;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use dbbackup_core::BackupScopeSelection;
use dbbackup_pipeline::artifact::DumpArtifact;
use dbbackup_pipeline::source::{
    BackupSource, DumpPartKind, DumpPlan, DumpPlanPart, ExtensionInfo, PlanWarning,
};
use tokio_postgres::Client;
use uuid::Uuid;
use zeroize::Zeroizing;

use version::{probe_client_version_with_identity, BundledToolMatrix, ToolVersion};

#[derive(thiserror::Error, Debug)]
pub enum PgSourceError {
    #[error("DB 접속 실패({database}): {source}")]
    ConnectionFailed {
        database: String,
        source: tokio_postgres::Error,
    },
    #[error("TLS DB 접속 실패({database}): {source}")]
    TlsConnectionFailed {
        database: String,
        source: tokio_postgres::Error,
    },
    #[error("TLS 설정 실패: {0}")]
    TlsConfigurationFailed(#[source] native_tls::Error),
    #[error("지원하지 않는 PostgreSQL TLS 모드: {0}")]
    TlsModeUnsupported(String),
    #[error("버전 조회 쿼리 실패: {0}")]
    VersionQueryFailed(#[source] tokio_postgres::Error),
    #[error("버전 문자열 파싱 실패: {0:?}")]
    VersionParseFailed(String),
    #[error("카탈로그 조회 실패: {0}")]
    CatalogQueryFailed(#[source] tokio_postgres::Error),
    #[error("서버 메이저 버전 {server_major}을(를) 지원하는 번들 클라이언트가 없음(PG14~18 확인)")]
    NoBundledClientForServer { server_major: u32 },
    #[error(
        "클라이언트 메이저({client_major}) < 서버 메이저({server_major}) — 하드 실패(DT §1.6)"
    )]
    ClientTooOld {
        client_major: u32,
        server_major: u32,
    },
    #[error("덤프 도구 실행 실패({path}): {source}")]
    DumpToolSpawnFailed {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("PostgreSQL dump tool trust verification failed: {path}")]
    DumpToolUntrusted { path: std::path::PathBuf },
}

pub struct PgConnCfg {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Zeroizing<String>,
    pub admin_database: String,
    pub tls_mode: String,
}

pub struct PostgresSource {
    pub job: String,
    pub alias: String,
    pub engine_alias: String,
    pub conn: PgConnCfg,
    pub registered_databases: Vec<String>,
    pub matrix: BundledToolMatrix,
}

pub struct ScopedPostgresSource {
    source: PostgresSource,
    selection: BackupScopeSelection,
}

#[derive(Clone, Debug)]
pub struct ClusterVersionProbe {
    pub server: ToolVersion,
    pub client: ToolVersion,
    pub detected_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgCatalogProbe {
    pub server_version: String,
    pub server_identity: String,
    pub databases: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgTableObservation {
    pub engine_object_id: String,
    pub schema_name: String,
    pub table_name: String,
    pub eligible: bool,
    pub exclusion_reason: Option<String>,
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
        connect_client(cfg, database, &self.conn.tls_mode).await
    }

    pub fn with_selection(self, selection: BackupScopeSelection) -> ScopedPostgresSource {
        ScopedPostgresSource {
            source: self,
            selection,
        }
    }
}

pub async fn probe_catalog(
    conn: &PgConnCfg,
    tls_mode: &str,
) -> Result<PgCatalogProbe, PgSourceError> {
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&conn.host)
        .port(conn.port)
        .user(&conn.user)
        .password(conn.password.as_str())
        .dbname(&conn.admin_database)
        .application_name("dbbackup-connection-inspector");

    let client = connect_client(cfg, &conn.admin_database, tls_mode).await?;
    inspect_client(&client).await
}

pub async fn probe_tables(
    conn: &PgConnCfg,
    database: &str,
    tls_mode: &str,
) -> Result<Vec<PgTableObservation>, PgSourceError> {
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(&conn.host)
        .port(conn.port)
        .user(&conn.user)
        .password(conn.password.as_str())
        .dbname(database)
        .application_name("dbbackup-scope-inspector");
    let client = connect_client(cfg, database, tls_mode).await?;
    let rows = client
        .query(
            "SELECT c.oid::text, n.nspname, c.relname, c.relkind::text
             FROM pg_catalog.pg_class AS c
             JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'p')
               AND n.nspname <> 'information_schema'
               AND n.nspname NOT LIKE 'pg\\_%' ESCAPE '\\'
             ORDER BY n.nspname, c.relname, c.oid",
            &[],
        )
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let oid: String = row.get(0);
            let schema_name: String = row.get(1);
            let table_name: String = row.get(2);
            let relation_kind: String = row.get(3);
            let eligible = relation_kind == "r";
            PgTableObservation {
                engine_object_id: format!("relation:{oid}"),
                schema_name,
                table_name,
                eligible,
                exclusion_reason: (!eligible).then(|| "partitioned parent".to_owned()),
            }
        })
        .collect())
}

fn native_tls_connector(tls_mode: &str) -> Result<native_tls::TlsConnector, PgSourceError> {
    let mut builder = native_tls::TlsConnector::builder();
    match tls_mode {
        "required" => {
            builder.danger_accept_invalid_certs(true);
            builder.danger_accept_invalid_hostnames(true);
        }
        "verify_ca" => {
            builder.danger_accept_invalid_hostnames(true);
        }
        "verify_full" => {}
        other => return Err(PgSourceError::TlsModeUnsupported(other.to_owned())),
    }
    builder
        .build()
        .map_err(PgSourceError::TlsConfigurationFailed)
}

async fn connect_client(
    cfg: tokio_postgres::Config,
    database: &str,
    tls_mode: &str,
) -> Result<Client, PgSourceError> {
    match tls_mode {
        "disabled" => connect_no_tls(cfg, database).await,
        "preferred" => {
            let tls = native_tls_connector("required")?;
            match connect_with_tls(cfg.clone(), tls, database).await {
                Ok(client) => Ok(client),
                Err(PgSourceError::TlsConnectionFailed { .. }) => {
                    connect_no_tls(cfg, database).await
                }
                Err(error) => Err(error),
            }
        }
        "required" | "verify_ca" | "verify_full" => {
            let tls = native_tls_connector(tls_mode)?;
            connect_with_tls(cfg, tls, database).await
        }
        other => Err(PgSourceError::TlsModeUnsupported(other.to_owned())),
    }
}

async fn connect_no_tls(
    cfg: tokio_postgres::Config,
    database: &str,
) -> Result<Client, PgSourceError> {
    let (client, connection) = cfg.connect(tokio_postgres::NoTls).await.map_err(|source| {
        PgSourceError::ConnectionFailed {
            database: database.to_owned(),
            source,
        }
    })?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(error = %error, "postgres catalog connection terminated");
        }
    });
    Ok(client)
}

async fn connect_with_tls(
    cfg: tokio_postgres::Config,
    connector: native_tls::TlsConnector,
    database: &str,
) -> Result<Client, PgSourceError> {
    let connector = postgres_native_tls::MakeTlsConnector::new(connector);
    let (client, connection) =
        cfg.connect(connector)
            .await
            .map_err(|source| PgSourceError::TlsConnectionFailed {
                database: database.to_owned(),
                source,
            })?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::warn!(error = %error, "postgres TLS catalog connection terminated");
        }
    });
    Ok(client)
}

async fn inspect_client(client: &Client) -> Result<PgCatalogProbe, PgSourceError> {
    let server = probe_server_version(client).await?;
    let server_identity = probe_server_identity(client).await?;
    let mut databases = list_live_databases(client).await?;
    databases.sort();
    Ok(PgCatalogProbe {
        server_version: server.raw,
        server_identity,
        databases,
    })
}

async fn probe_server_identity(client: &Client) -> Result<String, PgSourceError> {
    let row = client
        .query_one(
            "SELECT system_identifier::text FROM pg_control_system()",
            &[],
        )
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    let raw: String = row.get(0);
    canonical_pg_server_identity(&raw).ok_or(PgSourceError::VersionParseFailed(raw))
}

fn canonical_pg_server_identity(raw: &str) -> Option<String> {
    let value = raw.trim();
    let parsed = value.parse::<u64>().ok()?;
    if parsed == 0 || parsed.to_string() != value {
        return None;
    }
    Some(format!("postgresql-system-id:{value}"))
}

async fn probe_server_version(client: &Client) -> Result<ToolVersion, PgSourceError> {
    let row = client
        .query_one(
            "SELECT version(), current_setting('server_version_num')",
            &[],
        )
        .await
        .map_err(PgSourceError::VersionQueryFailed)?;
    let raw: String = row.get(0);
    let num_str: String = row.get(1);
    let num: u32 = num_str
        .parse()
        .map_err(|_| PgSourceError::VersionParseFailed(num_str.clone()))?;
    Ok(ToolVersion {
        major: num / 10000,
        raw,
    })
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
        .query(
            "SELECT extname, extversion FROM pg_extension ORDER BY extname",
            &[],
        )
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    Ok(rows
        .into_iter()
        .map(|r| ExtensionInfo {
            name: r.get(0),
            version: r.get(1),
        })
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

fn selection_includes_globals(selection: &BackupScopeSelection) -> bool {
    selection.is_whole_database()
}

impl PostgresSource {
    async fn plan_with_selection(
        &self,
        selection: &BackupScopeSelection,
    ) -> anyhow::Result<DumpPlan> {
        if !selection.is_whole_database() && self.registered_databases.len() != 1 {
            anyhow::bail!("POSTGRES_EXPLICIT_SCOPE_REQUIRES_ONE_DATABASE");
        }
        let admin = self.connect(&self.conn.admin_database).await?;

        // R4/R5: 버전 재검증이 어떤 파트 생성보다 먼저 게이트로 선다.
        let server = probe_server_version(&admin).await?;
        let pg_dump_path = self.matrix.select_pg_dump(server.major)?;
        let pg_dump_sha256 = self.matrix.expected_sha256(&pg_dump_path)?;
        let pg_dump_dependencies = self.matrix.trusted_dependencies(&pg_dump_path)?;
        let client = probe_client_version_with_identity(
            &pg_dump_path,
            pg_dump_sha256,
            &pg_dump_dependencies,
        )
        .await?;
        if client.major < server.major {
            return Err(PgSourceError::ClientTooOld {
                client_major: client.major,
                server_major: server.major,
            }
            .into());
        }
        let probe = ClusterVersionProbe {
            server,
            client,
            detected_at: Utc::now(),
        };
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
        let (globals_backup_id, globals_part) = if selection_includes_globals(selection) {
            let backup_id = Uuid::new_v4();
            let pg_dumpall_path = self.matrix.select_pg_dumpall(probe.server.major)?;
            let pg_dumpall_sha256 = self.matrix.expected_sha256(&pg_dumpall_path)?;
            let pg_dumpall_dependencies = self.matrix.trusted_dependencies(&pg_dumpall_path)?;
            (
                Some(backup_id),
                Some(DumpPlanPart {
                    backup_id,
                    job: self.job.clone(),
                    source_alias: self.alias.clone(),
                    kind: DumpPartKind::Globals,
                    pairs_with_globals_backup_id: None,
                    dump_tool: "pg_dumpall",
                    dump_tool_path: pg_dumpall_path,
                    dump_tool_sha256: pg_dumpall_sha256,
                    dump_tool_dependencies: pg_dumpall_dependencies,
                    dump_tool_version: probe.client.raw.clone(),
                    dump_format: "plain",
                    dump_flags: dump::render_globals_flags(&self.conn),
                    extensions: Vec::new(),
                    charset: None,
                    collation: None,
                }),
            )
        } else {
            (None, None)
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
                pairs_with_globals_backup_id: globals_backup_id,
                dump_tool: "pg_dump",
                dump_tool_path: pg_dump_path.clone(),
                dump_tool_sha256: pg_dump_sha256,
                dump_tool_dependencies: pg_dump_dependencies.clone(),
                dump_tool_version: probe.client.raw.clone(),
                dump_format: "custom",
                dump_flags: dump::render_db_flags(&self.conn, db, selection),
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

        Ok(DumpPlan {
            globals_part,
            db_parts,
            warnings,
        })
    }

    fn spawn_with_selection(
        &self,
        part: &DumpPlanPart,
        selection: &BackupScopeSelection,
    ) -> anyhow::Result<DumpArtifact> {
        dump::spawn_part(&self.conn, part, selection)
    }
}

#[async_trait]
impl BackupSource for PostgresSource {
    async fn plan(&self) -> anyhow::Result<DumpPlan> {
        self.plan_with_selection(&BackupScopeSelection::whole_database())
            .await
    }

    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
        self.spawn_with_selection(part, &BackupScopeSelection::whole_database())
    }
}

#[async_trait]
impl BackupSource for ScopedPostgresSource {
    async fn plan(&self) -> anyhow::Result<DumpPlan> {
        self.source.plan_with_selection(&self.selection).await
    }

    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
        self.source.spawn_with_selection(part, &self.selection)
    }
}

#[cfg(test)]
mod backup_scope_tests {
    use dbbackup_core::{BackupScopeSelection, SelectedTable};

    use super::{canonical_pg_server_identity, selection_includes_globals};

    #[test]
    fn explicit_table_scope_omits_the_globals_dump_part() {
        let explicit =
            BackupScopeSelection::explicit_tables(vec![
                SelectedTable::new("public", "orders").expect("selected table")
            ])
            .expect("explicit selection");

        assert!(!selection_includes_globals(&explicit));
        assert!(selection_includes_globals(
            &BackupScopeSelection::whole_database()
        ));
    }

    #[test]
    fn postgresql_server_identity_is_canonical_and_fail_closed() {
        assert_eq!(
            canonical_pg_server_identity(" 7541424833265539027 ")
                .expect("unsigned system identifier"),
            "postgresql-system-id:7541424833265539027"
        );
        assert!(canonical_pg_server_identity("").is_none());
        assert!(canonical_pg_server_identity("-1").is_none());
        assert!(canonical_pg_server_identity("system-a").is_none());
    }
}
