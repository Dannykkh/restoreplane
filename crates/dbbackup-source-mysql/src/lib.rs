// crates/dbbackup-source-mysql/src/lib.rs
//! S04 — MySqlSource. BackupSource(dbbackup-pipeline)를 구현한다. source-pg의 두 번째 평행 구현으로,
//! 같은 트레이트 시임이 근본적으로 다른 도구(mysqldump)에도 억지 없이 들어맞음을 증명한다.
//!
//! plan(): 매 run 서버 버전/종류 재감지(R7) + 배포판축 바이너리 매트릭스 해석(R6) + PRE-DUMP 엔진
//!         스캔(R1) + charset 캡처(R3) + 플래그 조립(R2/R4/R8). 하드 실패 게이트를 파트 생성 앞에 둔다.
//! spawn(): tokio::process로 mysqldump 구동, stdout을 binlog 앵커 tee로 감싸 파이프라인에 스트림(R8/R11).
//!
//! 런타임엔 실제 MySQL이 필요하지만, version/binary/flags/engine_scan/anchor 로직은 DB 없이 테스트된다.

pub mod anchor;
pub mod binary;
pub mod dump;
pub mod engine_scan;
pub mod flags;
pub mod meta;
pub mod version;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use dbbackup_core::{BackupScopeSelection, ConsistencyGrade};
use dbbackup_pipeline::artifact::{
    DumpArtifact, DumpExit, DumpStream, NativeChildProcess, NativeDumpStream,
};
use dbbackup_pipeline::source::{
    BackupSource, DumpPartKind, DumpPlan, DumpPlanPart, SourceManifestEvidence,
};
#[cfg(windows)]
use dbbackup_runtime::{StreamingProcessExit, SupervisedStreamingProcess};
use uuid::Uuid;

use anchor::{AnchorScanningReader, BinlogAnchor};
use binary::{tool_name_for, ClientKind, MysqldumpBinaryMatrix};
use dump::{drain_stderr_tail, render_dump_flags, spawn_mysqldump, MySqlConnCfg};
use engine_scan::{
    classify_non_transactional_for_selection, ensure_not_system_schema, NonTransactionalTable,
};
use flags::{build_dump_args, DDL_RACE_FATAL_PATTERNS};
use meta::MySqlMetaConn;
use version::{
    enforce_client_not_older, probe_client_version_with_identity, ServerKind, ServerVersionInfo,
};

