// crates/dbbackup-cli/src/cli.rs
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "dbbackup", version, about = "Ransomware-resilient DB backup orchestrator (Windows)")]
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
    #[arg(long)]
    pub db: Option<String>,
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
