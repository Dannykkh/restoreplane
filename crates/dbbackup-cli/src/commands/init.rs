// crates/dbbackup-cli/src/commands/init.rs
//
// 스텁: 마스터 비밀번호/DEK/복구코드/DPAPI 등록은 S02(SecretManager) 소관.
// config가 아직 없을 수 있으므로 여기서는 Config::load를 시도하지 않는다.
use std::path::Path;

use crate::cli::InitArgs;

pub fn run(_config_path: &Path, _args: InitArgs) -> anyhow::Result<()> {
    anyhow::bail!("init is not implemented yet (see section S02 SecretManager)");
}
