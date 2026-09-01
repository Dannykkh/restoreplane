// crates/restoreplane-pipeline/src/source.rs
//! S03 §2 — BackupSource trait 및 공용 계획 타입.
//! Postgres(S03)/MySQL(S04)이 이 trait을 구현한다. 타입은 여기(공용 계약)에 두고
//! restoreplane-source-pg 등 구현 크레이트가 이를 참조한다.

use std::collections::BTreeMap;

use async_trait::async_trait;
use restoreplane_core::ConsistencyGrade;
use uuid::Uuid;

use crate::artifact::DumpArtifact;

/// 덤프 소스 공통 계약. `dyn BackupSource`로 잡 오케스트레이터(CLI run)에서 다형적으로 다룬다.
#[async_trait]
pub trait BackupSource: Send + Sync {
    /// 백업 실행 계획을 세운다: 서버 상태 조회(버전/DB목록/확장) + 하드 실패 게이트(R4/R5).
    async fn plan(&self) -> anyhow::Result<DumpPlan>;

    /// 계획의 파트 1개를 실제 자식 프로세스로 구동한다(프로세스 시작 자체는 non-blocking).
    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact>;

    /// Collects source-specific evidence after every dump stream has been consumed.
    /// Callers invoke this once, immediately before signing the set manifest.
    fn take_manifest_evidence(&self) -> anyhow::Result<SourceManifestEvidence> {
        Ok(SourceManifestEvidence::default())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceManifestEvidence {
    pub manifest_options: BTreeMap<String, String>,
    pub consistency_grade: Option<ConsistencyGrade>,
}

/// 한 번의 `plan()` 호출로 확정된 실행 계획. globals + DB별 파트로 구성된다.
pub struct DumpPlan {
    pub globals_part: Option<DumpPlanPart>,
    pub db_parts: Vec<DumpPlanPart>,
    pub warnings: Vec<PlanWarning>,
}

/// 매니페스트(S08)가 그대로 옮겨 담을 수 있는 형태로 설계된 파트 단위 계획.
#[derive(Clone, Debug)]
pub struct DumpPlanPart {
    pub backup_id: Uuid,
    pub job: String,
    pub source_alias: String,
    pub kind: DumpPartKind,
    /// PG DB 파트만 Some(같은 plan()의 globals_part.backup_id). globals 파트 자신은 None.
    pub pairs_with_globals_backup_id: Option<Uuid>,
    pub dump_tool: &'static str, // "pg_dump" | "pg_dumpall"
    pub dump_tool_path: std::path::PathBuf,
    /// Package-attested executable digest. Production native sources require this value so the
    /// supervisor can verify the locked image before resuming a credential-bearing process.
    /// Legacy/test sources may leave it unset and rely on the supervisor's local hash pin.
    pub dump_tool_sha256: Option<[u8; 32]>,
    /// Exact audited dependency closure for the native executable. The Windows supervisor locks
    /// and verifies these files for the child lifetime to prevent post-start DLL substitution.
    pub dump_tool_dependencies: Vec<(std::path::PathBuf, [u8; 32])>,
    pub dump_tool_version: String,
    pub dump_format: &'static str, // "custom" | "plain"
    pub dump_flags: String,        // 실행될 전체 커맨드라인, 시크릿 없음(R9)
    pub extensions: Vec<ExtensionInfo>,
    pub charset: Option<String>,
    pub collation: Option<String>,
}

#[derive(Clone, Debug)]
pub enum DumpPartKind {
    Globals,
    Database { name: String },
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ExtensionInfo {
    pub name: String,
    pub version: String,
}

/// 하드 실패는 아니지만 운영자가 반드시 알아야 하는 드리프트(R7).
#[derive(Debug)]
pub enum PlanWarning {
    /// 서버엔 존재하지만 add-db로 등록되지 않은 DB(백업 대상에서 누락될 위험).
    UnregisteredDatabase(String),
    /// add-db로 등록됐지만 서버에서 더는 조회되지 않는 DB(삭제/오탈자 가능성).
    RegisteredDatabaseMissing(String),
}
