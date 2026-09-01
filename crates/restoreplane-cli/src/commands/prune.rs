// crates/restoreplane-cli/src/commands/prune.rs
use restoreplane_config::prune::PruneProfile;

use crate::cli::PruneArgs;

/// 의도적으로 메인 config 경로(&Path)를 받지 않는다 — prune은 오직
/// PruneProfile(격리 신원)만 로드할 수 있다.
pub fn run(args: PruneArgs) -> anyhow::Result<()> {
    let profile = PruneProfile::load(&args.prune_config)?;

    if args.dry_run {
        println!(
            "[dry-run] would prune destination '{}' using profile {}",
            profile.destination,
            args.prune_config.display()
        );
        return Ok(());
    }

    crate::commands::direct_execution_guard::enforce()?;
    anyhow::bail!("prune execution not implemented yet (see section S06/S07 Destination)");
}
