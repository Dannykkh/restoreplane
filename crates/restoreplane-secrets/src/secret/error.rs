// crates/restoreplane-secrets/src/secret/error.rs
//! SecretManager 전 경로 공용 에러. 키 자료(DEK/KEK/비번/복구코드)는 절대 담지 않는다(R11).

/// SecretManager 에러. 실패 원인만 표기하고 평문 키 자료는 노출하지 않는다.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("kdf(Argon2id) 유도 실패: {0}")]
    Kdf(String),
    #[error("DEK wrap 실패")]
    WrapFailed,
    #[error("DEK unwrap 실패(잘못된 자격증명 또는 손상된 스탠자)")]
    UnwrapFailed,
    #[error("복호된 DEK 길이가 32B가 아님")]
    BadDekLen,
    #[error("자격증명과 스탠자 종류가 일치하지 않음")]
    WrongStanza,
    #[error("제공된 자격증명으로 사용할 수 있는 스탠자가 없음")]
    NoUsableStanza,
    #[error("서명 검증 거부(미서명/위조) — 복원 HARD FAIL")]
    SignatureRejected,
    #[error("서명 개인키가 로드되지 않음(오프라인)")]
    SigningKeyMissing,
    #[error("복구코드 검증 실패")]
    RecoveryRejected,
    #[error("DPAPI 오류: {0}")]
    Dpapi(String),
    #[error("이 플랫폼에서는 DPAPI 경로를 사용할 수 없음")]
    DpapiUnavailable,
    #[error("키스토어 오류: {0}")]
    Keystore(String),
    #[error("입출력 오류: {0}")]
    Io(#[from] std::io::Error),
}
