// crates/dbbackup-cli/src/commands/keys.rs
//
// 스텁: wrap 경로(password/recovery/dpapi) 상태 조회는 S02(SecretManager) 소관.
use std::path::Path;

use crate::cli::KeysAction;

pub fn run(config_path: &Path, action: KeysAction) -> anyhow::Result<()> {
    let _ = dbbackup_config::Config::load(config_path)?;
    match action {
        KeysAction::Status => {
            anyhow::bail!("keys status is not implemented yet (see section S02 SecretManager)")
        }
    }
}
