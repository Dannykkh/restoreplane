// crates/dbbackup-cli/src/commands/doctor.rs
use std::path::Path;

pub fn run(config_path: &Path) -> anyhow::Result<()> {
    let _ = dbbackup_config::Config::load(config_path)?;
    anyhow::bail!("doctor self-check not implemented yet (see section S03/S04/S11)");
}
