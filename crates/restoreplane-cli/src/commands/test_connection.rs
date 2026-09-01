// crates/restoreplane-cli/src/commands/test_connection.rs
//
// 스텁: 실제 DB 접속/터널/목적지 쓰기가능 점검은 S03/S04(소스), S06/S07(목적지) 소관.
use std::path::Path;

use crate::cli::TestConnectionArgs;

pub fn run(config_path: &Path, args: TestConnectionArgs) -> anyhow::Result<()> {
    let config = restoreplane_config::Config::load(config_path)?;
    restoreplane_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;
    anyhow::bail!(
        "test-connection (--db={:?}) is not implemented yet (see sections S03/S04/S06/S07)",
        args.db
    );
}
