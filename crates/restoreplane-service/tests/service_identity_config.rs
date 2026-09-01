#![cfg(windows)]

use std::fs;

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use restoreplane_service::identity_config::ServiceIdentityConfig;
use restoreplane_service::ipc::{current_process_identity, WindowsSid};
use restoreplane_service::secrets::AclPolicy;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn protected_install_identity_config_loads_only_the_declared_owner_and_administrators() {
    let temp = TempDir::new().expect("temp directory");
    let path = temp.path().join("service-identity.json");
    let service = current_process_identity()
        .expect("current identity")
        .user_sid()
        .clone();
    let owner = sid_other_than(&service, &["S-1-5-18", "S-1-5-19", "S-1-5-20"]);
    let administrator = sid_other_than(
        &service,
        &["S-1-5-21-100-200-300-500", "S-1-5-21-100-200-300-501"],
    );
    fs::write(
        &path,
        format!(
            concat!(
                "{{\"version\":1,\"ownerSid\":\"{}\",",
                "\"approvedAdministratorSids\":[\"{}\"],",
                "\"cutoverVerifier\":{{",
                "\"evidenceDirectory\":\"cutover-evidence\",",
                "\"sourceRevision\":\"1111111111111111111111111111111111111111\",",
                "\"sourceTreeDigest\":\"2222222222222222222222222222222222222222222222222222222222222222\",",
                "\"buildId\":\"3333333333333333333333333333333333333333333333333333333333333333\"",
                "}}}}"
            ),
            owner.as_str(),
            administrator.as_str()
        ),
    )
    .expect("write config");
    AclPolicy::new(service.clone(), owner.clone())
        .expect("ACL policy")
        .apply_file(&path)
        .expect("protected ACL");
    let config = ServiceIdentityConfig::load(&path, &service).expect("load identity config");
    assert_eq!(config.owner_sid(), &owner);
    assert_eq!(config.approved_administrator_sids(), &[administrator]);
    let cutover = config
        .cutover_verifier()
        .expect("protected cutover verifier configuration");
    assert_eq!(cutover.evidence_directory(), "cutover-evidence");
    assert_eq!(
        cutover.build_identity().source_revision(),
        "1111111111111111111111111111111111111111"
    );
    assert_eq!(
        cutover.build_identity().source_tree_digest(),
        "2222222222222222222222222222222222222222222222222222222222222222"
    );
    assert_eq!(
        cutover.build_identity().build_id(),
        "3333333333333333333333333333333333333333333333333333333333333333"
    );
}

#[test]
fn protected_identity_config_loads_trusted_manifest_keys_and_rejects_duplicates() {
    let temp = TempDir::new().expect("temp directory");
    let service = current_process_identity()
        .expect("current identity")
        .user_sid()
        .clone();
    let owner = sid_other_than(&service, &["S-1-5-18", "S-1-5-19", "S-1-5-20"]);
    let signing_key = SigningKey::from_bytes(&[91_u8; 32]);
    let encoded_key =
        base64::engine::general_purpose::STANDARD.encode(signing_key.verifying_key().as_bytes());
    let valid = temp.path().join("trusted-key.json");
    fs::write(
        &valid,
        serde_json::to_vec(&json!({
            "version": 1,
            "ownerSid": owner.as_str(),
            "approvedAdministratorSids": [],
            "trustedSigningKeys": [{
                "keyId": "manifest-key-2026-07",
                "ed25519PublicKeyBase64": encoded_key,
            }],
        }))
        .expect("serialize identity config"),
    )
    .expect("write identity config");
    AclPolicy::new(service.clone(), owner.clone())
        .expect("ACL policy")
        .apply_file(&valid)
        .expect("protected ACL");

    let config = ServiceIdentityConfig::load(&valid, &service).expect("load trusted key");
    assert_eq!(config.trusted_signing_keys().len(), 1);
    assert_eq!(
        config
            .trusted_signing_keys()
            .get("manifest-key-2026-07")
            .expect("trusted key"),
        &signing_key.verifying_key()
    );

    let duplicate = temp.path().join("duplicate-key.json");
    fs::write(
        &duplicate,
        serde_json::to_vec(&json!({
            "version": 1,
            "ownerSid": owner.as_str(),
            "approvedAdministratorSids": [],
            "trustedSigningKeys": [
                {
                    "keyId": "manifest-key-2026-07",
                    "ed25519PublicKeyBase64": encoded_key,
                },
                {
                    "keyId": "manifest-key-2026-07",
                    "ed25519PublicKeyBase64": encoded_key,
                }
            ],
        }))
        .expect("serialize duplicate identity config"),
    )
    .expect("write duplicate identity config");
    AclPolicy::new(service.clone(), owner)
        .expect("ACL policy")
        .apply_file(&duplicate)
        .expect("protected ACL");
    assert_eq!(
        ServiceIdentityConfig::load(&duplicate, &service)
            .expect_err("duplicate trusted key IDs must fail closed")
            .code(),
        "SERVICE_IDENTITY_CONFIG_INVALID"
    );
}

