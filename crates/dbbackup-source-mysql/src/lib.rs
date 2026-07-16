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

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dbbackup_pipeline::artifact::{DumpArtifact, DumpExit, DumpStream};
use dbbackup_pipeline::source::{BackupSource, DumpPartKind, DumpPlan, DumpPlanPart};
use uuid::Uuid;

use anchor::{AnchorScanningReader, BinlogAnchor};
use binary::{ClientKind, MysqldumpBinaryMatrix};
use dump::{drain_stderr_tail, render_dump_flags, spawn_mysqldump, MySqlConnCfg};
use engine_scan::{classify_non_transactional, ensure_not_system_schema, NonTransactionalTable};
use flags::{build_flags, DDL_RACE_FATAL_PATTERNS};
use meta::MySqlMetaConn;
use version::{enforce_client_not_older, probe_client_version, ServerKind, ServerVersionInfo};

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
    binary: std::path::PathBuf,
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
            pre_dump: Arc::new(Mutex::new(None)),
        }
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

        // R6: 배포판축 매트릭스 해석 — 미매칭이면 하드 실패(가장 가까운 걸로 강행 금지).
        let (client_kind, binary) = self.binary_matrix.resolve(&server)?;

        // R7: 선택된 바이너리를 실제 실행해 클라이언트≥서버 게이트 통과 확인.
        let (client_major, client_raw) = probe_client_version(&binary).await?;
        enforce_client_not_older(client_major, server.major)?;
        tracing::info!(server = %server.raw, client = %client_raw, "mysql version probe ok");

        // R1: PRE-DUMP 엔진 스캔 — 덤프 시작 전에 반드시 완료.
        let rows = self.meta.scan_tables(&db_name).await?;
        let non_tx = classify_non_transactional(rows);
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
        let mut spawn_args = build_flags(&server, client_kind, self.capture_binlog);
        spawn_args.push(db_name.clone());
        let dump_flags = render_dump_flags(&binary, &spawn_args);

        let anchor = Arc::new(Mutex::new(BinlogAnchor::default()));

        // 매니페스트로 흘려보낼 도메인 메타데이터 + spawn이 읽을 실행 인자를 사이드채널에 적재.
        *self.pre_dump.lock().unwrap() = Some(MySqlPreDumpReport {
            server_version: server,
            client_kind,
            charset: charset.clone(),
            collation: collation.clone(),
            non_transactional_tables: non_tx,
            dump_flags: dump_flags.clone(),
            binary: binary.clone(),
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
            dump_tool: "mysqldump",
            dump_tool_path: binary,
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
        let (binary, spawn_args, anchor) = {
            let guard = self.pre_dump.lock().unwrap();
            let report = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("spawn 전에 plan()이 호출되지 않음"))?;
            (
                report.binary.clone(),
                report.spawn_args.clone(),
                report.binlog_anchor.clone(),
            )
        };

        let mut child = spawn_mysqldump(&binary, &spawn_args, &self.conn)?;
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // R8/R11: 바이트는 원본 그대로 파이프라인에 전달하되 헤더 구간에서 앵커를 훔쳐본다.
        let reader = AnchorScanningReader::new(stdout, anchor);

        let wait = Box::pin(async move {
            let stderr_tail = drain_stderr_tail(stderr).await;
            let status = child.wait().await?;
            Ok(DumpExit {
                code: status.code(),
                success: status.success(),
                stderr_tail,
            })
        });

        Ok(DumpArtifact::Stream(DumpStream {
            reader: Box::new(reader),
            wait,
        }))
    }
}

/// S08 Manifest로 흘려보낼 MySQL 전용 필드(현 파이프라인 Manifest에 아직 필드가 없어, 이 크레이트가
/// 별도 타입으로 노출한다 — S08 확장 시 그대로 매핑). non_transactional_tables는 비어도 명시적 []이다(R1).
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

impl MySqlPreDumpReport {
    /// 스트림 소비(=커밋)가 끝난 뒤 호출해야 binlog_anchor가 채워져 있다(anchor tee가 헤더까지 읽은 뒤).
    pub fn into_manifest_fields(self) -> MySqlManifestFields {
        let anchor = self.binlog_anchor.lock().unwrap().clone();
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
            dump_tool: "mysqldump".to_string(),
            dump_flags: self.dump_flags,
            binlog_coordinates: anchor.binlog_coordinates,
            gtid_set: anchor.gtid_set,
        }
    }
}
