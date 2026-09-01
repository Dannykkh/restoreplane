// S08 — catalog rebuild --from <dest> 배선.
// Local/S3/SSH 목적지에 동봉된 서명 매니페스트만으로 SQLite 카탈로그를 재구성한다(P0-6).
// 키스토어에 핀된 공개키를 사용하며, 원격 읽기는 writer와 분리된 reader 신원만 허용한다.
use std::path::Path;

use restoreplane_catalog::{catalog_rebuild, CatalogWriterHandle, PinnedKeyVerifier, RebuildArgs};
use restoreplane_secrets::{DekEncryptor, Keystore};

use crate::cli::CatalogAction;
use crate::commands::{
    keystore_util,
    manifest_source::{build_manifest_source, select_manifest_source},
    run::FIELD_WRAP_GEN,
};

pub fn run(config_path: &Path, action: CatalogAction) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(config_path, action))
}

async fn run_async(config_path: &Path, action: CatalogAction) -> anyhow::Result<()> {
    let config = restoreplane_config::Config::load(config_path)?;
    restoreplane_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;
    match action {
        CatalogAction::Rebuild(args) => {
            let source_spec = select_manifest_source(&config, &args.from_destination)?;

            let ks_path = keystore_util::keystore_path(config_path);
            if !Keystore::exists(&ks_path) {
                anyhow::bail!(
                    "키스토어가 없습니다({}) — 먼저 `restoreplane init`.",
                    ks_path.display()
                );
            }
            let keystore = Keystore::load(&ks_path)?;
            let verifier = PinnedKeyVerifier::new(keystore.verifying_key()?);

            // 로컬 rebuild는 공개키만 필요하다. 원격일 때만 KEK를 열어 reader 시크릿을 복호화한다.
            let field_enc = if source_spec.requires_secret_decryption() {
                let cred = keystore_util::resolve_unlock_credential(false, true)?;
                let kek = keystore
                    .unlock_kek(&cred)
                    .map_err(|e| anyhow::anyhow!("KEK unlock 실패(자격증명 확인): {e}"))?;
                Some(DekEncryptor::new(kek, FIELD_WRAP_GEN))
            } else {
                None
            };
            let source = build_manifest_source(&source_spec, field_enc.as_ref())?;

            let spool_dir = &config.policy.spool_dir;
            std::fs::create_dir_all(spool_dir)?;
            let db_path = spool_dir.join("catalog.db");

            let writer = CatalogWriterHandle::spawn(db_path.clone())?;
            let report = catalog_rebuild(
                source.as_ref(),
                &verifier,
                &writer,
                RebuildArgs {
                    dest_name: source_spec.name().to_string(),
                    dest_kind: source_spec.kind().to_string(),
                    ..Default::default()
                },
            )
            .await?;

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