#[derive(thiserror::Error, Debug)]
pub enum MySqlSourceError {
    #[error("VERSION() 파싱 실패, 원문 유지 필요: {0}")]
    VersionParseFailed(String),
    #[error("서버 종류 {server_kind:?} 메이저 {server_major}을(를) 커버하는 번들 mysqldump 없음 — 가장 가까운 걸로 강행하지 않음(하드 실패)")]
    NoBundledClientForServer {
        server_kind: ServerKind,
        server_major: u32,
    },
    #[error("클라이언트 메이저({client_major}) < 서버 메이저({server_major}) — 하드 실패(DT 버전 재검증)")]
    ClientTooOld {
        client_major: u32,
        server_major: u32,
    },
    #[error("add-db 대상이 시스템 스키마입니다: {0}")]
    SystemSchemaRejected(String),
    #[error("mysqldump 실행 실패({path}): {source}")]
    DumpToolSpawnFailed {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    #[error("MySQL-family dump tool trust verification failed: {path}")]
    DumpToolUntrusted { path: std::path::PathBuf },
    #[error("configured engine {expected:?} does not match detected server {actual:?}")]
    ServerKindMismatch {
        expected: ServerKind,
        actual: ServerKind,
    },
}

/// S04 소스. meta(경량 메타 조회)와 conn(mysqldump CLI 인증)을 분리해 둔다.
pub struct MySqlSource {
    pub job: String,
    pub alias: String,
    /// 메타데이터 조회 seam(버전/엔진스캔/charset). sqlx/mysql_async/CLI 교체 가능(meta.rs).
    pub meta: Box<dyn MySqlMetaConn>,
    /// mysqldump CLI 인증용. 비밀번호는 MYSQL_PWD env로만 흐른다(R10).
    pub conn: MySqlConnCfg,
    pub binary_matrix: MysqldumpBinaryMatrix,
    /// R8 — 기본 true 권장(비가역성: 앵커 공백은 영구 재현 불가).
    pub capture_binlog: bool,
    expected_server_kind: Option<ServerKind>,
    selection: BackupScopeSelection,
    /// plan()이 채우는 사이드채널. 트레이트가 DumpArtifact만 반환하도록 고정돼(S05 계약) 있어,
    /// 매니페스트용 도메인 메타데이터는 이 채널로 노출하고 커밋 직후 회수한다(take_pre_dump_report).
    pre_dump: Arc<Mutex<Option<MySqlPreDumpReport>>>,
}

/// spawn/커밋 이후 매니페스트로 흘려보낼 도메인 메타데이터 묶음.
#[derive(Clone)]
pub struct MySqlPreDumpReport {
    pub server_version: ServerVersionInfo,
    pub client_kind: ClientKind,
    pub charset: String,
    pub collation: String,
    pub non_transactional_tables: Vec<NonTransactionalTable>,
    pub dump_flags: String,
    spawn_args: Vec<String>,
    /// 스트림 소비(=커밋)가 끝난 뒤 값이 채워진다(anchor tee가 헤더 구간을 읽은 뒤).
    pub binlog_anchor: Arc<Mutex<BinlogAnchor>>,
}

impl MySqlSource {
    pub fn new(
        job: String,
        alias: String,
        meta: Box<dyn MySqlMetaConn>,
        conn: MySqlConnCfg,
        binary_matrix: MysqldumpBinaryMatrix,
        capture_binlog: bool,
    ) -> Self {
        Self {
            job,
            alias,
            meta,
            conn,
            binary_matrix,
            capture_binlog,
            expected_server_kind: None,
            selection: BackupScopeSelection::whole_database(),
            pre_dump: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_selection(mut self, selection: BackupScopeSelection) -> Self {
        self.selection = selection;
        self
    }

    pub fn with_expected_server_kind(mut self, expected: ServerKind) -> Self {
        self.expected_server_kind = Some(expected);
        self
    }

    /// S05 PipelineCfg 구성 시 이 잡의 fatal_patterns에 병합해야 하는 MySQL 전용 패턴(R9).
    /// 단순 상수 정의가 아니라 run 배선(commands/run.rs)에서 실제로 cfg에 병합된다.
    pub fn fatal_patterns() -> Vec<String> {
        DDL_RACE_FATAL_PATTERNS
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// 커밋 직후 정확히 1회 호출해 매니페스트용 메타데이터를 회수한다. plan()을 다시 호출하면
    /// 이전 값은 덮어써진다(잡당 1파트라 안전, 문서화된 계약 — T19).
    pub fn take_pre_dump_report(&self) -> Option<MySqlPreDumpReport> {
        self.pre_dump.lock().unwrap().take()
    }
}

#[async_trait]
impl BackupSource for MySqlSource {
    async fn plan(&self) -> anyhow::Result<DumpPlan> {
        let db_name = self.conn.database.clone();

        // R12: add-db 대상이 시스템 스키마면 즉시 거부(파트 생성보다 먼저).
        ensure_not_system_schema(&db_name)?;

        // R7: 매 run 서버 종류/버전 재감지(설치 시점 1회성 아님).
        let raw = self.meta.query_version().await?;
        let server = ServerVersionInfo::from_raw(&raw)?;
        ensure_expected_server_kind(self.expected_server_kind, server.kind)?;

        // R6: 배포판축 매트릭스 해석 — 미매칭이면 하드 실패(가장 가까운 걸로 강행 금지).
        let (client_kind, binary) = self.binary_matrix.resolve(&server)?;
        let binary_sha256 = self.binary_matrix.expected_sha256(&binary)?;
        let binary_dependencies = self.binary_matrix.trusted_dependencies(&binary)?;

        // R7: 선택된 바이너리를 실제 실행해 클라이언트≥서버 게이트 통과 확인.
        let (client_major, client_raw) =
            probe_client_version_with_identity(&binary, binary_sha256, &binary_dependencies)
                .await?;
        enforce_client_not_older(client_major, server.major)?;
        tracing::info!(server = %server.raw, client = %client_raw, "mysql version probe ok");

        // R1: PRE-DUMP 엔진 스캔 — 덤프 시작 전에 반드시 완료.
        let rows = self.meta.scan_tables(&db_name).await?;
        let non_tx = classify_non_transactional_for_selection(rows, &self.selection);
        if !non_tx.is_empty() {
            tracing::warn!(
                db = %db_name,
                count = non_tx.len(),
                tables = ?non_tx.iter().map(|t| t.display()).collect::<Vec<_>>(),
                "non-InnoDB 테이블 발견 — --single-transaction 컨센시 보장 밖, 덤프 도중 변경 시 조용히 비일관 가능"
            );
        }

        // R3: charset/collation 캡처(§2.4 mojibake 추적성).
        let (charset, collation) = self.meta.fetch_charset(&db_name).await?;

        // R2/R4/R8: 플래그 조립 + DB명. dump_args가 그대로 spawn argv가 된다.
        let spawn_args = build_dump_args(
            &server,
            client_kind,
            self.capture_binlog,
            &db_name,
            &self.selection,
        )?;
        let dump_flags = render_dump_flags(&binary, &spawn_args, &self.conn, client_kind)?;

        let anchor = Arc::new(Mutex::new(BinlogAnchor::default()));

        // 매니페스트로 흘려보낼 도메인 메타데이터 + spawn이 읽을 실행 인자를 사이드채널에 적재.
        *self.pre_dump.lock().unwrap() = Some(MySqlPreDumpReport {
            server_version: server,
            client_kind,
            charset: charset.clone(),
            collation: collation.clone(),
            non_transactional_tables: non_tx,
            dump_flags: dump_flags.clone(),
            spawn_args,
            binlog_anchor: anchor,
        });

        // MySQL은 항상 단일 파트(globals 개념 없음, plan.md §파이프라인 1).
        let part = DumpPlanPart {
            backup_id: Uuid::new_v4(),
            job: self.job.clone(),
            source_alias: self.alias.clone(),
            kind: DumpPartKind::Database { name: db_name },
            pairs_with_globals_backup_id: None,
            dump_tool: tool_name_for(client_kind),
            dump_tool_path: binary,
            dump_tool_sha256: binary_sha256,
            dump_tool_dependencies: binary_dependencies,
            dump_tool_version: client_raw,
            dump_format: "plain",
            dump_flags,
            extensions: Vec::new(), // MySQL엔 PG extension 개념 없음.
            charset: Some(charset),
            collation: Some(collation),
        };

        Ok(DumpPlan {
            globals_part: None,
            db_parts: vec![part],
            warnings: Vec::new(),
        })
    }

    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
        match &part.kind {
            DumpPartKind::Database { .. } => {}
            DumpPartKind::Globals => {
                anyhow::bail!("MySqlSource는 globals part를 지원하지 않음(PG 전용)")
            }
        }

        // plan()이 적재한 실행 인자/바이너리/앵커를 회수한다(잡당 1파트).
        let (spawn_args, anchor, client_kind) = {
            let guard = self.pre_dump.lock().unwrap();
            let report = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("spawn 전에 plan()이 호출되지 않음"))?;
            (
                report.spawn_args.clone(),
                report.binlog_anchor.clone(),
                report.client_kind,
            )
        };

        let mut child = spawn_mysqldump(
            &part.dump_tool_path,
            part.dump_tool_sha256,
            &part.dump_tool_dependencies,
            &spawn_args,
            &self.conn,
            client_kind,
        )?;
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

        // R8/R11: 바이트는 원본 그대로 파이프라인에 전달하되 헤더 구간에서 앵커를 훔쳐본다.
        let reader = AnchorScanningReader::new(stdout, anchor);

        let wait = Box::pin(async move {
            let stderr_task = tokio::spawn(drain_stderr_tail(stderr));
            let exit = process_waiter
                .await
                .map_err(|error| anyhow::anyhow!("MySQL dump waiter failed: {error}"))??;
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
                reader: Box::new(reader),
                wait,
                abort: Some(Box::new(move || {
                    abort_requested.store(true, Ordering::Release);
                })),
            },
        }))
    }

