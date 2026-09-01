// crates/dbbackup-cli/src/cli.rs
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "dbbackup",
    version,
    about = "Ransomware-resilient DB backup orchestrator (Windows)"
)]
pub struct Cli {
    /// Config 파일 경로.
    #[arg(long, global = true, default_value = "dbbackup.toml")]
    pub config: PathBuf,

    /// 로그 상세도 (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 최초 설정: 마스터 비밀번호, DEK, 복구코드, DPAPI 등록.
    Init(InitArgs),
    /// 환경 자가진단: 덤프 도구 바이너리/버전, 디스크 여유공간, DPAPI 가용성.
    Doctor,
    /// 상주 서비스의 authenticated local IPC 진단.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// DB 접속/터널/목적지 쓰기가능 여부를 무인 스케줄링 전에 점검.
    TestConnection(TestConnectionArgs),
    /// [[database]] 항목 추가.
    AddDb(AddDbArgs),
    /// [[destination]] 항목 추가.
    AddDest(AddDestArgs),
    /// 백업 잡 실행.
    Run(RunArgs),
    /// 검증(L1/L2/L3) 수행.
    Verify(VerifyArgs),
    /// 백업 복원.
    Restore(RestoreArgs),
    /// 비밀번호/복구코드/DPAPI 재등록 및 DEK 재래핑.
    RotateKey(RotateKeyArgs),
    /// 새 PC/재설치 시 DPAPI Machine-scope 래핑 재등록.
    RegisterDpapi,
    /// 키 관련 조회.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// 로컬 카탈로그 조작.
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// 격리 신원으로 보존정책 적용. run이 절대 호출하지 않음.
    Prune(PruneArgs),
    /// Windows Task Scheduler 등록/갱신.
    Schedule(ScheduleArgs),
}

#[derive(Subcommand, Debug)]
pub enum ServiceAction {
    /// Release qualification harness only: send one allowlisted command through the installed
    /// service IPC boundary. Hidden because this is not an operator-facing generic RPC surface.
    #[command(hide = true)]
    QualificationRequest(QualificationRequestArgs),
    /// 서비스 authority/schema/IPC 상태.
    Status,
    /// queue/lease/permit/spool/WAL/tool/Destination/notification 종합 진단.
    Diagnostics,
    /// Operational Store schema와 SQLite 안전 설정.
    Schema,
    /// 비밀·원시 프로세스 출력을 제외한 redacted support bundle 생성 작업.
    SupportBundle,
    /// 새 claim을 닫고 실행 중 작업이 drain될 때까지 업데이트 준비 상태로 전환.
    Drain,
    /// Operational Store 온라인 백업과 독립 rehearsal 복사본 생성.
    ControlBackup,
    /// 검증된 새 서비스가 minimum reader를 commit하고 최종 재시작을 요청.
    CommitUpdate,
    /// 서비스가 소유하는 Database Backup Scope 조회·발견·저장.
    Scope {
        #[command(subcommand)]
        action: ServiceScopeAction,
    },
    /// 정지된 서비스의 clean cutover만 legacy authority로 되돌리고 Task를 복원.
    RecoverCutover(RecoverCutoverArgs),
}

#[derive(Args, Debug)]
pub struct QualificationRequestArgs {
    /// Exact allowlisted ApplicationCommand descriptor name.
    #[arg(long)]
    pub command: String,
    /// UTF-8 JSON object payload. The file is bounded and read once before IPC dispatch.
    #[arg(long)]
    pub payload_file: PathBuf,
    /// Required for every non-read-only command; forbidden for read-only commands.
    #[arg(long)]
    pub idempotency_key: Option<uuid::Uuid>,
}

#[derive(Subcommand, Debug)]
pub enum ServiceScopeAction {
    /// Workspace 또는 Database Target의 immutable Backup Scope 목록.
    List(BackupScopeListArgs),
    /// 대상 Database의 선택 가능한 테이블 inventory 발견 작업을 queue에 등록.
    Discover(BackupScopeTargetArgs),
    /// 가장 최근 발견 inventory를 cursor 기반으로 조회.
    Objects(BackupScopeObjectsArgs),
    /// whole database 또는 exact-table Backup Scope 저장.
    Save(BackupScopeSaveArgs),
}

