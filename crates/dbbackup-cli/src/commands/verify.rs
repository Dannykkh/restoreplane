// S09 — verify 배선. 슬라이스 1에서는 출처 인증 게이트(서명 재검증)까지 완전 배선한다.
// L1+ 복호 검증은 DEK 복구(S02 WrapSet 동봉)가 필요한데 run이 아직 WrapSet을 산출하지 않으므로
// (슬라이스 1 한계) 여기서는 서명 게이트 통과를 확인하고 다음 단계 요건을 안내한다.
use std::path::Path;

use dbbackup_catalog::{EmbeddedKeyVerifier, LocalManifestSource, ManifestSource};
use dbbackup_verify::load_trusted_inputs;

use crate::cli::VerifyArgs;

pub fn run(config_path: &Path, args: VerifyArgs) -> anyhow::Result<()> {
    let config = dbbackup_config::Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    let spool_dir = &config.policy.spool_dir;
    let source = LocalManifestSource::new(spool_dir);
    let verifier = EmbeddedKeyVerifier;

    // 대상 매니페스트 키 결정(--backup id 힌트 또는 전부).
    let keys = pick_keys(&source, args.backup.as_deref())?;
    if keys.is_empty() {
        anyhow::bail!("검증할 매니페스트가 없습니다(spool_dir={})", spool_dir.display());
    }

    let level = args.level.unwrap_or(1);
    for key in &keys {
        // 출처 인증 게이트: 미서명/위조는 여기서 Err → 즉시 중단(C3).
        let inputs = load_trusted_inputs(&source, &verifier, key)?;
        println!(
            "게이트 통과(서명 유효): backup={} job={} — verified(gate)",
            inputs.db_manifest.backup_id, inputs.db_manifest.job
        );
    }
    println!(
        "요청 레벨 L{level}: 파일무결성(L1) 이상 복호 검증은 DEK 복구(S02 WrapSet 동봉)가 필요합니다 \
         — run이 WrapSet을 산출하는 슬라이스에서 활성화됩니다."
    );
    Ok(())
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
