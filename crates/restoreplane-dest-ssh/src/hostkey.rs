// crates/restoreplane-dest-ssh/src/hostkey.rs
//! S07 §1 — host-key 핀 정책. 이 크레이트의 보안 하중(H6/P2-1).
//!
//! STRICT 핀: config `host_key_fingerprint`(SHA256)와 **정확 일치할 때만** 서버 키를 수용한다.
//! 미지/불일치는 연결 거부. TOFU(첫 접속 자동 신뢰)나 accept-any는 금지 — blind-accept는 DB 터널을
//! 그대로 MITM 통로로 만든다. 이 도구는 항상 config 핀을 갖고 접속하므로 불일치는 언제나 "위험"이다.

use russh::keys::ssh_key::{HashAlg, PublicKey};

/// config에서 온 고정 지문. 형식: "SHA256:<base64>" (ssh-keygen -lf 와 동일, R2).
#[derive(Clone, Debug)]
pub struct HostKeyPolicy {
    pub pinned_fpr: String,
    pub label: String, // 로그용(host:port) — 시크릿 아님(R12)
}

#[derive(Debug, thiserror::Error)]
pub enum HostKeyError {
    #[error("host key가 핀과 불일치(MITM 가능): {label} 지문={got} != 핀={want} — 연결 거부")]
    Changed {
        label: String,
        got: String,
        want: String,
    },
}

impl HostKeyPolicy {
    pub fn new(pinned_fpr: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            pinned_fpr: pinned_fpr.into(),
            label: label.into(),
        }
    }

    /// 서버가 제시한 공개키의 SHA256 지문을 핀과 비교. **정확 일치만 통과**(R1/R2).
    /// 첫 접속이든 재접속이든 핀을 갖고 있으므로 불일치=위험(Changed)으로 수렴한다(R11).
    pub fn verify(&self, server_key: &PublicKey) -> Result<(), HostKeyError> {
        let got = server_key.fingerprint(HashAlg::Sha256).to_string();
        if fingerprints_match(&got, &self.pinned_fpr) {
            tracing::info!(label = %self.label, "host key 지문 일치 — 수용");
            Ok(())
        } else {
            Err(HostKeyError::Changed {
                label: self.label.clone(),
                got,
                want: self.pinned_fpr.clone(),
            })
        }
    }
}

/// 지문 문자열 비교(정확 일치). 별도 함수로 두어 인프라 없이 단위 테스트 가능(task 요구).
pub fn fingerprints_match(got: &str, pinned: &str) -> bool {
    got == pinned
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::ssh_key::PublicKey;

    // ssh-keygen -t ed25519로 생성한 실제 공개키 2개 + 각 SHA256 지문(ssh-keygen -lf).
    const PUB1: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINZYGo0V+QYJLqcDcwtClgQ8Nm2X+2JDP/qDDuQ80XS8";
    const FPR1: &str = "SHA256:+vEomyD3wxLe7sD8Pl/AWl81+S3J8DK9M38CNrsaD/4";
    const PUB2: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOtj6vRyJ5dSKqUWyK/O2JuEAWuGX7+J1SJU24iBIcEx";
    const FPR2: &str = "SHA256:0OWPeFQDediccU5/UOiz3ukINh8qnhYMvb3d+Ctbt7I";

    #[test]
    fn fingerprint_matches_ssh_keygen_format() {
        // 우리 계산 값이 ssh-keygen의 SHA256:... 와 동일해야 함(R2).
        let key = PublicKey::from_openssh(PUB1).unwrap();
        assert_eq!(key.fingerprint(HashAlg::Sha256).to_string(), FPR1);
    }

    #[test]
    fn matching_pin_accepts() {
        let key = PublicKey::from_openssh(PUB1).unwrap();
        let policy = HostKeyPolicy::new(FPR1, "bastion:22");
        assert!(policy.verify(&key).is_ok());
    }

    #[test]
    fn changed_key_rejected_as_mitm() {
        // 핀은 KEY1, 서버는 KEY2를 제시 → 거부(MITM 모사, T2 핵심).
        let key2 = PublicKey::from_openssh(PUB2).unwrap();
        let policy = HostKeyPolicy::new(FPR1, "bastion:22");
        let err = policy.verify(&key2).unwrap_err();
        match err {
            HostKeyError::Changed { got, want, .. } => {
                assert_eq!(got, FPR2);
                assert_eq!(want, FPR1);
            }
        }
    }

    #[test]
    fn comparison_function_direct() {
        assert!(fingerprints_match(FPR1, FPR1));
        assert!(!fingerprints_match(FPR1, FPR2));
    }
}
