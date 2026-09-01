// S09 9.3 — DEK unwrap. CLI 서브셰이프 → S02 Credential 매핑(3경로). S09는 DEK 원본을
// 직접 만들지 않고 S02 SecretManager::unwrap에 위임한다(P0-5 계층 분리).

use restoreplane_secrets::{Credential, Dek, SecretError, SecretManager, WrapSet};
use zeroize::Zeroizing;

/// CLI 서브셰이프에서 결정되는 자격증명 경로.
pub enum UnlockMode {
    /// 기본(무인): DPAPI Machine-scope(Windows).
    Unattended,
    /// 대화형 마스터 비밀번호.
    Password(Zeroizing<String>),
    /// --with-recovery-code: 복구코드 경로.
    Recovery(Zeroizing<String>),
}

/// 자격증명 경로에 맞는 스탠자로 DEK를 복구한다.
pub fn unwrap_dek(
    sm: &SecretManager,
    wrap_set: &WrapSet,
    mode: UnlockMode,
) -> Result<Dek, SecretError> {
    let cred = match mode {
        UnlockMode::Unattended => Credential::Dpapi,
        UnlockMode::Password(pw) => Credential::Password(pw),
        UnlockMode::Recovery(code) => Credential::Recovery(code),
    };
    sm.unwrap(wrap_set, &cred)
}
