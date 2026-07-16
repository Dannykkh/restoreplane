//! config 편집 커맨드(인프로세스) — [[database]] 등록/조회.
//!
//! CLI add-db가 쓰던 NotYetImplementedEncryptor 스텁 대신 실제 DekEncryptor로 비밀번호를 봉투
//! 암호화한다(EncField = enc:<wrap_gen>:<b64>). KEK를 마스터 비밀번호로 언락해 필드 DEK로 쓰며,
//! wrap_gen은 키스토어 init 세대(1)와 맞춘다(rotate-key 시 재래핑 대상 식별).
//!
//! 주의(백엔드 미배선 seam): run.rs는 현재 DB 비밀번호를 env(PGPASSWORD/MYSQL_PWD)로 받는다.
//! 즉 여기서 기록한 EncField를 백업 실행이 아직 복호화해 쓰지는 않는다(run 측 unwrap 배선은 별도
//! 백엔드 작업). 이 커맨드는 "config를 올바르게(암호화된 시크릿 포함) 기록"하는 계층까지 책임진다.

use dbbackup_config::schema::{Config, Database, Destination, ObjectLockConfig};
use dbbackup_core::{DestKind, Engine, ObjectLockMode, SecretEncryptor};
use dbbackup_secrets::{Credential, DekEncryptor, Keystore};
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

    // S3 자격증명이 있으면 KEK 언락 후 EncField로 암호화.
    let (access_enc, secret_enc) = if kind == DestKind::S3
        && (dest.access_key_id.is_some() || dest.secret_access_key.is_some())
    {
        let ks = Keystore::load(&keystore_path(&cfg_path))
            .map_err(|e| format!("키스토어 로드 실패(초기 설정 필요): {e}"))?;
        let kek = ks
            .unlock_kek(&Credential::Password(Zeroizing::new(master_password)))
            .map_err(|_| "마스터 비밀번호가 올바르지 않습니다.".to_string())?;
        let enc = DekEncryptor::new(kek, FIELD_WRAP_GEN);
        let a = dest
            .access_key_id
            .as_deref()
            .map(|v| enc.wrap(v))
            .transpose()
            .map_err(|e| format!("access key 암호화 실패: {e:?}"))?;
        let s = dest
            .secret_access_key
            .as_deref()
            .map(|v| enc.wrap(v))
            .transpose()
            .map_err(|e| format!("secret key 암호화 실패: {e:?}"))?;
        (a, s)
    } else {
        (None, None)
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

    config.destinations.push(Destination {
        name: dest.name,
        kind,
        endpoint: dest.endpoint.filter(|s| !s.trim().is_empty()),
        region: dest.region.filter(|s| !s.trim().is_empty()),
        bucket: dest.bucket.filter(|s| !s.trim().is_empty()),
        prefix: dest.prefix,
        access_key_id: access_enc,
        secret_access_key: secret_enc,
        checksum_algo: None,
        object_lock,
        tunnel: None,
        host: dest.host.filter(|s| !s.trim().is_empty()),
        port: dest.port,
        user: dest.user.filter(|s| !s.trim().is_empty()),
        private_key_path: dest.private_key_path.map(Into::into),
        host_key_fingerprint: dest.host_key_fingerprint.filter(|s| !s.trim().is_empty()),
        path: dest.path.map(Into::into),
    });
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

#[cfg(test)]
mod tests {
    use super::*;
    use dbbackup_secrets::Keystore;

    fn init_ks(dir: &std::path::Path) -> String {
        let cfg = dir.join("dbbackup.toml");
        std::env::set_var("DBBACKUP_CONFIG", &cfg);
        let pw = "correct horse battery".to_string();
        let (ks, _rec) =
            Keystore::init(&Zeroizing::new(pw.clone()), dir.join("dpapi.keyfile")).unwrap();
        ks.save(&keystore_path(&cfg)).unwrap();
        pw
    }

    #[test]
    fn add_database_encrypts_password_roundtrip() {
        let _env = crate::paths::env_guard();
        let dir = std::env::temp_dir().join(format!("dbconsole-adddb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pw = init_ks(&dir);

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

        std::env::remove_var("DBBACKUP_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_destination_s3_encrypts_keys_and_local_needs_no_secret() {
        let _env = crate::paths::env_guard();
        let dir = std::env::temp_dir().join(format!("dbconsole-adddest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pw = init_ks(&dir);

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
            },
        )
        .is_err());

        std::env::remove_var("DBBACKUP_CONFIG");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
