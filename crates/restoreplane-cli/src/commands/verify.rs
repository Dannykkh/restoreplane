// S09 — verify 배선. Local/S3/SSH에서 매니페스트와 암호문을 읽고, 키스토어의 핀된 공개키로
// 서명 게이트를 통과시키며(미서명/위조 거부),
// KEK unlock → 매니페스트 동봉 WrapSet에서 DEK 복구 → Verifier L1(복호+디코드+blake3==매니페스트).
// L2(pg_restore)/L3(Docker)는 외부 런타임이 필요하므로 슬라이스 1에서는 L1까지 실제 구동한다.

use std::path::Path;

use restoreplane_catalog::{ManifestSource, PinnedKeyVerifier};
use restoreplane_secrets::{DekEncryptor, Keystore};
use restoreplane_verify::{load_trusted_inputs, ImagePolicy, Restorer, Verifier, VerifyResult};

use crate::cli::VerifyArgs;
use crate::commands::{
    keystore_util,
    manifest_source::{build_manifest_source, select_manifest_source, ManifestSourceSpec},
    run::FIELD_WRAP_GEN,
};

const VERIFY_LEVEL_NOT_EXECUTED: &str = "VERIFY_LEVEL_NOT_EXECUTED";

pub fn run(config_path: &Path, args: VerifyArgs) -> anyhow::Result<()> {
    validate_requested_level(args.level)?;
    crate::commands::direct_execution_guard::enforce()?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(config_path, args))
}

fn validate_requested_level(requested: Option<u8>) -> anyhow::Result<u8> {
    let level = requested.unwrap_or(1);
    if level > 1 {
        anyhow::bail!(
            "{VERIFY_LEVEL_NOT_EXECUTED}: verification level L{level} is unavailable; L1 was not executed"
        );
    }
    Ok(level)
}

async fn run_async(config_path: &Path, args: VerifyArgs) -> anyhow::Result<()> {
    let config = restoreplane_config::Config::load(config_path)?;
    restoreplane_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    // 키스토어 로드: 핀된 서명 공개키 + KEK unlock 자격증명.
    let ks_path = keystore_util::keystore_path(config_path);
    if !Keystore::exists(&ks_path) {
        anyhow::bail!(
            "키스토어가 없습니다({}) — 먼저 `restoreplane init`.",
            ks_path.display()
        );
    }
    let keystore = Keystore::load(&ks_path)?;
    let vk = keystore.verifying_key()?;

    // 생략은 로컬 작업 스풀을 뜻한다. 값을 명시했다면 오타/미등록 이름은 절대 스풀로 폴백하지 않는다.
    let source_spec = match args.from_destination.as_deref() {
        Some(selector) => select_manifest_source(&config, selector)?,
        None => ManifestSourceSpec::Local {
            name: "local-spool".to_string(),
            root: config.policy.spool_dir.clone(),
        },
    };

    // KEK unlock(비번 env/프롬프트, 또는 DPAPI). 원격 reader 시크릿과 backup DEK에 공유한다.
    let cred = keystore_util::resolve_unlock_credential(false, true)?;
    let kek = keystore
        .unlock_kek(&cred)
        .map_err(|e| anyhow::anyhow!("KEK unlock 실패(자격증명 확인): {e}"))?;
    let field_enc = DekEncryptor::new(kek.clone(), FIELD_WRAP_GEN);
    let source = build_manifest_source(&source_spec, Some(&field_enc))?;
    // 핀된 공개키로 검증 — 공격자가 자기 키로 재서명한 위조 매니페스트를 거부한다(C3).
    let verifier = PinnedKeyVerifier::new(vk);

    let keys = pick_keys(source.as_ref(), args.backup.as_deref()).await?;
    if keys.is_empty() {
        anyhow::bail!(
            "검증할 매니페스트가 없습니다(source={})",
            source_spec.name()
        );
    }

    let mut failures = 0usize;
    for key in &keys {
        // 출처 인증 게이트: 미서명/위조는 여기서 Err → DEK unwrap·복호 이전에 즉시 중단(C3).
        let inputs = load_trusted_inputs(source.as_ref(), &verifier, key).await?;
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
            source: source.as_ref(),
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
    if failures > 0 {
        anyhow::bail!("{failures}건 L1 검증 실패");
    }
    Ok(())
}

/// backup id가 주어지면 그 매니페스트만, 아니면 전부.
async fn pick_keys(
    source: &dyn ManifestSource,
    backup: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let all = source.list_manifests(None).await?;
    let Some(id) = backup else { return Ok(all) };
    let mut out = Vec::new();
    for k in all {
        if let Ok(bytes) = source.read_bytes(&k).await {
            if let Ok(sm) = serde_json::from_slice::<restoreplane_pipeline::SignedManifest>(&bytes)
            {
                if sm.manifest.backup_id.to_string() == id {
                    out.push(k);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{validate_requested_level, VERIFY_LEVEL_NOT_EXECUTED};

    #[test]
    fn l2_and_l3_are_rejected_instead_of_falling_back_to_l1() {
        for level in [2, 3] {
            let error = validate_requested_level(Some(level))
                .expect_err("L2/L3 must not execute until their runtime is wired");

            assert!(
                error.to_string().contains(VERIFY_LEVEL_NOT_EXECUTED),
                "level {level} returned an unstable error: {error}"
            );
        }
    }
}
