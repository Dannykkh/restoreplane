use restoreplane_application::{
    canonical_payload_digest, enforce_authority, AccessClass, ApplicationCommand,
    AuthoritySnapshot, ConfirmationAction, CutoverMode, ExecutionAuthority, MutationClass,
};
use serde_json::json;

#[test]
fn command_catalog_has_stable_authorization_and_execution_boundaries() {
    let cases = [
        (
            ApplicationCommand::ServiceHealth,
            "service.health",
            AccessClass::Read,
            MutationClass::ReadOnly,
            None,
            false,
        ),
        (
            ApplicationCommand::ServiceDiagnostics,
            "service.diagnostics",
            AccessClass::Read,
            MutationClass::ReadOnly,
            None,
            false,
        ),
        (
            ApplicationCommand::SupportBundleCreate,
            "support_bundle.create",
            AccessClass::Operate,
            MutationClass::Mutate,
            None,
            false,
        ),
        (
            ApplicationCommand::ServiceUpdateCommit,
            "service.update_commit",
            AccessClass::Admin,
            MutationClass::Mutate,
            None,
            false,
        ),
        (
            ApplicationCommand::RetentionDryRun,
            "retention.dry_run",
            AccessClass::Read,
            MutationClass::ReadOnly,
            None,
            false,
        ),
        (
            ApplicationCommand::RetentionControlSet,
            "retention.control_set",
            AccessClass::Admin,
            MutationClass::Mutate,
            None,
            false,
        ),
        (
            ApplicationCommand::RetentionTombstone,
            "retention.tombstone",
            AccessClass::Admin,
            MutationClass::Mutate,
            None,
            false,
        ),
        (
            ApplicationCommand::ConfirmationPrepare,
            "confirmation.prepare",
            AccessClass::Admin,
            MutationClass::Mutate,
            None,
            false,
        ),
        (
            ApplicationCommand::RetentionDelete,
            "retention.delete",
            AccessClass::Admin,
            MutationClass::Destructive,
            Some(ConfirmationAction::RetentionDelete),
            false,
        ),
        (
            ApplicationCommand::BackupEnqueue,
            "backup.enqueue",
            AccessClass::Operate,
            MutationClass::Enqueue,
            None,
            true,
        ),
        (
            ApplicationCommand::RestorePreflight,
            "restore.preflight",
            AccessClass::Read,
            MutationClass::ReadOnly,
            None,
            false,
        ),
        (
            ApplicationCommand::RestoreEnqueueOverwrite,
            "restore.enqueue_overwrite",
            AccessClass::Admin,
            MutationClass::Destructive,
            Some(ConfirmationAction::RestoreOverwrite),
            true,
        ),
        (
            ApplicationCommand::SecretRotate,
            "secret.rotate",
            AccessClass::Admin,
            MutationClass::Destructive,
            Some(ConfirmationAction::SecretRotate),
            false,
        ),
        (
            ApplicationCommand::ConfigCutover,
            "config.cutover",
            AccessClass::Admin,
            MutationClass::Destructive,
            Some(ConfirmationAction::ConfigCutover),
            true,
        ),
    ];

    for (command, name, access, mutation, confirmation, requires_g1) in cases {
        let descriptor = command.descriptor();
        assert_eq!(descriptor.name(), name);
        assert_eq!(descriptor.access(), access);
        assert_eq!(descriptor.mutation(), mutation);
        assert_eq!(descriptor.confirmation(), confirmation);
        assert_eq!(descriptor.requires_g1(), requires_g1);
        assert_eq!(
            ApplicationCommand::try_from(name).expect("known command"),
            command
        );
    }

    let unknown =
        ApplicationCommand::try_from("native.exec").expect_err("unknown commands are denied");
    assert_eq!(unknown.code(), "APPLICATION_COMMAND_UNKNOWN");
}

#[test]
fn canonical_payload_digest_is_order_independent_and_rejects_floats() {
    let first = json!({
        "target": {"database": "orders", "host": "db.internal"},
        "policy": {"overwrite": true},
    });
    let reordered = json!({
        "policy": {"overwrite": true},
        "target": {"host": "db.internal", "database": "orders"},
    });
    let changed = json!({
        "policy": {"overwrite": false},
        "target": {"host": "db.internal", "database": "orders"},
    });

    assert_eq!(
        canonical_payload_digest(&first).expect("first digest"),
        canonical_payload_digest(&reordered).expect("reordered digest")
    );
    assert_ne!(
        canonical_payload_digest(&first).expect("first digest"),
        canonical_payload_digest(&changed).expect("changed digest")
    );

    let floating =
        canonical_payload_digest(&json!({"ratio": 0.5})).expect_err("floats are not canonical");
    assert_eq!(floating.code(), "APPLICATION_PAYLOAD_INVALID");
}

#[test]
fn pre_g1_authority_is_rehearsal_only_and_never_claims_production_work() {
    let pre_g1 = AuthoritySnapshot::new(
        ExecutionAuthority::LegacyDirect,
        CutoverMode::RehearsalOnly,
        false,
        false,
    );

    enforce_authority(&pre_g1, ApplicationCommand::ServiceHealth)
        .expect("diagnostics remain available");
    enforce_authority(&pre_g1, ApplicationCommand::SecretRotate)
        .expect("non-execution administration remains available");

    for command in [
        ApplicationCommand::BackupEnqueue,
        ApplicationCommand::RestoreEnqueueOverwrite,
        ApplicationCommand::ConfigCutover,
    ] {
        let error = enforce_authority(&pre_g1, command)
            .expect_err("production execution and cutover stay disabled");
        assert_eq!(error.code(), "APPLICATION_REHEARSAL_ONLY");
    }

    let post_g1 = AuthoritySnapshot::new(
        ExecutionAuthority::Service,
        CutoverMode::Production,
        true,
        true,
    );
    enforce_authority(&post_g1, ApplicationCommand::BackupEnqueue)
        .expect("service may enqueue after complete G1 cutover");
}
