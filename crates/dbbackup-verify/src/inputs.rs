// S09 9.1 — 출처 인증 게이트(C3-a). 복원/검증의 모든 진입점이 통과하는 단일 게이트.
// 카탈로그는 후보 나열/힌트로만 쓰고, 실제 신뢰 판단은 목적지에서 새로 읽은 서명 바이트로만 내린다(H5).
// 서명 검증은 dbbackup-catalog gate(파이프라인 SignedManifest ed25519)를 재사용한다.
//
// S09 소비 확장 필드(server_version/schema_hash/source_row_counts/pairs_with_globals)는 파이프라인
// 서명 Manifest가 아직 채우지 않는다(슬라이스 1). 여기서는 별도 side-input(None/empty)으로 모델링해
// L3가 "없으면 skip + 기록"으로 하위호환 degrade하게 한다. S03/S04가 채우는 것이 계약(문서 동기화 대상).

use std::collections::BTreeMap;

use dbbackup_catalog::gate::{load_and_verify_manifest_for_restore, GateError, ManifestVerifier};
use dbbackup_catalog::source::ManifestSource;
use dbbackup_pipeline::Manifest;
use dbbackup_secrets::WrapSet;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RestoreInputError {
    #[error("서명 게이트 실패(미서명/위조/치환 의심): {0}")]
    Signature(#[from] GateError),
    #[error("목적지 I/O 실패: {0}")]
    Io(String),
}

/// 서명 검증을 통과한, 복원에 필요한 신뢰된 입력.
pub struct RestoreInputs {
    /// 서명 검증 완료된 DB 매니페스트(파이프라인 타입, 진실 소스).
    pub db_manifest: Manifest,
    /// PG globals 짝(서명 검증 완료). 슬라이스 1은 None(파이프라인 매니페스트에 pairing 필드 없음).
    pub globals_manifest: Option<Manifest>,
    /// DEK wrap 스탠자 묶음(사이드카가 있으면 Some). 없으면 호출부가 DEK를 직접 공급.
    pub wrap_set: Option<WrapSet>,

    // --- S09 소비 확장(하위호환 side-input; 매니페스트가 아직 채우지 않으면 None/empty) ---
    pub server_version: Option<String>,
    pub schema_hash: Option<String>,
    pub source_row_counts: BTreeMap<String, u64>,
    pub pairs_with_globals_backup_id: Option<String>,
}

impl RestoreInputs {
    /// 검증된 매니페스트만으로 최소 입력 구성(확장 필드는 degrade).
    pub fn from_manifest(db_manifest: Manifest) -> Self {
        Self {
            db_manifest,
            globals_manifest: None,
            wrap_set: None,
            server_version: None,
            schema_hash: None,
            source_row_counts: BTreeMap::new(),
            pairs_with_globals_backup_id: None,
        }
    }
}

/// 복원/검증 시작점. manifest_key(사용자 지정 또는 카탈로그 힌트)만 입력으로 받고, 실제 신뢰 판단은
/// 목적지에서 새로 읽은 바이트에 대해서만 수행한다. 미서명/위조는 여기서 즉시 Err(DEK unwrap 이전, C3).
pub async fn load_trusted_inputs(
    source: &dyn ManifestSource,
    verifier: &dyn ManifestVerifier,
    db_manifest_key: &str,
) -> Result<RestoreInputs, RestoreInputError> {
    // (1) DB 매니페스트: 서명 검증 통과해야만 반환됨(미서명/위조는 여기서 Err).
    let db_manifest =
        load_and_verify_manifest_for_restore(source, verifier, db_manifest_key).await?;

    // (2) WrapSet: 매니페스트 동봉본(서명 보호)을 우선한다. 없으면 사이드카
    //     <spool_object>.wrapset.json로 폴백(하위호환). 둘 다 없으면 None(호출부가 DEK 직접 공급).
    let wrap_set = if db_manifest.wrap_set.is_some() {
        db_manifest.wrap_set.clone()
    } else {
        let wrapset_key = format!("{}.wrapset.json", db_manifest.spool_object);
        match source.read_bytes(&wrapset_key).await {
            Ok(bytes) => serde_json::from_slice::<WrapSet>(&bytes).ok(),
            Err(_) => None,
        }
    };

    let mut inputs = RestoreInputs::from_manifest(db_manifest);
    inputs.wrap_set = wrap_set;
    Ok(inputs)
}
