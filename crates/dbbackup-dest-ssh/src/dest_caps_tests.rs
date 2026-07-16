// crates/dbbackup-dest-ssh/src/dest_caps_tests.rs
//! S07 §8 — SshDest 캡 + Job 불변성 assert(R8/R9). 인프라 불필요(capabilities/assert는 연결 없이 동작).

#[cfg(test)]
mod tests {
    use crate::{HostKeyPolicy, SshAuth, SshDest};
    use dbbackup_pipeline::{
        assert_job_immutability, DestCaps, Destination, ImmutabilityKind, RequiredImmutability,
    };
    use std::sync::Arc;
    use zeroize::Zeroizing;

    fn dest(append_only: bool) -> SshDest {
        SshDest::new(
            "ssh",
            "bastion.example",
            22,
            "backup",
            HostKeyPolicy::new("SHA256:pin", "bastion.example:22"),
            SshAuth::Password { password: Zeroizing::new("pw".to_string()) },
            "/backups",
            append_only,
        )
    }

    fn caps(append_only: bool) -> DestCaps {
        dest(append_only).capabilities()
    }

    #[test]
    fn caps_none_when_not_append_only() {
        assert_eq!(caps(false).immutability, ImmutabilityKind::None);
        assert!(caps(false).resumable);
        assert!(!caps(false).checksum_required);
    }

    #[test]
    fn caps_server_append_only_when_flagged() {
        assert_eq!(caps(true).immutability, ImmutabilityKind::ServerAppendOnly);
    }

    #[test]
    fn object_lock_job_always_fails_on_ssh() {
        // R9/T12 핵심: Governance/Compliance 요구 Job은 SFTP만으로는 반드시 실패.
        let none = Arc::new(dest(false)) as Arc<dyn Destination>;
        let append = Arc::new(dest(true)) as Arc<dyn Destination>;
        assert!(assert_job_immutability(RequiredImmutability::ObjectLock, &[none.clone()]).is_err());
        assert!(assert_job_immutability(RequiredImmutability::ObjectLock, &[append.clone()]).is_err());
        assert!(assert_job_immutability(RequiredImmutability::ComplianceOnly, &[append.clone()]).is_err());
        // AppendOnlyOrLock은 append_only=true(ServerAppendOnly)일 때만 통과.
        assert!(assert_job_immutability(RequiredImmutability::AppendOnlyOrLock, &[none]).is_err());
        assert!(assert_job_immutability(RequiredImmutability::AppendOnlyOrLock, &[append]).is_ok());
    }
}
