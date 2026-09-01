use dbbackup_dest_ssh::{SshAuth, SshDeleteCfg, SshDeleteOperatorV2};
use dbbackup_pipeline::{DeleteErrorV2, DestinationDeleteV2};
use zeroize::Zeroizing;

#[tokio::test]
async fn versioned_or_unsafe_identity_is_rejected_before_ssh_connection() {
    let delete = SshDeleteOperatorV2::new(delete_cfg()).expect("delete operator");

    for (key, version) in [
        ("recovery/payload.bin", Some("version-17")),
        ("../payload.bin", None),
        ("/payload.bin", None),
        ("recovery\\payload.bin", None),
        ("recovery/\r\npayload.bin", None),
    ] {
        let error = delete
            .delete_object(key, version)
            .await
            .expect_err("unsupported identity must fail before connection");
        assert!(matches!(
            error,
            DeleteErrorV2::Backend {
                retryable: false,
                ..
            }
        ));
    }
}

#[test]
fn delete_configuration_requires_a_pinned_host_key_and_separate_identity() {
    let mut cfg = delete_cfg();
    cfg.host_key_fingerprint.clear();
    assert!(SshDeleteOperatorV2::new(cfg).is_err());

    let mut cfg = delete_cfg();
    cfg.user.clear();
    assert!(SshDeleteOperatorV2::new(cfg).is_err());
}

fn delete_cfg() -> SshDeleteCfg {
    SshDeleteCfg {
        host: "127.0.0.1".to_owned(),
        port: 9,
        user: "delete-only".to_owned(),
        host_key_fingerprint: "SHA256:+vEomyD3wxLe7sD8Pl/AWl81+S3J8DK9M38CNrsaD/4".to_owned(),
        auth: SshAuth::Password {
            password: Zeroizing::new("not-used".to_owned()),
        },
        remote_dir: "/srv/backups".to_owned(),
    }
}
