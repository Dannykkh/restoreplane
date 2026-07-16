// S09 — restore 배선. 슬라이스 1에서는 출처 인증 게이트(서명 재검증)까지 완전 배선한다.
// 실제 복호/샌드박스 복원은 DEK 복구(S02 WrapSet 동봉)가 필요하며, run이 WrapSet을 산출하는
// 슬라이스에서 활성화된다. 카탈로그 SQLite 값은 복원 판단 근거로 쓰지 않는다(H5).
use std::path::Path;

use dbbackup_catalog::{EmbeddedKeyVerifier, LocalManifestSource, ManifestSource};
use dbbackup_verify::load_trusted_inputs;

use crate::cli::RestoreArgs;

pub fn run(config_path: &Path, args: RestoreArgs) -> anyhow::Result<()> {
    let config = dbbackup_config::Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    // --from-destination은 슬라이스 1에서 로컬 목적지(spool_dir)로 해석한다.
    let spool_dir = &config.policy.spool_dir;
    let source = LocalManifestSource::new(spool_dir);
    let verifier = EmbeddedKeyVerifier;

    // 복원 대상 매니페스트 키를 backup id로 조회(카탈로그는 힌트로만, 신뢰 판단은 게이트에서).
    let key = find_manifest_key(&source, &args.backup)?.ok_or_else(|| {
        anyhow::anyhow!("backup '{}'에 해당하는 서명 매니페스트를 목적지에서 찾을 수 없음", args.backup)
    })?;

    // 출처 인증 게이트(C3): 미서명/위조는 DEK unwrap·복호 이전에 즉시 거부.
    let inputs = load_trusted_inputs(&source, &verifier, &key)?;
    println!(
        "게이트 통과(서명 유효): backup={} spool_object={}",
        inputs.db_manifest.backup_id, inputs.db_manifest.spool_object
    );

    if args.decrypt_only {
        anyhow::bail!(
            "--decrypt-only: 서명 게이트는 통과했으나 평문 산출에는 DEK 복구(S02 WrapSet 동봉)가 필요합니다 \
             — run이 WrapSet을 산출하는 슬라이스에서 활성화됩니다."
        );
    }
    anyhow::bail!(
        "샌드박스 복원에는 DEK 복구(S02) + Docker가 필요합니다 — 서명 게이트까지 배선 완료(슬라이스 1)."
    )
}

fn find_manifest_key(
    source: &LocalManifestSource,
    backup_id: &str,
) -> anyhow::Result<Option<String>> {
    for k in source.list_manifests(None)? {
        if let Ok(bytes) = source.read_bytes(&k) {
            if let Ok(sm) = serde_json::from_slice::<dbbackup_pipeline::SignedManifest>(&bytes) {
                if sm.manifest.backup_id.to_string() == backup_id {
                    return Ok(Some(k));
                }
            }
        }
    }
    Ok(None)
}
