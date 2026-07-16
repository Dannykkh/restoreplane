// S09 — restore 배선. 키스토어의 핀된 공개키로 서명 게이트를 통과시키고, KEK unlock →
// 매니페스트 동봉 WrapSet에서 DEK 복구 → --decrypt-only는 평문 덤프를 --to 경로에 산출한다.
// 샌드박스 복원(비-decrypt-only)은 Docker가 필요하므로 슬라이스 1에서는 명확히 미배선.
// 카탈로그 SQLite 값은 복원 판단 근거로 쓰지 않는다(H5).
use std::path::{Path, PathBuf};

use dbbackup_catalog::{LocalManifestSource, ManifestSource, PinnedKeyVerifier};
use dbbackup_secrets::Keystore;
use dbbackup_verify::{load_trusted_inputs, ImagePolicy, RestoreTarget, Restorer};

use crate::cli::RestoreArgs;
use crate::commands::keystore_util;

pub fn run(config_path: &Path, args: RestoreArgs) -> anyhow::Result<()> {
    let config = dbbackup_config::Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    let ks_path = keystore_util::keystore_path(config_path);
    if !Keystore::exists(&ks_path) {
        anyhow::bail!(
            "키스토어가 없습니다({}) — 먼저 `dbbackup init`.",
            ks_path.display()
        );
    }
    let keystore = Keystore::load(&ks_path)?;
    let vk = keystore.verifying_key()?;

    // --from-destination은 로컬 경로면 그대로, 아니면 spool_dir로 해석(슬라이스 1: 로컬만).
    let root = {
        let p = PathBuf::from(&args.from_destination);
        if p.exists() {
            p
        } else {
            config.policy.spool_dir.clone()
        }
    };
    let source = LocalManifestSource::new(&root);
    let verifier = PinnedKeyVerifier::new(vk);

    let key = find_manifest_key(&source, &args.backup)?.ok_or_else(|| {
        anyhow::anyhow!(
            "backup '{}'에 해당하는 서명 매니페스트를 목적지에서 찾을 수 없음",
            args.backup
        )
    })?;

    // 출처 인증 게이트(C3): 미서명/위조는 DEK unwrap·복호 이전에 즉시 거부.
    let inputs = load_trusted_inputs(&source, &verifier, &key)?;
    println!(
        "게이트 통과(서명 유효): backup={} spool_object={}",
        inputs.db_manifest.backup_id, inputs.db_manifest.spool_object
    );

    if !args.decrypt_only {
        anyhow::bail!(
            "샌드박스 복원에는 Docker가 필요합니다 — 슬라이스 1은 --decrypt-only만 지원합니다."
        );
    }
    let to = args
        .to
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--decrypt-only에는 평문 산출 경로 --to가 필요합니다"))?;

    // KEK unlock(--with-recovery-code면 복구코드, 아니면 비번 env/프롬프트).
    let cred = keystore_util::resolve_unlock_credential(args.with_recovery_code, true)?;
    let kek = keystore
        .unlock_kek(&cred)
        .map_err(|e| anyhow::anyhow!("KEK unlock 실패(자격증명 확인): {e}"))?;
    let wrap_set = inputs
        .wrap_set
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("매니페스트에 WrapSet이 동봉되지 않음 — DEK 복구 불가"))?;
    let dek = keystore
        .unwrap_dek(&kek, wrap_set)
        .map_err(|e| anyhow::anyhow!("DEK unwrap 실패: {e}"))?;

    let restorer = Restorer {
        source: &source,
        dek,
        docker_bin: std::path::PathBuf::from("docker"),
        image_policy: ImagePolicy::default(),
    };
    let out_path = PathBuf::from(&to);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        restorer
            .restore(&inputs, RestoreTarget::DecryptOnly(out_path))
            .await
    })?;

    println!("평문 덤프를 기록했습니다: {to}");
    Ok(())
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
