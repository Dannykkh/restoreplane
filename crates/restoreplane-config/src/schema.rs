// crates/restoreplane-config/src/schema.rs
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use restoreplane_core::{
    ChecksumAlgo, DestKind, Engine, ImmutabilityCapability, NotifyChannel, ObjectLockMode,
};
use serde::{Deserialize, Serialize};

use crate::ConfigError;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub meta: Meta,
    #[serde(default, rename = "database")]
    pub databases: Vec<Database>,
    #[serde(default, rename = "ssh")]
    pub tunnels: Vec<Ssh>,
    #[serde(default, rename = "destination")]
    pub destinations: Vec<Destination>,
    #[serde(default, rename = "jobs")]
    pub jobs: Vec<Job>,
    pub policy: Policy,
    #[serde(default)]
    pub notify: Option<Notify>,
}

impl Config {
    /// 새 config를 위한 최소 골격(`init`이 채워 나감).
    pub fn empty() -> Self {
        Config {
            meta: Meta {
                schema_version: CURRENT_SCHEMA_VERSION,
            },
            databases: Vec::new(),
            tunnels: Vec::new(),
            destinations: Vec::new(),
            jobs: Vec::new(),
            policy: Policy::default(),
            notify: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        // Windows 메모장 등이 붙이는 UTF-8 BOM은 toml 파서가 처리하지 못한다 — 방어적 스트립.
        let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
        crate::migrate::load_and_migrate(raw)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let raw = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        std::fs::write(path, raw).map_err(|e| ConfigError::Io(path.to_path_buf(), e))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Database {
    pub alias: String,
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: restoreplane_core::EncField,
    /// [[ssh]] name 참조 — 바스티온 경유 시.
    #[serde(default)]
    pub tunnel: Option<String>,
    /// PostgreSQL 전용: pg_dumpall --globals-only 동봉 여부(P2-1: "Globals toggle").
    /// MySQL/MariaDB에서는 무시되며 validate()가 경고 대상으로 표시한다.
    #[serde(default = "default_true")]
    pub dump_globals: bool,
    /// MySQL/MariaDB 전용: None이면 [policy]의 기본값을 따른다.
    #[serde(default)]
    pub single_transaction: Option<bool>,
}

/// 백업 호스트 -> DB 서버 바스티온 터널. host_key_fingerprint는 필수(옵션 아님) —
/// blind-accept는 MITM 구멍이라는 것이 team-review.md P2-1/H6의 결론.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ssh {
    pub name: String,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    pub private_key_path: PathBuf,
    /// 예: "SHA256:base64..." — russh host-key 검증에 사용(S07).
    pub host_key_fingerprint: String,
}

/// s3 | ssh | local 세 종류를 한 구조체에 담되, kind별 필드는 모두 Option으로 둔다.
/// (내부적으로 tag+flatten enum도 고려했으나 toml serde 직렬화 쪽 알려진 함정이 있어
/// 스키마 안정성을 우선해 평탄한 구조를 택함 — Test Scenarios/Risk 절 참고.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub name: String,
    pub kind: DestKind,

    // --- s3 ---
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    #[serde(default)]
    pub prefix: String,
    pub access_key_id: Option<restoreplane_core::EncField>,
    pub secret_access_key: Option<restoreplane_core::EncField>,
    pub checksum_algo: Option<ChecksumAlgo>,
    pub object_lock: Option<ObjectLockConfig>,

    // --- ssh (russh 직결, [[ssh]] 터널과 별개 host-key 핀) ---
    pub tunnel: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub private_key_path: Option<PathBuf>,
    pub host_key_fingerprint: Option<String>,

    // --- ssh 원격 경로 / local 경로 공용 ---
    pub path: Option<PathBuf>,

    /// 복구·검증 전용 읽기 신원. 백업 쓰기 자격증명과 의도적으로 분리하며,
    /// 원격 ManifestSource는 이 값이 없을 때 writer 신원으로 폴백하지 않는다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reader: Option<ReaderIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReaderIdentity {
    // --- s3 ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<restoreplane_core::EncField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<restoreplane_core::EncField>,

    // --- ssh ---
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key_passphrase: Option<restoreplane_core::EncField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<restoreplane_core::EncField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectLockConfig {
    pub mode: ObjectLockMode,
    pub retention_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    /// Database.alias 참조.
    pub source: String,
    /// Destination.name 참조(1개 이상, 옵션 C 팬아웃).
    pub destinations: Vec<String>,
    pub required_immutability: ImmutabilityCapability,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub verify: VerifyPolicy,
    /// cron 유사 표현식. 실제 등록/파싱은 S10(schtasks) 소관, 여기서는 원문만 보관.
    pub schedule: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// GFS 등급별 보존(daily/weekly/monthly 세대 수).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Retention {
    #[serde(default)]
    pub daily: u32,
    #[serde(default)]
    pub weekly: u32,
    #[serde(default)]
    pub monthly: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyPolicy {
    /// 1=파일무결성 2=포맷유효성 3=실제복원 (domain-dictionary.md Verify Level).
    #[serde(default = "default_verify_level")]
    pub level: u8,
    /// "every_run" | "weekly" | cron 표현식.
    #[serde(default = "default_verify_schedule")]
    pub schedule: String,
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        VerifyPolicy {
            level: default_verify_level(),
            schedule: default_verify_schedule(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default = "default_max_parallel_dumps")]
    pub max_parallel_dumps: u32,
    #[serde(default)]
    pub retry: RetryPolicy,
    pub spool_dir: PathBuf,
    #[serde(default = "default_min_free_disk_mb")]
    pub min_free_disk_mb: u64,
    /// db alias -> 핀 고정된 덤프 도구 버전(P2-1: "dump_tool_version pin per source").
    #[serde(default)]
    pub dump_tool_version: BTreeMap<String, String>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_parallel_dumps: default_max_parallel_dumps(),
            retry: RetryPolicy::default(),
            spool_dir: PathBuf::from("spool"),
            min_free_disk_mb: default_min_free_disk_mb(),
            dump_tool_version: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    #[serde(default = "default_backoff_secs")]
    pub backoff_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: default_retry_attempts(),
            backoff_secs: default_backoff_secs(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notify {
    pub channel: NotifyChannel,
    #[serde(default)]
    pub webhook_url: Option<restoreplane_core::EncField>,
    #[serde(default)]
    pub email_to: Option<String>,
}

fn default_true() -> bool {
    true
}
fn default_ssh_port() -> u16 {
    22
}
fn default_verify_level() -> u8 {
    2
}
fn default_verify_schedule() -> String {
    "every_run".to_string()
}
fn default_max_parallel_dumps() -> u32 {
    1
}
fn default_min_free_disk_mb() -> u64 {
    2048
}
fn default_retry_attempts() -> u32 {
    3
}
fn default_backoff_secs() -> u64 {
    30
}