#[derive(Args, Debug)]
pub struct BackupScopeListArgs {
    /// 생략하면 Default Workspace를 사용한다.
    #[arg(long)]
    pub workspace_id: Option<uuid::Uuid>,
    /// 생략하면 Workspace 전체 Scope를 조회한다.
    #[arg(long)]
    pub database_target_id: Option<uuid::Uuid>,
    /// Previous response nextCursor. Omit for the first page.
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub limit: u32,
    #[arg(long)]
    pub state: Option<String>,
    #[arg(long)]
    pub search: Option<String>,
}

#[derive(Args, Debug)]
pub struct BackupScopeTargetArgs {
    /// 생략하면 Default Workspace를 사용한다.
    #[arg(long)]
    pub workspace_id: Option<uuid::Uuid>,
    #[arg(long)]
    pub database_target_id: uuid::Uuid,
    /// 응답 유실 후 동일 작업을 안전하게 재시도할 때 그대로 재사용할 UUID.
    #[arg(long)]
    pub idempotency_key: uuid::Uuid,
}

#[derive(Args, Debug)]
pub struct BackupScopeObjectsArgs {
    /// 생략하면 Default Workspace를 사용한다.
    #[arg(long)]
    pub workspace_id: Option<uuid::Uuid>,
    #[arg(long)]
    pub database_target_id: uuid::Uuid,
    #[arg(long)]
    pub discovery_run_id: uuid::Uuid,
    /// 이전 응답의 nextCursor. 생략하면 첫 페이지.
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub limit: u32,
    #[arg(long)]
    pub search: Option<String>,
    #[arg(long)]
    pub schema_name: Option<String>,
    #[arg(long)]
    pub eligibility: Option<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum BackupScopeModeArg {
    WholeDatabase,
    ExplicitTables,
}

#[derive(Args, Debug)]
pub struct BackupScopeSaveArgs {
    /// 생략하면 Default Workspace를 사용한다.
    #[arg(long)]
    pub workspace_id: Option<uuid::Uuid>,
    #[arg(long)]
    pub database_target_id: uuid::Uuid,
    /// 응답 유실 후 동일 저장을 안전하게 재시도할 때 그대로 재사용할 UUID.
    #[arg(long)]
    pub idempotency_key: uuid::Uuid,
    #[arg(long)]
    pub display_name: String,
    #[arg(long, value_enum)]
    pub mode: BackupScopeModeArg,
    /// explicit-tables에서 inventory를 만든 discovery operation UUID.
    #[arg(long)]
    pub discovery_run_id: Option<uuid::Uuid>,
    /// explicit-tables에 포함할 exact engine object ID. 여러 번 지정할 수 있다.
    #[arg(long = "object-id", action = clap::ArgAction::Append)]
    pub engine_object_ids: Vec<String>,
}

#[derive(Args, Debug)]
pub struct RecoverCutoverArgs {
    /// 설치된 Windows 서비스 계정의 canonical SID.
    #[arg(long)]
    pub service_sid: String,
    /// rollback audit와 결합할 원 cutover marker.
    #[arg(long)]
    pub cutover_marker: String,
    /// protected evidence root 기준 recovery bundle manifest 상대 경로.
    #[arg(long, default_value = "bundle-manifest.json")]
    pub recovery_bundle_manifest: String,
    /// 재시도할 때도 동일하게 유지해야 하는 rollback UUID.
    #[arg(long)]
    pub rollback_id: uuid::Uuid,
    /// 재시도할 때도 동일하게 유지해야 하는 최초 rollback 시각(epoch milliseconds).
    #[arg(long)]
    pub rolled_back_at_ms: i64,
    #[arg(long, value_enum)]
    pub reason: RecoveryReasonArg,
    /// 오입력 방지를 위해 cutover marker 전체를 동일하게 다시 입력.
    #[arg(long)]
    pub confirm_cutover_marker: String,
    /// 기본값은 `%ProgramData%\DbBackup`; fixture/복구 매체에서만 재지정.
    #[arg(long)]
    pub program_data_directory: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum RecoveryReasonArg {
    ServiceStartFailed,
    PostCutoverHealthFailed,
    OperatorRequested,
}

#[derive(Subcommand, Debug)]
pub enum KeysAction {
    /// 어떤 wrap 경로(password/recovery/dpapi)가 활성인지 표시.
    /// 복구코드 자체는 init 시 1회만 표시하며 여기서 재출력하지 않는다.
    Status,
}

#[derive(Subcommand, Debug)]
pub enum CatalogAction {
    /// 서명된 매니페스트로부터 로컬 SQLite 카탈로그 재구성.
    Rebuild(CatalogRebuildArgs),
}

#[derive(Args, Debug)]
pub struct InitArgs {
    #[arg(long)]
    pub yes: bool,
    /// 기존 키스토어를 덮어쓴다(기본은 거부).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct TestConnectionArgs {
    #[arg(long)]
    pub db: Option<String>,
}

#[derive(Args, Debug)]
pub struct AddDbArgs {
    #[arg(long)]
    pub alias: String,
    #[arg(long, value_enum)]
    pub engine: EngineArg,
    #[arg(long)]
    pub host: String,
    #[arg(long)]
    pub port: u16,
    #[arg(long)]
    pub dbname: String,
    #[arg(long)]
    pub user: String,
    #[arg(long)]
    pub tunnel: Option<String>,
    #[arg(long, default_value_t = true)]
    pub dump_globals: bool,
}

#[derive(Args, Debug)]
pub struct AddDestArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long, value_enum)]
    pub kind: DestKindArg,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// 특정 [[database]] alias의 잡만 실행. 생략 시 활성화된 모든 잡.
    #[arg(long, conflicts_with = "job")]
    pub db: Option<String>,
    /// 특정 [[jobs]] name의 잡만 실행. Task Scheduler가 생성하는 실행 경로에서도 사용.
    #[arg(long)]
    pub job: Option<String>,
    /// Windows Task Scheduler에서 시작된 실행임을 표시. 수동 실행과 동일한 보호 진입점을 사용한다.
    #[arg(long, requires = "job", conflicts_with = "dry_run")]
    pub scheduled: bool,
    /// 계획만 세우고 덤프/업로드는 실행하지 않음.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    #[arg(long)]
    pub backup: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=3))]
    pub level: Option<u8>,
    /// 검증할 매니페스트/암호문을 읽을 설정 목적지 이름 또는 로컬 경로.
    #[arg(long = "from-destination")]
    pub from_destination: Option<String>,
}