    fn take_manifest_evidence(&self) -> anyhow::Result<SourceManifestEvidence> {
        self.take_pre_dump_report()
            .ok_or_else(|| anyhow::anyhow!("MYSQL_PRE_DUMP_EVIDENCE_MISSING"))?
            .into_manifest_fields()
            .into_source_evidence()
    }
}

fn ensure_expected_server_kind(
    expected: Option<ServerKind>,
    actual: ServerKind,
) -> Result<(), MySqlSourceError> {
    if let Some(expected) = expected {
        if expected != actual {
            return Err(MySqlSourceError::ServerKindMismatch { expected, actual });
        }
    }
    Ok(())
}

/// S08 Manifest로 흘려보낼 MySQL 전용 필드(현 파이프라인 Manifest에 아직 필드가 없어, 이 크레이트가
/// 별도 타입으로 노출한다 — S08 확장 시 그대로 매핑). non_transactional_tables는 비어도 명시적 []이다(R1).
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

pub struct MySqlManifestFields {
    pub db_engine: String, // "mysql" | "mariadb"
    pub server_version: String,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub non_transactional_tables: Vec<String>,
    pub dump_tool: String,
    pub dump_flags: String,
    pub binlog_coordinates: Option<String>,
    pub gtid_set: Option<String>,
}

