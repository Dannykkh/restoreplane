//! config 편집 커맨드(인프로세스) — [[database]] 등록/조회.
//!
//! CLI add-db가 쓰던 NotYetImplementedEncryptor 스텁 대신 실제 DekEncryptor로 비밀번호를 봉투
//! 암호화한다(EncField = enc:<wrap_gen>:<b64>). KEK를 마스터 비밀번호로 언락해 필드 DEK로 쓰며,
//! wrap_gen은 키스토어 init 세대(1)와 맞춘다(rotate-key 시 재래핑 대상 식별).
//!
//! 주의(백엔드 미배선 seam): run.rs는 현재 DB 비밀번호를 env(PGPASSWORD/MYSQL_PWD)로 받는다.
//! 즉 여기서 기록한 EncField를 백업 실행이 아직 복호화해 쓰지는 않는다(run 측 unwrap 배선은 별도
//! 백엔드 작업). 이 커맨드는 "config를 올바르게(암호화된 시크릿 포함) 기록"하는 계층까지 책임진다.

use restoreplane_config::schema::{
    Config, Database, Destination, Job, ObjectLockConfig, ReaderIdentity, Retention, VerifyPolicy,
};
use restoreplane_core::{DestKind, Engine, ImmutabilityCapability, ObjectLockMode, SecretEncryptor};
use restoreplane_secrets::{Credential, DekEncryptor, Keystore};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::paths::{config_path, keystore_path};

/// 키스토어 init 세대와 일치. rotate-key가 이 세대를 올리면 재래핑 대상이 된다.
const FIELD_WRAP_GEN: u32 = 1;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDatabase {
    pub alias: String,
    pub engine: String,
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: String,
    pub dump_globals: bool,
}

fn parse_engine(s: &str) -> Result<Engine, String> {
    match s {
        "postgresql" => Ok(Engine::Postgresql),
        "mysql" => Ok(Engine::Mysql),
        "mariadb" => Ok(Engine::Mariadb),
        other => Err(format!("알 수 없는 엔진: {other}")),
    }
}

fn load_or_empty(cfg_path: &std::path::Path) -> Result<Config, String> {
    if cfg_path.exists() {
        Config::load(cfg_path).map_err(|e| format!("config 로드 실패: {e}"))
    } else {
        Ok(Config::empty())
    }
}

