//! config 편집 커맨드(인프로세스) — [[database]] 등록/조회.
//!
//! CLI add-db가 쓰던 NotYetImplementedEncryptor 스텁 대신 실제 DekEncryptor로 비밀번호를 봉투
//! 암호화한다(EncField = enc:<wrap_gen>:<b64>). KEK를 마스터 비밀번호로 언락해 필드 DEK로 쓰며,
//! wrap_gen은 키스토어 init 세대(1)와 맞춘다(rotate-key 시 재래핑 대상 식별).
//!
//! 주의(백엔드 미배선 seam): run.rs는 현재 DB 비밀번호를 env(PGPASSWORD/MYSQL_PWD)로 받는다.
//! 즉 여기서 기록한 EncField를 백업 실행이 아직 복호화해 쓰지는 않는다(run 측 unwrap 배선은 별도
//! 백엔드 작업). 이 커맨드는 "config를 올바르게(암호화된 시크릿 포함) 기록"하는 계층까지 책임진다.

use dbbackup_config::schema::{Config, Database};
use dbbackup_core::{Engine, SecretEncryptor};
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
}