impl MySqlManifestFields {
    pub fn into_source_evidence(self) -> anyhow::Result<SourceManifestEvidence> {
        let consistency_grade = if self.non_transactional_tables.is_empty() {
            None
        } else {
            Some(ConsistencyGrade::CrashConsistent)
        };
        let mut manifest_options = std::collections::BTreeMap::new();
        manifest_options.insert("mysql.dbEngine".to_owned(), self.db_engine);
        manifest_options.insert("mysql.serverVersion".to_owned(), self.server_version);
        if let Some(charset) = self.charset {
            manifest_options.insert("mysql.charset".to_owned(), charset);
        }
        if let Some(collation) = self.collation {
            manifest_options.insert("mysql.collation".to_owned(), collation);
        }
        manifest_options.insert(
            "mysql.nonTransactionalTables".to_owned(),
            serde_json::to_string(&self.non_transactional_tables)?,
        );
        manifest_options.insert("mysql.dumpTool".to_owned(), self.dump_tool);
        manifest_options.insert("mysql.dumpFlags".to_owned(), self.dump_flags);
        if let Some(coordinates) = self.binlog_coordinates {
            manifest_options.insert("mysql.binlogCoordinates".to_owned(), coordinates);
        }
        if let Some(gtid_set) = self.gtid_set {
            manifest_options.insert("mysql.gtidSet".to_owned(), gtid_set);
        }
        Ok(SourceManifestEvidence {
            manifest_options,
            consistency_grade,
        })
    }
}

impl MySqlPreDumpReport {
    /// 스트림 소비(=커밋)가 끝난 뒤 호출해야 binlog_anchor가 채워져 있다(anchor tee가 헤더까지 읽은 뒤).
    pub fn into_manifest_fields(self) -> MySqlManifestFields {
        let anchor = self.binlog_anchor.lock().unwrap().clone();
        let dump_tool = tool_name_for(self.client_kind).to_owned();
        MySqlManifestFields {
            db_engine: match self.server_version.kind {
                ServerKind::MariaDb => "mariadb".to_string(),
                ServerKind::OracleMySql => "mysql".to_string(),
            },
            server_version: self.server_version.raw,
            charset: Some(self.charset),
            collation: Some(self.collation),
            non_transactional_tables: self
                .non_transactional_tables
                .into_iter()
                .map(|t| t.display())
                .collect(),
            dump_tool,
            dump_flags: self.dump_flags,
            binlog_coordinates: anchor.binlog_coordinates,
            gtid_set: anchor.gtid_set,
        }
    }
}

#[cfg(test)]
mod manifest_evidence_tests {
    use dbbackup_core::ConsistencyGrade;

    use super::*;

    #[test]
    fn configured_engine_family_must_match_live_server_before_tool_selection() {
        assert!(
            ensure_expected_server_kind(Some(ServerKind::OracleMySql), ServerKind::MariaDb)
                .is_err()
        );
        assert!(
            ensure_expected_server_kind(Some(ServerKind::MariaDb), ServerKind::MariaDb).is_ok()
        );
    }

    #[test]
    fn non_transactional_tables_and_binlog_anchor_flow_into_source_evidence() {
        let fields = MySqlManifestFields {
            db_engine: "mysql".to_owned(),
            server_version: "8.4.5".to_owned(),
            charset: Some("utf8mb4".to_owned()),
            collation: Some("utf8mb4_0900_ai_ci".to_owned()),
            non_transactional_tables: vec!["app.legacy_log(MyISAM)".to_owned()],
            dump_tool: "mysqldump".to_owned(),
            dump_flags: "--single-transaction --source-data=2".to_owned(),
            binlog_coordinates: Some(
                "-- CHANGE MASTER TO MASTER_LOG_FILE='bin.0001', MASTER_LOG_POS=154;".to_owned(),
            ),
            gtid_set: Some("SET @@GLOBAL.GTID_PURGED='uuid:1-42';".to_owned()),
        };

        let evidence = fields.into_source_evidence().expect("evidence");

        assert_eq!(
            evidence.consistency_grade,
            Some(ConsistencyGrade::CrashConsistent)
        );
        assert_eq!(
            evidence.manifest_options.get("mysql.serverVersion"),
            Some(&"8.4.5".to_owned())
        );
        assert_eq!(
            evidence
                .manifest_options
                .get("mysql.nonTransactionalTables"),
            Some(&"[\"app.legacy_log(MyISAM)\"]".to_owned())
        );
        assert!(evidence
            .manifest_options
            .get("mysql.binlogCoordinates")
            .is_some_and(|value| value.contains("MASTER_LOG_POS=154")));
        assert!(evidence
            .manifest_options
            .get("mysql.gtidSet")
            .is_some_and(|value| value.contains("uuid:1-42")));
    }
}