/// [[database]] 추가. 비밀번호는 KEK(마스터 비번 언락)로 봉투 암호화해 EncField로 저장.
#[tauri::command]
pub fn add_database(master_password: String, db: NewDatabase) -> Result<(), String> {
    if db.alias.trim().is_empty() || db.host.trim().is_empty() || db.dbname.trim().is_empty() {
        return Err("alias/host/dbname은 비울 수 없습니다.".to_string());
    }
    let engine = parse_engine(&db.engine)?;
    let cfg_path = config_path();

    let mut config = load_or_empty(&cfg_path)?;
    if config.databases.iter().any(|d| d.alias == db.alias) {
        return Err(format!("이미 존재하는 alias입니다: {}", db.alias));
    }

    // KEK 언락 → 필드 암호화.
    let ks = Keystore::load(&keystore_path(&cfg_path))
        .map_err(|e| format!("키스토어 로드 실패(먼저 초기 설정을 완료하세요): {e}"))?;
    let kek = ks
        .unlock_kek(&Credential::Password(Zeroizing::new(master_password)))
        .map_err(|_| "마스터 비밀번호가 올바르지 않습니다.".to_string())?;
    let password_enc = DekEncryptor::new(kek, FIELD_WRAP_GEN)
        .wrap(&db.password)
        .map_err(|e| format!("비밀번호 암호화 실패: {e:?}"))?;

    config.databases.push(Database {
        alias: db.alias,
        engine,
        host: db.host,
        port: db.port,
        dbname: db.dbname,
        user: db.user,
        password: password_enc,
        tunnel: None,
        dump_globals: db.dump_globals,
        single_transaction: None,
    });
    config
        .save(&cfg_path)
        .map_err(|e| format!("config 저장 실패: {e}"))?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DbSummary {
    alias: String,
    engine: String,
    host: String,
    port: u16,
    dbname: String,
    user: String,
    has_tunnel: bool,
}

/// 등록된 [[database]] 목록(비밀번호 제외).
#[tauri::command]
pub fn list_databases() -> Result<Vec<DbSummary>, String> {
    let cfg_path = config_path();
    if !cfg_path.exists() {
        return Ok(vec![]);
    }
    let config = Config::load(&cfg_path).map_err(|e| format!("config 로드 실패: {e}"))?;
    Ok(config
        .databases
        .iter()
        .map(|d| DbSummary {
            alias: d.alias.clone(),
            engine: format!("{:?}", d.engine).to_lowercase(),
            host: d.host.clone(),
            port: d.port,
            dbname: d.dbname.clone(),
            user: d.user.clone(),
            has_tunnel: d.tunnel.is_some(),
        })
        .collect())
}

// ----------------------------------------------------------------------------
// 목적지(S3 / SSH / Local) 등록
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewReaderIdentity {
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub user: Option<String>,
    pub private_key_path: Option<String>,
    pub private_key_passphrase: Option<String>,
    pub password: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewDestination {
    pub name: String,
    pub kind: String, // local | s3 | ssh
    // local / ssh 원격 경로
    pub path: Option<String>,
    // s3
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    #[serde(default)]
    pub prefix: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    #[serde(default)]
    pub object_lock_mode: Option<String>, // none | governance | compliance
    #[serde(default)]
    pub retention_days: Option<u32>,
    // ssh
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub private_key_path: Option<String>,
    pub host_key_fingerprint: Option<String>,
    #[serde(default)]
    pub reader: Option<NewReaderIdentity>,
}

fn parse_dest_kind(s: &str) -> Result<DestKind, String> {
    match s {
        "local" => Ok(DestKind::Local),
        "s3" => Ok(DestKind::S3),
        "ssh" => Ok(DestKind::Ssh),
        other => Err(format!("알 수 없는 목적지 종류: {other}")),
    }
}

fn parse_object_lock(
    mode: Option<&str>,
    days: Option<u32>,
) -> Result<Option<ObjectLockConfig>, String> {
    match mode {
        None | Some("none") | Some("") => Ok(None),
        Some("governance") => Ok(Some(ObjectLockConfig {
            mode: ObjectLockMode::Governance,
            retention_days: days.unwrap_or(30),
        })),
        Some("compliance") => Ok(Some(ObjectLockConfig {
            mode: ObjectLockMode::Compliance,
            retention_days: days.unwrap_or(30),
        })),
        Some(other) => Err(format!("알 수 없는 Object Lock 모드: {other}")),
    }
}

/// [[destination]] 추가. S3 자격증명은 EncField로 봉투암호화(마스터 비번으로 KEK 언락).
#[tauri::command]
pub fn add_destination(master_password: String, dest: NewDestination) -> Result<(), String> {
    if dest.name.trim().is_empty() {
        return Err("목적지 이름은 비울 수 없습니다.".to_string());
    }
    let kind = parse_dest_kind(&dest.kind)?;
    let cfg_path = config_path();
    let mut config = load_or_empty(&cfg_path)?;
    if config.destinations.iter().any(|d| d.name == dest.name) {
        return Err(format!("이미 존재하는 목적지 이름입니다: {}", dest.name));
    }
    if matches!(kind, DestKind::S3 | DestKind::Ssh) && dest.reader.is_none() {
        return Err(format!(
            "{} 목적지는 writer와 분리된 복구용 reader 신원이 필요합니다.",
            dest.kind
        ));
    }

    let s3_reader_secret_present = kind == DestKind::S3
        && dest.reader.as_ref().is_some_and(|reader| {
            reader.access_key_id.is_some() || reader.secret_access_key.is_some()
        });
    let ssh_reader_secret_present = kind == DestKind::Ssh
        && dest.reader.as_ref().is_some_and(|reader| {
            reader.private_key_passphrase.is_some() || reader.password.is_some()
        });
    let needs_encryptor = (kind == DestKind::S3
        && (dest.access_key_id.is_some()
            || dest.secret_access_key.is_some()
            || s3_reader_secret_present))
        || ssh_reader_secret_present;
    let encryptor = if needs_encryptor {
        let ks = Keystore::load(&keystore_path(&cfg_path))
            .map_err(|e| format!("키스토어 로드 실패(초기 설정 필요): {e}"))?;
        let kek = ks
            .unlock_kek(&Credential::Password(Zeroizing::new(master_password)))
            .map_err(|_| "마스터 비밀번호가 올바르지 않습니다.".to_string())?;
        Some(DekEncryptor::new(kek, FIELD_WRAP_GEN))
    } else {
        None
    };

    let wrap = |value: Option<String>, label: &str| {
        value
            .map(|value| {
                encryptor
                    .as_ref()
                    .expect("시크릿이 있으면 encryptor가 초기화되어야 함")
                    .wrap(&value)
                    .map_err(|e| format!("{label} 암호화 실패: {e:?}"))
            })
            .transpose()
    };

    // S3 writer 자격증명은 EncField로 봉투암호화.
    let (access_enc, secret_enc) = if kind == DestKind::S3 {
        let a = dest
            .access_key_id
            .map(|value| wrap(Some(value), "access key"))
            .transpose()?
            .flatten();
        let s = dest
            .secret_access_key
            .map(|value| wrap(Some(value), "secret key"))
            .transpose()?
            .flatten();
        (a, s)
    } else {
        (None, None)
    };

    let reader = match (kind, dest.reader) {
        (DestKind::S3, Some(reader)) => {
            let access_key_id = reader
                .access_key_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "s3 reader는 access key id가 필요합니다.".to_string())?;
            let secret_access_key = reader
                .secret_access_key
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "s3 reader는 secret access key가 필요합니다.".to_string())?;
            Some(ReaderIdentity {
                access_key_id: wrap(Some(access_key_id), "reader access key")?,
                secret_access_key: wrap(Some(secret_access_key), "reader secret key")?,
                user: None,
                private_key_path: None,
                private_key_passphrase: None,
                password: None,
            })
        }
        (DestKind::Ssh, Some(reader)) => {
            let user = reader
                .user
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "ssh reader는 user가 필요합니다.".to_string())?;
            let private_key_path = reader
                .private_key_path
                .filter(|value| !value.trim().is_empty());
            let password = reader.password.filter(|value| !value.is_empty());
            if private_key_path.is_none() && password.is_none() {
                return Err("ssh reader는 개인키 경로 또는 비밀번호가 필요합니다.".to_string());
            }
            if private_key_path.is_some() && password.is_some() {
                return Err(
                    "ssh reader 인증은 개인키와 비밀번호 중 하나만 선택해야 합니다.".to_string(),
                );
            }
            let private_key_passphrase = reader
                .private_key_passphrase
                .filter(|value| !value.is_empty());
            if private_key_passphrase.is_some() && private_key_path.is_none() {
                return Err(
                    "ssh reader 개인키 암호문은 개인키 경로와 함께 입력해야 합니다.".to_string(),
                );
            }
            Some(ReaderIdentity {
                access_key_id: None,
                secret_access_key: None,
                user: Some(user),
                private_key_path: private_key_path.map(Into::into),
                private_key_passphrase: wrap(
                    private_key_passphrase,
                    "reader private key passphrase",
                )?,
                password: wrap(password, "reader password")?,
            })
        }
        _ => None,
    };

    // 종류별 필수 필드 검증.
    match kind {
        DestKind::Local => {
            if dest.path.as_deref().unwrap_or("").trim().is_empty() {
                return Err("local 목적지는 path가 필요합니다.".to_string());
            }
        }
        DestKind::S3 => {
            if dest.bucket.as_deref().unwrap_or("").trim().is_empty() {
                return Err("s3 목적지는 bucket이 필요합니다.".to_string());
            }
        }
        DestKind::Ssh => {
            if dest.host.as_deref().unwrap_or("").trim().is_empty()
                || dest
                    .host_key_fingerprint
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .is_empty()
            {
                return Err(
                    "ssh 목적지는 host와 host_key_fingerprint가 필요합니다(MITM 방어).".to_string(),
                );
            }
        }
    }

    let object_lock = parse_object_lock(dest.object_lock_mode.as_deref(), dest.retention_days)?;
    let checksum_algo = object_lock
        .as_ref()
        .map(|_| restoreplane_core::ChecksumAlgo::Sha256);

    config.destinations.push(Destination {
        name: dest.name,
        kind,
        endpoint: dest.endpoint.filter(|s| !s.trim().is_empty()),
        region: dest.region.filter(|s| !s.trim().is_empty()),
        bucket: dest.bucket.filter(|s| !s.trim().is_empty()),
        prefix: dest.prefix,
        access_key_id: access_enc,
        secret_access_key: secret_enc,
        checksum_algo,
        object_lock,
        tunnel: None,
        host: dest.host.filter(|s| !s.trim().is_empty()),
        port: dest.port,
        user: dest.user.filter(|s| !s.trim().is_empty()),
        private_key_path: dest.private_key_path.map(Into::into),
        host_key_fingerprint: dest.host_key_fingerprint.filter(|s| !s.trim().is_empty()),
        path: dest.path.map(Into::into),
        reader,
    });
    if let Err(errors) = restoreplane_config::validate(&config) {
        let details = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(format!("목적지 설정 검증 실패: {details}"));
    }
    config
        .save(&cfg_path)
        .map_err(|e| format!("config 저장 실패: {e}"))?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DestSummary {
    name: String,
    kind: String,
    /// 대시보드와 동일 어휘의 불변성 등급(none/server_append_only/object_lock_*).
    immutability: String,
    location: String,
}

