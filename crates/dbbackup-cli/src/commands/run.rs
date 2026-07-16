// crates/dbbackup-cli/src/commands/run.rs
use std::path::Path;

use dbbackup_config::Config;

use crate::cli::RunArgs;

pub fn run(config_path: &Path, args: RunArgs) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    let jobs: Vec<_> = config
        .jobs
        .iter()
        .filter(|j| j.enabled)
        .filter(|j| args.db.as_deref().map_or(true, |db| j.source == db))
        .collect();

    if jobs.is_empty() {
        anyhow::bail!("no matching enabled jobs for --db={:?}", args.db);
    }

    for job in jobs {
        if args.dry_run {
            println!(
                "[dry-run] would run job '{}': {} -> {:?}",
                job.name, job.source, job.destinations
            );
            continue;
        }
        anyhow::bail!("job execution not implemented yet (see section S05 Pipeline)");
    }
    Ok(())
}