#[derive(Args, Debug)]
pub struct RestoreArgs {
    #[arg(long)]
    pub backup: String,
    #[arg(long = "from-destination")]
    pub from_destination: String,
    #[arg(long = "to")]
    pub to: Option<String>,
    /// 임포트 없이 평문 덤프만 산출.
    #[arg(long = "decrypt-only")]
    pub decrypt_only: bool,
    #[arg(long = "with-recovery-code")]
    pub with_recovery_code: bool,
}

#[derive(Args, Debug)]
pub struct RotateKeyArgs {
    #[arg(long)]
    pub new_recovery_code: bool,
    #[arg(long)]
    pub re_register_dpapi: bool,
}

#[derive(Args, Debug)]
pub struct CatalogRebuildArgs {
    #[arg(long = "from")]
    pub from_destination: String,
}

#[derive(Args, Debug)]
pub struct PruneArgs {
    /// 격리 프로파일 경로. 기본 config(dbbackup.toml)는 절대 여기 오지 않는다.
    #[arg(long, default_value = "prune.toml")]
    pub prune_config: PathBuf,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct ScheduleArgs {
    #[arg(long)]
    pub job: String,
    #[arg(long)]
    pub remove: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EngineArg {
    Postgresql,
    Mysql,
    Mariadb,
}

impl From<EngineArg> for dbbackup_core::Engine {
    fn from(v: EngineArg) -> Self {
        match v {
            EngineArg::Postgresql => dbbackup_core::Engine::Postgresql,
            EngineArg::Mysql => dbbackup_core::Engine::Mysql,
            EngineArg::Mariadb => dbbackup_core::Engine::Mariadb,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DestKindArg {
    S3,
    Ssh,
    Local,
}