fn dest_immutability(d: &Destination) -> &'static str {
    match d.kind {
        DestKind::Local => "none",
        DestKind::Ssh => "server_append_only",
        DestKind::S3 => match d.object_lock.as_ref().map(|c| c.mode) {
            Some(ObjectLockMode::Governance) => "object_lock_governance",
            Some(ObjectLockMode::Compliance) => "object_lock_compliance",
            _ => "none",
        },
    }
}

/// 등록된 [[destination]] 목록(자격증명 제외).
#[tauri::command]
pub fn list_destinations() -> Result<Vec<DestSummary>, String> {
    let cfg_path = config_path();
    if !cfg_path.exists() {
        return Ok(vec![]);
    }
    let config = Config::load(&cfg_path).map_err(|e| format!("config 로드 실패: {e}"))?;
    Ok(config
        .destinations
        .iter()
        .map(|d| {
            let location = match d.kind {
                DestKind::Local => d
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                DestKind::S3 => format!("{}/{}", d.bucket.clone().unwrap_or_default(), d.prefix),
                DestKind::Ssh => format!(
                    "{}@{}:{}",
                    d.user.clone().unwrap_or_default(),
                    d.host.clone().unwrap_or_default(),
                    d.path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
            };
            DestSummary {
                name: d.name.clone(),
                kind: format!("{:?}", d.kind).to_lowercase(),
                immutability: dest_immutability(d).to_string(),
                location,
            }
        })
        .collect())
}

// ----------------------------------------------------------------------------
// 백업 잡([[jobs]]) 등록 — source(DB) + destinations(1+) + 보존/검증/스케줄 정책
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewJob {
    pub name: String,
    pub source: String,
    pub destinations: Vec<String>,
    pub required_immutability: String,
    #[serde(default)]
    pub retention_daily: u32,
    #[serde(default)]
    pub retention_weekly: u32,
    #[serde(default)]
    pub retention_monthly: u32,
    #[serde(default = "default_level")]
    pub verify_level: u8,
    #[serde(default)]
    pub schedule: String,
}

fn default_level() -> u8 {
    1
}

fn parse_immutability(s: &str) -> Result<ImmutabilityCapability, String> {
    match s {
        "none" => Ok(ImmutabilityCapability::None),
        "governance" => Ok(ImmutabilityCapability::Governance),
        "compliance" => Ok(ImmutabilityCapability::Compliance),
        "server_append_only" => Ok(ImmutabilityCapability::ServerAppendOnly),
        other => Err(format!("알 수 없는 불변성 요구: {other}")),
    }
}

/// [[jobs]] 추가. source/destinations는 이미 등록된 것이어야 한다(참조 무결성).
#[tauri::command]
pub fn add_job(job: NewJob) -> Result<(), String> {
    if job.name.trim().is_empty() {
        return Err("잡 이름은 비울 수 없습니다.".to_string());
    }
    if job.destinations.is_empty() {
        return Err("목적지를 1개 이상 선택하세요(옵션 C 팬아웃).".to_string());
    }
    let required = parse_immutability(&job.required_immutability)?;
    let cfg_path = config_path();
    let mut config = load_or_empty(&cfg_path)?;

    if config.jobs.iter().any(|j| j.name == job.name) {
        return Err(format!("이미 존재하는 잡 이름입니다: {}", job.name));
    }
    if !config.databases.iter().any(|d| d.alias == job.source) {
        return Err(format!("등록되지 않은 소스 DB: {}", job.source));
    }
    for dn in &job.destinations {
        if !config.destinations.iter().any(|d| &d.name == dn) {
            return Err(format!("등록되지 않은 목적지: {dn}"));
        }
    }

    config.jobs.push(Job {
        name: job.name,
        source: job.source,
        destinations: job.destinations,
        required_immutability: required,
        retention: Retention {
            daily: job.retention_daily,
            weekly: job.retention_weekly,
            monthly: job.retention_monthly,
        },
        verify: VerifyPolicy {
            level: job.verify_level.clamp(1, 3),
            schedule: "every_run".to_string(),
        },
        schedule: job.schedule,
        enabled: true,
    });
    config
        .save(&cfg_path)
        .map_err(|e| format!("config 저장 실패: {e}"))?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobSummary {
    name: String,
    source: String,
    destinations: Vec<String>,
    required_immutability: String,
    schedule: String,
    verify_level: u8,
    enabled: bool,
}

/// 등록된 [[jobs]] 목록.
#[tauri::command]
pub fn list_jobs() -> Result<Vec<JobSummary>, String> {
    let cfg_path = config_path();
    if !cfg_path.exists() {
        return Ok(vec![]);
    }
    let config = Config::load(&cfg_path).map_err(|e| format!("config 로드 실패: {e}"))?;
    Ok(config
        .jobs
        .iter()
        .map(|j| JobSummary {
            name: j.name.clone(),
            source: j.source.clone(),
            destinations: j.destinations.clone(),
            required_immutability: format!("{:?}", j.required_immutability).to_lowercase(),
            schedule: j.schedule.clone(),
            verify_level: j.verify.level,
            enabled: j.enabled,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use restoreplane_secrets::Keystore;

    fn init_ks(dir: &std::path::Path) -> String {
        let cfg = dir.join("restoreplane.toml");
        std::env::set_var("RESTOREPLANE_CONFIG", &cfg);
        let pw = "correct horse battery".to_string();
        let (ks, _rec) =
            Keystore::init(&Zeroizing::new(pw.clone()), dir.join("dpapi.keyfile")).unwrap();
        ks.save(&keystore_path(&cfg)).unwrap();
        pw
    }

    #[test]
    fn add_database_encrypts_password_roundtrip() {
        let _env = crate::paths::env_guard();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let pw = init_ks(dir);

        add_database(
            pw.clone(),
            NewDatabase {
                alias: "prod-main".into(),
                engine: "postgresql".into(),
                host: "10.0.0.5".into(),
                port: 5432,
                dbname: "app".into(),
                user: "backup".into(),
                password: "s3cr3t-db-pw".into(),
                dump_globals: true,
            },
        )
        .expect("add_database 성공");

        // 목록에 반영(비밀번호 없이).
        let list = list_databases().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].alias, "prod-main");
        assert_eq!(list[0].engine, "postgresql");

        // 라운드트립: 저장된 EncField를 KEK로 복호화하면 원문 비번이 나온다.
        let cfg_path = config_path();
        let config = Config::load(&cfg_path).unwrap();
        let enc_field = &config.databases[0].password;
        let ks = Keystore::load(&keystore_path(&cfg_path)).unwrap();
        let kek = ks
            .unlock_kek(&Credential::Password(Zeroizing::new(pw.clone())))
            .unwrap();
        let dec = DekEncryptor::new(kek, FIELD_WRAP_GEN);
        let plain = dec.unwrap_field(enc_field).unwrap();
        assert_eq!(&*plain, "s3cr3t-db-pw", "EncField 복호화 == 원문");

        // 중복 alias 거부.
        assert!(add_database(
            pw.clone(),
            NewDatabase {
                alias: "prod-main".into(),
                engine: "mysql".into(),
                host: "h".into(),
                port: 3306,
                dbname: "d".into(),
                user: "u".into(),
                password: "p".into(),
                dump_globals: false,
            },
        )
        .is_err());

        // 틀린 비번 거부.
        assert!(add_database(
            "wrong-password".to_string(),
            NewDatabase {
                alias: "other".into(),
                engine: "mysql".into(),
                host: "h".into(),
                port: 3306,
                dbname: "d".into(),
                user: "u".into(),
                password: "p".into(),
                dump_globals: false,
            },
        )
        .is_err());

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    #[test]
    fn add_destination_s3_encrypts_keys_and_local_needs_no_secret() {
        let _env = crate::paths::env_guard();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let pw = init_ks(dir);

        // S3 + Object Lock governance + 자격증명 암호화.
        add_destination(
            pw.clone(),
            NewDestination {
                name: "s3-worm".into(),
                kind: "s3".into(),
                path: None,
                endpoint: Some("https://s3.ap-northeast-2.amazonaws.com".into()),
                region: Some("ap-northeast-2".into()),
                bucket: Some("my-backups".into()),
                prefix: "prod/".into(),
                access_key_id: Some("AKIAEXAMPLE".into()),
                secret_access_key: Some("s3cr3t-key-value".into()),
                object_lock_mode: Some("governance".into()),
                retention_days: Some(30),
                host: None,
                port: None,
                user: None,
                private_key_path: None,
                host_key_fingerprint: None,
                reader: Some(NewReaderIdentity {
                    access_key_id: Some("RESTORE-ID".into()),
                    secret_access_key: Some("restore-secret".into()),
                    user: None,
                    private_key_path: None,
                    private_key_passphrase: None,
                    password: None,
                }),
            },
        )
        .expect("s3 목적지 등록");

        // Local: 시크릿 없이 등록(마스터 비번 무관).
        add_destination(
            "".into(),
            NewDestination {
                name: "local-nas".into(),
                kind: "local".into(),
                path: Some(r"D:\backups".into()),
                endpoint: None,
                region: None,
                bucket: None,
                prefix: String::new(),
                access_key_id: None,
                secret_access_key: None,
                object_lock_mode: None,
                retention_days: None,
                host: None,
                port: None,
                user: None,
                private_key_path: None,
                host_key_fingerprint: None,
                reader: None,
            },
        )
        .expect("local 목적지 등록");

        let list = list_destinations().unwrap();
        assert_eq!(list.len(), 2);
        let s3 = list.iter().find(|d| d.name == "s3-worm").unwrap();
        assert_eq!(s3.immutability, "object_lock_governance");
        let local = list.iter().find(|d| d.name == "local-nas").unwrap();
        assert_eq!(local.immutability, "none");

        // S3 secret key 라운드트립 복호화.
        let cfg_path = config_path();
        let config = Config::load(&cfg_path).unwrap();
        let s3d = config
            .destinations
            .iter()
            .find(|d| d.name == "s3-worm")
            .unwrap();
        assert_eq!(
            s3d.checksum_algo,
            Some(restoreplane_core::ChecksumAlgo::Sha256),
            "Object Lock 등록은 S3 checksum을 함께 고정해야 함"
        );
        restoreplane_config::validate(&config).expect("GUI가 저장한 config는 즉시 유효해야 함");
        let ks = Keystore::load(&keystore_path(&cfg_path)).unwrap();
        let kek = ks
            .unlock_kek(&Credential::Password(Zeroizing::new(pw)))
            .unwrap();
        let dec = DekEncryptor::new(kek, FIELD_WRAP_GEN);
        let secret = dec
            .unwrap_field(s3d.secret_access_key.as_ref().unwrap())
            .unwrap();
        assert_eq!(&*secret, "s3cr3t-key-value");

        // SSH: host_key_fingerprint 없으면 거부(MITM 방어).
        assert!(add_destination(
            "".into(),
            NewDestination {
                name: "bad-ssh".into(),
                kind: "ssh".into(),
                path: Some("/backups".into()),
                endpoint: None,
                region: None,
                bucket: None,
                prefix: String::new(),
                access_key_id: None,
                secret_access_key: None,
                object_lock_mode: None,
                retention_days: None,
                host: Some("bastion".into()),
                port: Some(22),
                user: Some("svc".into()),
                private_key_path: Some("/keys/id".into()),
                host_key_fingerprint: None,
                reader: None,
            },
        )
        .is_err());

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    #[test]
    fn add_destination_s3_encrypts_separate_reader_identity() {
        let _env = crate::paths::env_guard();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let pw = init_ks(dir);

        add_destination(
            pw.clone(),
            NewDestination {
                name: "s3-reader".into(),
                kind: "s3".into(),
                path: None,
                endpoint: Some("http://127.0.0.1:9000".into()),
                region: Some("us-east-1".into()),
                bucket: Some("backups".into()),
                prefix: "prod/".into(),
                access_key_id: Some("WRITER-ID".into()),
                secret_access_key: Some("writer-secret".into()),
                object_lock_mode: Some("governance".into()),
                retention_days: Some(30),
                host: None,
                port: None,
                user: None,
                private_key_path: None,
                host_key_fingerprint: None,
                reader: Some(NewReaderIdentity {
                    access_key_id: Some("READER-ID".into()),
                    secret_access_key: Some("reader-secret".into()),
                    user: None,
                    private_key_path: None,
                    private_key_passphrase: None,
                    password: None,
                }),
            },
        )
        .expect("분리된 S3 reader 신원 등록");

        let cfg_path = config_path();
        let config = Config::load(&cfg_path).unwrap();
        let destination = &config.destinations[0];
        let reader = destination.reader.as_ref().expect("reader 저장");
        let ks = Keystore::load(&keystore_path(&cfg_path)).unwrap();
        let kek = ks
            .unlock_kek(&Credential::Password(Zeroizing::new(pw)))
            .unwrap();
        let dec = DekEncryptor::new(kek, FIELD_WRAP_GEN);

        assert_eq!(
            &*dec
                .unwrap_field(reader.access_key_id.as_ref().unwrap())
                .unwrap(),
            "READER-ID"
        );
        assert_eq!(
            &*dec
                .unwrap_field(reader.secret_access_key.as_ref().unwrap())
                .unwrap(),
            "reader-secret"
        );

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    #[test]
    fn add_destination_rejects_s3_without_reader_identity() {
        let _env = crate::paths::env_guard();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let pw = init_ks(dir);

        let error = add_destination(
            pw,
            NewDestination {
                name: "s3-no-reader".into(),
                kind: "s3".into(),
                path: None,
                endpoint: Some("http://127.0.0.1:9000".into()),
                region: Some("us-east-1".into()),
                bucket: Some("backups".into()),
                prefix: "prod/".into(),
                access_key_id: Some("WRITER-ID".into()),
                secret_access_key: Some("writer-secret".into()),
                object_lock_mode: Some("governance".into()),
                retention_days: Some(30),
                host: None,
                port: None,
                user: None,
                private_key_path: None,
                host_key_fingerprint: None,
                reader: None,
            },
        )
        .expect_err("복구 reader 없는 S3 등록은 거부");

        assert!(error.contains("reader"), "{error}");

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    #[test]
    fn add_destination_rejects_config_that_schema_validator_would_reject() {
        let _env = crate::paths::env_guard();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let pw = init_ks(dir);

        let error = add_destination(
            pw,
            NewDestination {
                name: "s3-missing-endpoint".into(),
                kind: "s3".into(),
                path: None,
                endpoint: None,
                region: Some("us-east-1".into()),
                bucket: Some("backups".into()),
                prefix: "prod/".into(),
                access_key_id: Some("WRITER-ID".into()),
                secret_access_key: Some("writer-secret".into()),
                object_lock_mode: Some("governance".into()),
                retention_days: Some(30),
                host: None,
                port: None,
                user: None,
                private_key_path: None,
                host_key_fingerprint: None,
                reader: Some(NewReaderIdentity {
                    access_key_id: Some("READER-ID".into()),
                    secret_access_key: Some("reader-secret".into()),
                    user: None,
                    private_key_path: None,
                    private_key_passphrase: None,
                    password: None,
                }),
            },
        )
        .expect_err("schema validator가 거부할 config는 저장하지 않음");

        assert!(error.contains("endpoint"), "{error}");
        assert!(
            !config_path().exists(),
            "유효하지 않은 config는 저장하지 않음"
        );

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    #[test]
    fn add_destination_ssh_encrypts_reader_key_passphrase() {
        let _env = crate::paths::env_guard();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let pw = init_ks(dir);

        add_destination(
            pw.clone(),
            NewDestination {
                name: "ssh-reader".into(),
                kind: "ssh".into(),
                path: Some("/srv/backups".into()),
                endpoint: None,
                region: None,
                bucket: None,
                prefix: String::new(),
                access_key_id: None,
                secret_access_key: None,
                object_lock_mode: None,
                retention_days: None,
                host: Some("archive.example.test".into()),
                port: Some(22),
                user: Some("backup-writer".into()),
                private_key_path: Some("C:/keys/writer".into()),
                host_key_fingerprint: Some("SHA256:pinned".into()),
                reader: Some(NewReaderIdentity {
                    access_key_id: None,
                    secret_access_key: None,
                    user: Some("restore-reader".into()),
                    private_key_path: Some("C:/keys/reader".into()),
                    private_key_passphrase: Some("reader-key-passphrase".into()),
                    password: None,
                }),
            },
        )
        .expect("분리된 SSH reader 신원 등록");

        let cfg_path = config_path();
        let config = Config::load(&cfg_path).unwrap();
        let reader = config.destinations[0].reader.as_ref().expect("reader 저장");
        assert_eq!(reader.user.as_deref(), Some("restore-reader"));
        assert_eq!(
            reader
                .private_key_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .as_deref(),
            Some("C:/keys/reader")
        );
        let ks = Keystore::load(&keystore_path(&cfg_path)).unwrap();
        let kek = ks
            .unlock_kek(&Credential::Password(Zeroizing::new(pw)))
            .unwrap();
        let dec = DekEncryptor::new(kek, FIELD_WRAP_GEN);
        assert_eq!(
            &*dec
                .unwrap_field(reader.private_key_passphrase.as_ref().unwrap())
                .unwrap(),
            "reader-key-passphrase"
        );

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    #[test]
    fn add_destination_ssh_encrypts_reader_password() {
        let _env = crate::paths::env_guard();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let pw = init_ks(dir);

        add_destination(
            pw.clone(),
            NewDestination {
                name: "ssh-password-reader".into(),
                kind: "ssh".into(),
                path: Some("/srv/backups".into()),
                endpoint: None,
                region: None,
                bucket: None,
                prefix: String::new(),
                access_key_id: None,
                secret_access_key: None,
                object_lock_mode: None,
                retention_days: None,
                host: Some("archive.example.test".into()),
                port: Some(22),
                user: Some("backup-writer".into()),
                private_key_path: Some("C:/keys/writer".into()),
                host_key_fingerprint: Some("SHA256:pinned".into()),
                reader: Some(NewReaderIdentity {
                    access_key_id: None,
                    secret_access_key: None,
                    user: Some("restore-reader".into()),
                    private_key_path: None,
                    private_key_passphrase: None,
                    password: Some("reader-password".into()),
                }),
            },
        )
        .expect("SSH reader 비밀번호 신원 등록");

        let cfg_path = config_path();
        let config = Config::load(&cfg_path).unwrap();
        let reader = config.destinations[0].reader.as_ref().expect("reader 저장");
        let ks = Keystore::load(&keystore_path(&cfg_path)).unwrap();
        let kek = ks
            .unlock_kek(&Credential::Password(Zeroizing::new(pw)))
            .unwrap();
        let dec = DekEncryptor::new(kek, FIELD_WRAP_GEN);
        assert_eq!(
            &*dec.unwrap_field(reader.password.as_ref().unwrap()).unwrap(),
            "reader-password"
        );

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    #[test]
    fn add_destination_rejects_ssh_without_reader_identity() {
        let _env = crate::paths::env_guard();

        let error = add_destination(
            String::new(),
            NewDestination {
                name: "ssh-no-reader".into(),
                kind: "ssh".into(),
                path: Some("/srv/backups".into()),
                endpoint: None,
                region: None,
                bucket: None,
                prefix: String::new(),
                access_key_id: None,
                secret_access_key: None,
                object_lock_mode: None,
                retention_days: None,
                host: Some("archive.example.test".into()),
                port: Some(22),
                user: Some("backup-writer".into()),
                private_key_path: Some("C:/keys/writer".into()),
                host_key_fingerprint: Some("SHA256:pinned".into()),
                reader: None,
            },
        )
        .expect_err("복구 reader 없는 SSH 등록은 거부");

        assert!(error.contains("reader"), "{error}");

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    #[test]
    fn add_job_enforces_referential_integrity() {
        let _env = crate::paths::env_guard();
        let temp_dir = tempfile::tempdir().unwrap();
        let dir = temp_dir.path();
        let pw = init_ks(dir);

        add_database(
            pw.clone(),
            NewDatabase {
                alias: "pg".into(),
                engine: "postgresql".into(),
                host: "h".into(),
                port: 5432,
                dbname: "app".into(),
                user: "u".into(),
                password: "p".into(),
                dump_globals: true,
            },
        )
        .unwrap();
        add_destination(
            "".into(),
            NewDestination {
                name: "nas".into(),
                kind: "local".into(),
                path: Some(r"D:\b".into()),
                endpoint: None,
                region: None,
                bucket: None,
                prefix: String::new(),
                access_key_id: None,
                secret_access_key: None,
                object_lock_mode: None,
                retention_days: None,
                host: None,
                port: None,
                user: None,
                private_key_path: None,
                host_key_fingerprint: None,
                reader: None,
            },
        )
        .unwrap();

        // 알 수 없는 소스 → 거부.
        assert!(mk_job("j1", "nope", &["nas"]).is_err());
        // 알 수 없는 목적지 → 거부.
        assert!(mk_job("j1", "pg", &["ghost"]).is_err());
        // 정상.
        mk_job("nightly", "pg", &["nas"]).expect("잡 등록");
        // 중복 이름 → 거부.
        assert!(mk_job("nightly", "pg", &["nas"]).is_err());

        let jobs = list_jobs().unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].name, "nightly");
        assert_eq!(jobs[0].destinations, vec!["nas".to_string()]);
        assert!(jobs[0].enabled);

        std::env::remove_var("RESTOREPLANE_CONFIG");
    }

    fn mk_job(name: &str, source: &str, dests: &[&str]) -> Result<(), String> {
        add_job(NewJob {
            name: name.into(),
            source: source.into(),
            destinations: dests.iter().map(|s| s.to_string()).collect(),
            required_immutability: "none".into(),
            retention_daily: 7,
            retention_weekly: 4,
            retention_monthly: 6,
            verify_level: 1,
            schedule: "0 3 * * *".into(),
        })
    }
}
