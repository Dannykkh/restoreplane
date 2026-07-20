// crates/dbbackup-cli/src/commands/add_db.rs
//! DB 등록. 비밀번호는 키스토어 KEK로 봉투암호화해 config에 EncField로만 저장한다 —
//! 평문을 config에 쓰는 조용한 폴백은 하지 않는다(키스토어가 없으면 요란히 실패).
use std::path::Path;

use dbbackup_config::{schema::Database, Config};
use dbbackup_core::SecretEncryptor;
use dbbackup_secrets::{DekEncryptor, Keystore};

use crate::cli::AddDbArgs;
use crate::commands::keystore_util;
use crate::commands::run::FIELD_WRAP_GEN;

pub fn run(config_path: &Path, args: AddDbArgs) -> anyhow::Result<()> {
    let mut config = Config::load(config_path)?;

    // 키스토어 먼저 — 암호화할 수 없으면 비밀번호를 묻지도 않는다.
    let ks_path = keystore_util::keystore_path(config_path);
    if !Keystore::exists(&ks_path) {
        anyhow::bail!(
            "키스토어가 없습니다({}) — 먼저 `dbbackup init`을 실행하세요.",
            ks_path.display()
        );
    }
    let keystore = Keystore::load(&ks_path)?;
    let cred = keystore_util::resolve_unlock_credential(false, true)?;
    let kek = keystore
        .unlock_kek(&cred)
        .map_err(|e| anyhow::anyhow!("KEK unlock 실패(마스터 비밀번호 확인): {e}"))?;

    let prompt = format!("Password for '{}': ", args.alias);
    let password = zeroize::Zeroizing::new(rpassword::prompt_password(prompt)?);

    let password_enc = DekEncryptor::new(kek, FIELD_WRAP_GEN)
        .wrap(&password)
        .map_err(|e| anyhow::anyhow!("비밀번호 암호화 실패: {e}"))?;

    config.databases.push(Database {
        alias: args.alias.clone(),
        engine: args.engine.into(),
        host: args.host,
        port: args.port,
        dbname: args.dbname,
        user: args.user,
        password: password_enc,
        tunnel: args.tunnel,
        dump_globals: args.dump_globals,
        single_transaction: None,
    });

    dbbackup_config::validate(&config)
        .map_err(|errs| anyhow::anyhow!("config invalid after add-db: {errs:?}"))?;
    config.save(config_path)?;

    println!(
        "added database '{}' to {}",
        args.alias,
        config_path.display()
    );
    Ok(())
}
