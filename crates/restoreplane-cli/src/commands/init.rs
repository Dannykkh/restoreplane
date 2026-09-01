// crates/restoreplane-cli/src/commands/init.rs
//! 최초 설정: 마스터 비밀번호로 KEK 봉투 구성, 복구코드 1회 표시, DPAPI 등록(Windows, 실패 허용),
//! 지속 ed25519 서명 키페어 생성, 키스토어 기록. config가 아직 없을 수 있으므로 Config::load는 하지 않는다.

use std::path::Path;

use restoreplane_secrets::Keystore;

use crate::cli::InitArgs;
use crate::commands::keystore_util::{keyfile_path, keystore_path, resolve_master_pw};

pub fn run(config_path: &Path, args: InitArgs) -> anyhow::Result<()> {
    let ks_path = keystore_path(config_path);
    if Keystore::exists(&ks_path) && !args.force {
        anyhow::bail!(
            "키스토어가 이미 존재합니다({}). 덮어쓰려면 --force를 쓰세요(기존 백업의 DEK 복구 불가 위험).",
            ks_path.display()
        );
    }

    // 마스터 비밀번호: 무인/테스트는 env, 대화형은 프롬프트.
    let master_pw = resolve_master_pw("새 마스터 비밀번호: ", true)?;

    // 키스토어 초기화 — KEK 발급/3경로 wrap + 서명 키페어(seed는 KEK로 wrap) + 복구코드 1회 반환.
    let (keystore, recovery_code) = Keystore::init(&master_pw, keyfile_path(config_path))?;
    keystore.save(&ks_path)?;

    println!("키스토어를 생성했습니다: {}", ks_path.display());
    println!();
    println!("=== 복구코드(단 한 번만 표시됩니다 — 안전한 곳에 보관하세요) ===");
    println!("    {}", *recovery_code);
    println!("===============================================================");
    println!();
    println!(
        "서명 공개키(fingerprint 대조용): {}",
        keystore.verifying_key_hex
    );
    #[cfg(windows)]
    println!("DPAPI 무인 실행 경로가 등록되었습니다(실패 시 위 경고 참조).");
    #[cfg(not(windows))]
    println!("비Windows: DPAPI 경로 없이 비번/복구코드 경로만 활성화됩니다.");

    Ok(())
}
