// crates/restoreplane-cli/src/commands/rotate_key.rs
//
// 스텁: DEK 재래핑/복구코드 재발급/DPAPI 재등록은 S02(SecretManager) 소관.
// EncField.wrap_gen이 재래핑 대상 탐지의 근거가 된다.
use std::path::Path;

use crate::cli::RotateKeyArgs;

pub fn run(config_path: &Path, args: RotateKeyArgs) -> anyhow::Result<()> {
    let _ = restoreplane_config::Config::load(config_path)?;
    anyhow::bail!(
        "rotate-key (new_recovery_code={}, re_register_dpapi={}) is not implemented yet (see section S02 SecretManager)",
        args.new_recovery_code,
        args.re_register_dpapi
    );
}
