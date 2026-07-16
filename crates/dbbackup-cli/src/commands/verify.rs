// S09 — verify 배선. 키스토어의 핀된 공개키로 서명 게이트를 통과시키고(미서명/위조 거부),
// KEK unlock → 매니페스트 동봉 WrapSet에서 DEK 복구 → Verifier L1(복호+디코드+blake3==매니페스트).
// L2(pg_restore)/L3(Docker)는 외부 런타임이 필요하므로 슬라이스 1에서는 L1까지 실제 구동한다.

use std::path::Path;

use dbbackup_catalog::{LocalManifestSource, ManifestSource, PinnedKeyVerifier};
use dbbackup_secrets::Keystore;
use dbbackup_verify::{load_trusted_inputs, ImagePolicy, Restorer, VerifyResult, Verifier};

use crate::cli::VerifyArgs;
use crate::commands::keystore_util;

pub fn run(config_path: &Path, args: VerifyArgs) -> anyhow::Result<()> {
    let config = dbbackup_config::Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    // 키스토어 로드: 핀된 서명 공개키 + KEK unlock 자격증명.
    let ks_path = keystore_util::keystore_path(config_path);
    if !Keystore::exists(&ks_path) {
        anyhow::bail!("키스토어가 없습니다({}) — 먼저 `dbbackup init`.", ks_path.display());
    }
    let keystore = Keystore::load(&ks_path)?;
    let vk = keystore.verifying_key()?;

    let spool_dir = &config.policy.spool_dir;
    let source = LocalManifestSource::new(spool_dir);
    // 핀된 공개키로 검증 — 공격자가 자기 키로 재서명한 위조 매니페스트를 거부한다(C3).
    let verifier = PinnedKeyVerifier::new(vk);

    let keys = pick_keys(&source, args.backup.as_deref())?;
    if keys.is_empty() {
        anyhow::bail!("검증할 매니페스트가 없습니다(spool_dir={})", spool_dir.display());
    }

    // KEK unlock(비번 env/프롬프트, 또는 DPAPI). 실패는 DEK 복구 이전에 명확히 보고.
    let cred = keystore_util::resolve_unlock_credential(false, true)?;
    let kek = keystore
        .unlock_kek(&cred)
        .map_err(|e| anyhow::anyhow!("KEK unlock 실패(자격증명 확인): {e}"))?;

    let level = args.level.unwrap_or(1);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let mut failures = 0usize;
        for key in &keys {
            // 출처 인증 게이트: 미서명/위조는 여기서 Err → DEK unwrap·복호 이전에 즉시 중단(C3).
            let inputs = load_trusted_inputs(&source, &verifier, key)?;
            let wrap_set = inputs.wrap_set.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "backup={} 매니페스트에 WrapSet이 동봉되지 않음 — DEK 복구 불가",
                    inputs.db_manifest.backup_id
                )
            })?;
            let dek = keystore
                .unwrap_dek(&kek, wrap_set)
                .map_err(|e| anyhow::anyhow!("DEK unwrap 실패: {e}"))?;

            let restorer = Restorer {
                source: &source,
                dek,
                docker_bin: std::path::PathBuf::from("docker"),
                image_policy: ImagePolicy::default(),
            };
            let verifier_l1 = Verifier { restorer };
            let outcome = verifier_l1.l1_file_integrity(&inputs).await?;
            println!(
                "backup={} job={} — {} ({})",
                inputs.db_manifest.backup_id,
                inputs.db_manifest.job,
                outcome.label(),
                outcome.notes
            );
            if outcome.result != VerifyResult::Pass {
                failures += 1;
            }
        }
        if level >= 2 {
            println!(
                "요청 레벨 L{level}: L2(pg_restore)/L3(Docker) 검증은 외부 런타임이 필요합니다 — L1까지 구동했습니다."
            );
        }
        if failures > 0 {
            anyhow::bail!("{failures}건 L1 검증 실패");
        }
        Ok::<_, anyhow::Error>(())
    })
}

/// backup id가 주어지면 그 매니페스트만, 아니면 전부.
fn pick_keys(source: &LocalManifestSource, backup: Option<&str>) -> anyhow::Result<Vec<String>> {
    let all = source.list_manifests(None)?;
    let Some(id) = backup else { return Ok(all) };
    let mut out = Vec::new();
    for k in all {
        if let Ok(bytes) = source.read_bytes(&k) {
            if let Ok(sm) = serde_json::from_slice::<dbbackup_pipeline::SignedManifest>(&bytes) {
                if sm.manifest.backup_id.to_string() == id {
                    out.push(k);
                }
            }
        }
    }
    Ok(out)
}
