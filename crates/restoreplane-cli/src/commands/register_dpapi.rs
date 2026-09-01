// crates/restoreplane-cli/src/commands/register_dpapi.rs
//
// 스텁: DPAPI Machine-scope 래핑 재등록은 S02(SecretManager) 소관.
use std::path::Path;

pub fn run(config_path: &Path) -> anyhow::Result<()> {
    let _ = restoreplane_config::Config::load(config_path)?;
    anyhow::bail!("register-dpapi is not implemented yet (see section S02 SecretManager)");
}
