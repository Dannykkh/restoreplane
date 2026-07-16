// crates/dbbackup-secrets/src/secret/rotate.rs
//! rotate-key(R8) + keys status(R9). 덤프 본체는 재암호화하지 않고 DEK(32B)만 재-wrap한다.
//!
//! WORM 목적지에서는 delete 없이 새 세대 WrapSet을 append하며, unwrap은 최고 dek_wrap_gen을
//! 우선한다. S08 Catalog 연동은 상위 계층이 담당하고, 여기서는 WrapSet 단위 재-wrap을 제공한다.

use zeroize::Zeroizing;

use crate::secret::error::SecretError;
use crate::secret::wrap::{self, WrapSet, WrapStanza};
use crate::secret::{Credential, SecretManager};

/// rotate-key 결과 리포트. 덤프 재업로드는 항상 0건(회전 비용 = DEK 크기 * N).
#[derive(Default, Debug, Clone)]
pub struct RotateReport {
    pub rewrapped: usize,
    pub next_gen: u32,
}

/// 기존 자격증명으로 unwrap한 DEK를 새 비번/복구코드 KEK로 재-wrap한다(덤프 불변).
pub(crate) fn rotate_wrapset(
    mgr: &SecretManager,
    set: &WrapSet,
    old: &Credential,
    new_pw: &Zeroizing<String>,
    new_recovery: &Zeroizing<String>,
    next_gen: u32,
) -> Result<WrapSet, SecretError> {
    // 값싼 32B 복구 — 덤프는 손대지 않는다.
    let dek = mgr.unwrap(set, old)?;
    let mut stanzas = Vec::new();
    stanzas.push(wrap::wrap_password(&dek, new_pw.as_bytes())?);
    stanzas.push(wrap::wrap_recovery(&dek, new_recovery.as_bytes())?);
    #[cfg(windows)]
    {
        stanzas.push(mgr.wrap_dpapi(&dek)?);
    }
    Ok(WrapSet {
        dek_wrap_gen: next_gen,
        stanzas,
    })
}

/// keys status(R9) 요약 문자열 — 3경로 존재/세대/DPAPI 등록 상태를 사람이 읽게 표기한다.
pub(crate) fn keys_status(mgr: &SecretManager, set: &WrapSet) -> String {
    let has_pw = set
        .stanzas
        .iter()
        .any(|s| matches!(s, WrapStanza::Argon2id { .. }));
    let has_rec = set
        .stanzas
        .iter()
        .any(|s| matches!(s, WrapStanza::RecoveryCode { .. }));
    let dpapi_ids: Vec<String> = set
        .stanzas
        .iter()
        .filter_map(|s| match s {
            WrapStanza::Dpapi {
                machine_id,
                keyfile_id,
                ..
            } => Some(format!("{}(keyfile {})", machine_id, keyfile_id)),
            _ => None,
        })
        .collect();

    let mark = |b: bool| if b { "[x]" } else { "[ ]" };
    let signing = if mgr.has_signing_key() {
        "개인키 로드됨"
    } else {
        "개인키: 오프라인(로컬 없음)"
    };
    let dpapi_line = if dpapi_ids.is_empty() {
        "미등록 → register-dpapi 필요".to_string()
    } else {
        dpapi_ids.join(", ")
    };

    format!(
        "Signing:     ed25519 pub 핀됨 (fp {}) / {}\n\
         Wrap 경로:   {} Argon2id(비번)  {} 복구코드  {} DPAPI\n\
         DEK 세대:    dek_wrap_gen = {}\n\
         DPAPI 등록:  {}\n\
         복구코드:    {} (원본 코드는 표시되지 않음)",
        mgr.verifying_fingerprint(),
        signing,
        mark(has_pw),
        mark(has_rec),
        mark(!dpapi_ids.is_empty()),
        set.dek_wrap_gen,
        dpapi_line,
        if has_rec { "등록됨(검증자 존재)" } else { "미등록" },
    )
}
