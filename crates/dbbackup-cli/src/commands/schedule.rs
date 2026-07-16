// crates/dbbackup-cli/src/commands/schedule.rs
//
// 스텁: Windows Task Scheduler(schtasks) 등록/갱신/삭제는 S10(Scheduler) 소관.
use std::path::Path;

use crate::cli::ScheduleArgs;

pub fn run(config_path: &Path, args: ScheduleArgs) -> anyhow::Result<()> {
    let config = dbbackup_config::Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;
    anyhow::bail!(
        "schedule (job='{}', remove={}) is not implemented yet (see section S10 Scheduler)",
        args.job,
        args.remove
    );
}
