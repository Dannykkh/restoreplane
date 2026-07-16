// crates/dbbackup-cli/src/commands/catalog.rs
//
// 스텁: 서명된 매니페스트로부터의 SQLite 카탈로그 재구성은 S08(Catalog) 소관.
use std::path::Path;

use crate::cli::CatalogAction;

pub fn run(config_path: &Path, action: CatalogAction) -> anyhow::Result<()> {
    let _ = dbbackup_config::Config::load(config_path)?;
    match action {
        CatalogAction::Rebuild(args) => {
            anyhow::bail!(
                "catalog rebuild from destination '{}' is not implemented yet (see section S08 Catalog)",
                args.from_destination
            )
        }
    }
}
