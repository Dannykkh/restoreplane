// crates/dbbackup-cli/src/commands/restore.rs
//
// 스텁: 다운로드/복호화/임포트 경로는 S09(Restore/Verify) 소관.
use std::path::Path;

use crate::cli::RestoreArgs;

pub fn run(config_path: &Path, args: RestoreArgs) -> anyhow::Result<()> {
    let config = dbbackup_config::Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;
    anyhow::bail!(
        "restore of backup '{}' from destination '{}' is not implemented yet (see section S09)",
        args.backup,
        args.from_destination
    );
}
