// crates/dbbackup-cli/src/commands/verify.rs
//
// 스텁: L1/L2/L3 검증 로직은 S09(Restore/Verify) 소관.
use std::path::Path;

use crate::cli::VerifyArgs;

pub fn run(config_path: &Path, args: VerifyArgs) -> anyhow::Result<()> {
    let config = dbbackup_config::Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;
    anyhow::bail!(
        "verify (backup={:?}, level={:?}) is not implemented yet (see section S09)",
        args.backup,
        args.level
    );
}
