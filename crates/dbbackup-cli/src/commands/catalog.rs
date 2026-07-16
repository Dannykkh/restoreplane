// S08 — catalog rebuild --from <dest> 배선.
// 목적지(로컬 경로)에 동봉된 서명 매니페스트만으로 SQLite 카탈로그를 재구성한다(P0-6).
// 서명 공개키는 config에 아직 핀되기 전(슬라이스 1)이라 매니페스트 동봉키로 TOFU 검증한다.
use std::path::{Path, PathBuf};

use dbbackup_catalog::{
    catalog_rebuild, CatalogWriterHandle, EmbeddedKeyVerifier, LocalManifestSource, RebuildArgs,
};

use crate::cli::CatalogAction;

pub fn run(config_path: &Path, action: CatalogAction) -> anyhow::Result<()> {
    let config = dbbackup_config::Config::load(config_path)?;
    match action {
        CatalogAction::Rebuild(args) => {
            // --from은 슬라이스 1에서 로컬 목적지 루트 경로로 해석한다(S3/SSH는 후속 슬라이스).
            let from_root = PathBuf::from(&args.from_destination);
            if !from_root.exists() {
                anyhow::bail!(
                    "목적지 경로 '{}'가 존재하지 않습니다(슬라이스 1 rebuild는 로컬 경로만 지원)",
                    from_root.display()
                );
            }

            let spool_dir = &config.policy.spool_dir;
            std::fs::create_dir_all(spool_dir)?;
            let db_path = spool_dir.join("catalog.db");

            tracing::warn!(
                "서명 공개키가 config에 핀되기 전이라 매니페스트 동봉키로 TOFU 검증합니다(슬라이스 1)."
            );

            let source = LocalManifestSource::new(&from_root);
            let verifier = EmbeddedKeyVerifier;
            let writer = CatalogWriterHandle::spawn(db_path.clone())?;
            let report = catalog_rebuild(
                &source,
                &verifier,
                &writer,
                RebuildArgs {
                    dest_name: args.from_destination.clone(),
                    dest_kind: "local".to_string(),
                    ..Default::default()
                },
            )?;

            println!(
                "카탈로그 재구성 완료({}): 반영 {}건, 거부(미서명/변조) {}건",
                db_path.display(),
                report.accepted,
                report.rejected_unsigned_or_tampered.len()
            );
            for k in &report.rejected_unsigned_or_tampered {
                println!("  [거부] {k}");
            }
            Ok(())
        }
    }
}