#[test]
fn missing_unknown_or_acl_mismatched_identity_config_fails_closed() {
    let temp = TempDir::new().expect("temp directory");
    let service = current_process_identity()
        .expect("current identity")
        .user_sid()
        .clone();
    assert_eq!(
        ServiceIdentityConfig::load(&temp.path().join("missing.json"), &service)
            .expect_err("missing config must fail")
            .code(),
        "SERVICE_IDENTITY_CONFIG_IO"
    );

    let owner = sid_other_than(&service, &["S-1-5-18", "S-1-5-19", "S-1-5-20"]);
    let malformed = temp.path().join("malformed.json");
    fs::write(
        &malformed,
        format!(
            "{{\"version\":1,\"ownerSid\":\"{}\",\"approvedAdministratorSids\":[],\"unexpected\":true}}",
            owner.as_str()
        ),
    )
    .expect("write malformed config");
    AclPolicy::new(service.clone(), owner.clone())
        .expect("ACL policy")
        .apply_file(&malformed)
        .expect("protected ACL");
    assert_eq!(
        ServiceIdentityConfig::load(&malformed, &service)
            .expect_err("unknown fields must fail")
            .code(),
        "SERVICE_IDENTITY_CONFIG_INVALID"
    );

    let mismatched = temp.path().join("mismatched.json");
    fs::write(
        &mismatched,
        format!(
            "{{\"version\":1,\"ownerSid\":\"{}\",\"approvedAdministratorSids\":[]}}",
            owner.as_str()
        ),
    )
    .expect("write mismatched config");
    let wrong_owner = sid_other_than(
        &service,
        &["S-1-5-21-100-200-300-600", "S-1-5-21-100-200-300-601"],
    );
    AclPolicy::new(service.clone(), wrong_owner)
        .expect("wrong ACL policy")
        .apply_file(&mismatched)
        .expect("mismatched protected ACL");
    assert_eq!(
        ServiceIdentityConfig::load(&mismatched, &service)
            .expect_err("ACL owner mismatch must fail")
            .code(),
        "SERVICE_IDENTITY_CONFIG_ACL_INVALID"
    );

    let escaped = temp.path().join("escaped.json");
    fs::write(
        &escaped,
        format!(
            concat!(
                "{{\"version\":1,\"ownerSid\":\"{}\",",
                "\"approvedAdministratorSids\":[],",
                "\"cutoverVerifier\":{{",
                "\"evidenceDirectory\":\"..\\\\outside\",",
                "\"sourceRevision\":\"1111111111111111111111111111111111111111\",",
                "\"sourceTreeDigest\":\"2222222222222222222222222222222222222222222222222222222222222222\",",
                "\"buildId\":\"3333333333333333333333333333333333333333333333333333333333333333\"",
                "}}}}"
            ),
            owner.as_str()
        ),
    )
    .expect("write escaped config");
    AclPolicy::new(service.clone(), owner)
        .expect("ACL policy")
        .apply_file(&escaped)
        .expect("protected ACL");
    assert_eq!(
        ServiceIdentityConfig::load(&escaped, &service)
            .expect_err("cutover evidence path escape must fail")
            .code(),
        "SERVICE_IDENTITY_CONFIG_INVALID"
    );
}

fn sid_other_than(service: &WindowsSid, candidates: &[&str]) -> WindowsSid {
    candidates
        .iter()
        .map(|candidate| WindowsSid::parse(candidate).expect("candidate SID"))
        .find(|candidate| candidate != service)
        .expect("different SID")
}
