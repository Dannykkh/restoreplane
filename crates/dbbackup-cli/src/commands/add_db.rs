// crates/dbbackup-cli/src/commands/add_db.rs
use std::path::Path;

use dbbackup_config::{schema::Database, Config};
use dbbackup_core::{NotYetImplementedEncryptor, SecretEncryptor};

use crate::cli::AddDbArgs;

pub fn run(config_path: &Path, args: AddDbArgs) -> anyhow::Result<()> {
    let mut config = Config::load(config_path)?;

    let prompt = format!("Password for '{}': ", args.alias);
    let password = rpassword::prompt_password(prompt)?;

    // S02(SecretManager)가 붙기 전까지는 항상 명확히 실패한다 — 평문을 config에
    // 쓰는 조용한 폴백은 절대 하지 않는다.
    let encryptor = NotYetImplementedEncryptor;
    let password_enc = encryptor
        .wrap(&password)
        .map_err(|e| anyhow::anyhow!("cannot save credential yet: {e}"))?;

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

    println!("added database '{}' to {}", args.alias, config_path.display());
    Ok(())
}
