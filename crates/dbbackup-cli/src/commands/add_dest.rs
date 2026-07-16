// crates/dbbackup-cli/src/commands/add_dest.rs
//
// 스텁: kind별 대화형 필드 수집과 자격증명 래핑은 S02(SecretManager) +
// S06/S07(Destination) 이후에 완성된다. add_db와 동일하게 평문 폴백은 금지.
use std::path::Path;

use crate::cli::AddDestArgs;

pub fn run(config_path: &Path, args: AddDestArgs) -> anyhow::Result<()> {
    let _ = dbbackup_config::Config::load(config_path)?;
    anyhow::bail!(
        "add-dest '{}' ({:?}) is not implemented yet (see sections S02/S06/S07)",
        args.name,
        args.kind
    );
}
