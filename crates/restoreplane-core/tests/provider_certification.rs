use restoreplane_core::{
    CapabilityEvidence, CapabilityOutcome, CertificationIdentity, Engine, NativeToolIdentity,
    ProviderCapability, ProviderCertification, RestoreMode, RestoreSafetyPolicy,
    RestoreTargetSnapshot, SupportStatus, ToolTrust,
};

fn identity() -> CertificationIdentity {
    CertificationIdentity::new(
        "0123456789abcdef0123456789abcdef01234567",
        "a".repeat(64),
        "windows",
        "x86_64",
        "fixture-pg18",
        1_722_000_000_000,
    )
    .expect("valid certification identity")
}

fn trusted_tool() -> NativeToolIdentity {
    NativeToolIdentity::new(
        "pg_dump",
        r"C:\restoreplane\tools\pg18\pg_dump.exe",
        "18.0",
        "b".repeat(64),
        ToolTrust::AuthenticodeValid {
            publisher: "PostgreSQL Global Development Group".to_owned(),
        },
    )
    .expect("valid tool identity")
}

fn complete_capabilities() -> Vec<CapabilityEvidence> {
    ProviderCapability::REQUIRED
        .into_iter()
        .map(|capability| {
            CapabilityEvidence::new(
                capability,
                CapabilityOutcome::Passed,
                format!("artifacts/g2/{capability}.json"),
                None,
            )
            .expect("valid capability evidence")
        })
        .collect()
}

#[test]
fn supported_requires_every_capability_and_trusted_tool_in_one_evidence_identity() {
    let certification = ProviderCertification::evaluate(
        identity(),
        Engine::Postgresql,
        "PostgreSQL 18.0",
        "cluster-fixture-pg18",
        vec![trusted_tool()],
        complete_capabilities(),
        Vec::new(),
    )
    .expect("evaluate complete evidence");

    assert_eq!(certification.status(), SupportStatus::Supported);
    assert!(certification.missing_capabilities().is_empty());
    assert!(certification.is_publishable_supported());
}

#[test]
fn missing_l3_or_untrusted_tool_never_becomes_supported() {
    let capabilities = complete_capabilities()
        .into_iter()
        .filter(|evidence| evidence.capability() != ProviderCapability::RestoreL3)
        .collect();
    let missing_l3 = ProviderCertification::evaluate(
        identity(),
        Engine::Postgresql,
        "PostgreSQL 18.0",
        "cluster-fixture-pg18",
        vec![trusted_tool()],
        capabilities,
        Vec::new(),
    )
    .expect("evaluate incomplete evidence");
    assert_eq!(missing_l3.status(), SupportStatus::Candidate);
    assert_eq!(
        missing_l3.missing_capabilities(),
        &[ProviderCapability::RestoreL3]
    );
    assert!(!missing_l3.is_publishable_supported());

    let untrusted = NativeToolIdentity::new(
        "pg_restore",
        r"C:\restoreplane\tools\pg18\pg_restore.exe",
        "18.0",
        "c".repeat(64),
        ToolTrust::Unverified,
    )
    .expect("valid untrusted inventory");
    let untrusted_tool = ProviderCertification::evaluate(
        identity(),
        Engine::Postgresql,
        "PostgreSQL 18.0",
        "cluster-fixture-pg18",
        vec![untrusted],
        complete_capabilities(),
        Vec::new(),
    )
    .expect("evaluate untrusted tool");
    assert_eq!(untrusted_tool.status(), SupportStatus::Candidate);
    assert!(!untrusted_tool.is_publishable_supported());
}

#[test]
fn limited_and_failed_evidence_keep_exact_non_supported_status() {
    let mut limited = complete_capabilities();
    let l2 = limited
        .iter_mut()
        .find(|evidence| evidence.capability() == ProviderCapability::VerifyL2)
        .expect("L2 capability");
    *l2 = CapabilityEvidence::new(
        ProviderCapability::VerifyL2,
        CapabilityOutcome::Limited,
        "artifacts/g2/l2.json",
        Some("schema oracle only".to_owned()),
    )
    .expect("limited evidence");
    let limited = ProviderCertification::evaluate(
        identity(),
        Engine::Mysql,
        "MySQL 8.4.6",
        "mysql-fixture",
        vec![trusted_tool()],
        limited,
        Vec::new(),
    )
    .expect("evaluate limited evidence");
    assert_eq!(limited.status(), SupportStatus::Limited);

    let mut failed = complete_capabilities();
    let validation = failed
        .iter_mut()
        .find(|evidence| evidence.capability() == ProviderCapability::Validation)
        .expect("validation capability");
    *validation = CapabilityEvidence::new(
        ProviderCapability::Validation,
        CapabilityOutcome::Failed,
        "artifacts/g2/oracle.json",
        Some("content hash mismatch".to_owned()),
    )
    .expect("failed evidence");
    let failed = ProviderCertification::evaluate(
        identity(),
        Engine::Mysql,
        "MySQL 8.4.6",
        "mysql-fixture",
        vec![trusted_tool()],
        failed,
        Vec::new(),
    )
    .expect("evaluate failed evidence");
    assert_eq!(failed.status(), SupportStatus::Experimental);
}

#[test]
fn overwrite_preflight_rejects_fingerprint_drift_replay_and_system_database() {
    let policy = RestoreSafetyPolicy::new(true, ["postgres", "template0", "template1"], true, true)
        .expect("valid policy");

    let drift = policy
        .validate(
            RestoreMode::Overwrite,
            &RestoreTargetSnapshot::new(
                "app",
                true,
                Some("target-before"),
                Some("target-now"),
                true,
                false,
            ),
        )
        .expect_err("target drift must fail before process start");
    assert_eq!(drift.code(), "RESTORE_TARGET_FINGERPRINT_CHANGED");

    let replay = policy
        .validate(
            RestoreMode::Overwrite,
            &RestoreTargetSnapshot::new(
                "app",
                true,
                Some("target-same"),
                Some("target-same"),
                true,
                true,
            ),
        )
        .expect_err("consumed token must fail");
    assert_eq!(replay.code(), "RESTORE_CONFIRMATION_INVALID_OR_REPLAYED");

    let system_database = policy
        .validate(
            RestoreMode::CreateNew,
            &RestoreTargetSnapshot::new("postgres", false, None, None, false, false),
        )
        .expect_err("system database target must fail");
    assert_eq!(system_database.code(), "RESTORE_SYSTEM_DATABASE_FORBIDDEN");
}
