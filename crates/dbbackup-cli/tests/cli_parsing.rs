// 13개 최상위 커맨드 + keys status + catalog rebuild 파싱 고정 테스트.
// dbbackup-cli는 lib 타깃이 없는 바이너리 크레이트라 src/cli.rs를 #[path]로 직접 포함한다.
#[path = "../src/cli.rs"]
#[allow(dead_code)]
mod cli;

use std::path::PathBuf;

use clap::Parser;
use cli::{
    BackupScopeModeArg, CatalogAction, Cli, Command, DestKindArg, EngineArg, KeysAction,
    RecoveryReasonArg, ServiceAction, ServiceScopeAction,
};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("CLI must parse")
}

#[test]
fn parses_hidden_release_qualification_ipc_request_without_legacy_execution() {
    let idempotency_key = "323e4567-e89b-42d3-a456-426614174000";
    let command = parse(&[
        "dbbackup",
        "service",
        "qualification-request",
        "--command",
        "backup.enqueue",
        "--payload-file",
        r"C:\qualification\backup.json",
        "--idempotency-key",
        idempotency_key,
    ]);
    match command.command {
        Command::Service {
            action: ServiceAction::QualificationRequest(args),
        } => {
            assert_eq!(args.command, "backup.enqueue");
            assert_eq!(
                args.payload_file,
                PathBuf::from(r"C:\qualification\backup.json")
            );
            assert_eq!(
                args.idempotency_key.expect("idempotency key").to_string(),
                idempotency_key
            );
        }
        other => panic!("expected release qualification IPC request, got {other:?}"),
    }
}

#[test]
fn parses_init() {
    let c = parse(&["dbbackup", "init", "--yes"]);
    assert!(matches!(c.command, Command::Init(ref a) if a.yes));
}

#[test]
fn parses_doctor() {
    let c = parse(&["dbbackup", "doctor"]);
    assert!(matches!(c.command, Command::Doctor));
}

#[test]
fn parses_shared_ipc_service_diagnostics() {
    let status = parse(&["dbbackup", "service", "status"]);
    assert!(matches!(
        status.command,
        Command::Service {
            action: ServiceAction::Status
        }
    ));

    let schema = parse(&["dbbackup", "service", "schema"]);
    assert!(matches!(
        schema.command,
        Command::Service {
            action: ServiceAction::Schema
        }
    ));
}

#[test]
fn parses_typed_backup_scope_service_commands() {
    let target_id = "123e4567-e89b-12d3-a456-426614174000";
    let discovery_run_id = "223e4567-e89b-12d3-a456-426614174000";
    let idempotency_key = "323e4567-e89b-12d3-a456-426614174000";
    let listed = parse(&[
        "dbbackup",
        "service",
        "scope",
        "list",
        "--database-target-id",
        target_id,
        "--cursor",
        "120:scope-id",
        "--limit",
        "40",
        "--state",
        "active",
        "--search",
        "orders",
    ]);
    match listed.command {
        Command::Service {
            action:
                ServiceAction::Scope {
                    action: ServiceScopeAction::List(args),
                },
        } => {
            assert_eq!(
                args.database_target_id.expect("target").to_string(),
                target_id
            );
            assert_eq!(args.cursor.as_deref(), Some("120:scope-id"));
            assert_eq!(args.limit, 40);
            assert_eq!(args.state.as_deref(), Some("active"));
            assert_eq!(args.search.as_deref(), Some("orders"));
        }
        other => panic!("expected typed Backup Scope list command, got {other:?}"),
    }

    let objects = parse(&[
        "dbbackup",
        "service",
        "scope",
        "objects",
        "--database-target-id",
        target_id,
        "--discovery-run-id",
        discovery_run_id,
        "--cursor",
        "12",
        "--limit",
        "25",
        "--search",
        "orders",
        "--schema-name",
        "public",
        "--eligibility",
        "eligible",
    ]);
    match objects.command {
        Command::Service {
            action:
                ServiceAction::Scope {
                    action: ServiceScopeAction::Objects(args),
                },
        } => {
            assert_eq!(args.database_target_id.to_string(), target_id);
            assert_eq!(args.discovery_run_id.to_string(), discovery_run_id);
            assert_eq!(args.cursor.as_deref(), Some("12"));
            assert_eq!(args.limit, 25);
            assert_eq!(args.search.as_deref(), Some("orders"));
            assert_eq!(args.schema_name.as_deref(), Some("public"));
            assert_eq!(args.eligibility.as_deref(), Some("eligible"));
        }
        other => panic!("expected typed Backup Scope objects command, got {other:?}"),
    }

    let saved = parse(&[
        "dbbackup",
        "service",
        "scope",
        "save",
        "--database-target-id",
        target_id,
        "--idempotency-key",
        idempotency_key,
        "--display-name",
        "Orders only",
        "--mode",
        "explicit-tables",
        "--discovery-run-id",
        discovery_run_id,
        "--object-id",
        "table:orders",
        "--object-id",
        "table:order-lines",
    ]);
    match saved.command {
        Command::Service {
            action:
                ServiceAction::Scope {
                    action: ServiceScopeAction::Save(args),
                },
        } => {
            assert_eq!(args.database_target_id.to_string(), target_id);
            assert_eq!(args.idempotency_key.to_string(), idempotency_key);
            assert!(matches!(args.mode, BackupScopeModeArg::ExplicitTables));
            assert_eq!(args.engine_object_ids.len(), 2);
            assert_eq!(
                args.discovery_run_id.expect("discovery run").to_string(),
                discovery_run_id
            );
        }
        other => panic!("expected typed Backup Scope save command, got {other:?}"),
    }
}

#[test]
fn backup_scope_mutations_require_a_typed_idempotency_key() {
    let target_id = "123e4567-e89b-12d3-a456-426614174000";
    assert!(Cli::try_parse_from([
        "dbbackup",
        "service",
        "scope",
        "discover",
        "--database-target-id",
        target_id,
    ])
    .is_err());
    assert!(Cli::try_parse_from([
        "dbbackup",
        "service",
        "scope",
        "discover",
        "--database-target-id",
        target_id,
        "--idempotency-key",
        "not-a-uuid",
    ])
    .is_err());
}

#[test]
fn backup_scope_pages_reject_unbounded_limits_and_require_discovery_identity() {
    let target_id = "123e4567-e89b-12d3-a456-426614174000";
    assert!(
        Cli::try_parse_from(["dbbackup", "service", "scope", "list", "--limit", "101",]).is_err()
    );
    assert!(Cli::try_parse_from([
        "dbbackup",
        "service",
        "scope",
        "objects",
        "--database-target-id",
        target_id,
    ])
    .is_err());
}

#[test]
fn parses_offline_cutover_recovery_with_explicit_replay_and_confirmation_fields() {
    let rollback_id = "123e4567-e89b-12d3-a456-426614174000";
    let marker = concat!(
        "config-cutover-v1:",
        "1111111111111111111111111111111111111111111111111111111111111111"
    );
    let recovered = parse(&[
        "dbbackup",
        "service",
        "recover-cutover",
        "--service-sid",
        "S-1-5-18",
        "--cutover-marker",
        marker,
        "--rollback-id",
        rollback_id,
        "--rolled-back-at-ms",
        "2001",
        "--reason",
        "post-cutover-health-failed",
        "--confirm-cutover-marker",
        marker,
    ]);
    match recovered.command {
        Command::Service {
            action: ServiceAction::RecoverCutover(args),
        } => {
            assert_eq!(args.rollback_id.to_string(), rollback_id);
            assert_eq!(args.cutover_marker, marker);
            assert_eq!(args.confirm_cutover_marker, marker);
            assert!(matches!(
                args.reason,
                RecoveryReasonArg::PostCutoverHealthFailed
            ));
        }
        other => panic!("expected offline recovery command, got {other:?}"),
    }
}

#[test]
fn offline_cutover_recovery_cannot_override_the_service_singleton_key() {
    let marker = concat!(
        "config-cutover-v1:",
        "1111111111111111111111111111111111111111111111111111111111111111"
    );
    assert!(Cli::try_parse_from([
        "dbbackup",
        "service",
        "recover-cutover",
        "--service-sid",
        "S-1-5-18",
        "--cutover-marker",
        marker,
        "--rollback-id",
        "123e4567-e89b-12d3-a456-426614174000",
        "--rolled-back-at-ms",
        "2001",
        "--reason",
        "operator-requested",
        "--confirm-cutover-marker",
        marker,
        "--instance-key",
        "bypass"
    ])
    .is_err());
}

#[test]
fn parses_test_connection() {
    let c = parse(&["dbbackup", "test-connection", "--db", "app"]);
    match c.command {
        Command::TestConnection(a) => assert_eq!(a.db.as_deref(), Some("app")),
        other => panic!("expected TestConnection, got {other:?}"),
    }
}

#[test]
fn parses_add_db() {
    let c = parse(&[
        "dbbackup",
        "add-db",
        "--alias",
        "app",
        "--engine",
        "postgresql",
        "--host",
        "10.0.0.5",
        "--port",
        "5432",
        "--dbname",
        "app",
        "--user",
        "backup_reader",
    ]);
    match c.command {
        Command::AddDb(a) => {
            assert_eq!(a.alias, "app");
            assert!(matches!(a.engine, EngineArg::Postgresql));
            assert_eq!(a.port, 5432);
            assert!(a.dump_globals);
            assert!(a.tunnel.is_none());
        }
        other => panic!("expected AddDb, got {other:?}"),
    }
}

#[test]
fn parses_add_dest() {
    let c = parse(&[
        "dbbackup",
        "add-dest",
        "--name",
        "s3-primary",
        "--kind",
        "s3",
    ]);
    match c.command {
        Command::AddDest(a) => {
            assert_eq!(a.name, "s3-primary");
            assert!(matches!(a.kind, DestKindArg::S3));
        }
        other => panic!("expected AddDest, got {other:?}"),
    }
}

#[test]
fn parses_run_with_db_and_dry_run() {
    let c = parse(&["dbbackup", "run", "--db", "app", "--dry-run"]);
    match c.command {
        Command::Run(a) => {
            assert_eq!(a.db.as_deref(), Some("app"));
            assert!(a.dry_run);
        }
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn accepts_task_scheduler_generated_run_arguments() {
    let c = parse(&["dbbackup", "run", "--job", "prod-pg-nightly", "--scheduled"]);
    match c.command {
        Command::Run(a) => {
            assert_eq!(a.job.as_deref(), Some("prod-pg-nightly"));
            assert!(a.scheduled);
        }
        other => panic!("expected Run, got {other:?}"),
    }
}

#[test]
fn scheduled_run_requires_an_exact_job() {
    assert!(Cli::try_parse_from(["dbbackup", "run", "--scheduled"]).is_err());
}

#[test]
fn run_rejects_ambiguous_db_and_job_filters() {
    assert!(
        Cli::try_parse_from(["dbbackup", "run", "--db", "app", "--job", "prod-pg-nightly",])
            .is_err()
    );
}

#[test]
fn scheduled_run_cannot_be_a_dry_run() {
    assert!(Cli::try_parse_from([
        "dbbackup",
        "run",
        "--job",
        "prod-pg-nightly",
        "--scheduled",
        "--dry-run",
    ])
    .is_err());
}

#[test]
fn parses_verify_with_level() {
    let c = parse(&[
        "dbbackup",
        "verify",
        "--backup",
        "b1",
        "--level",
        "2",
        "--from-destination",
        "s3-primary",
    ]);
    match c.command {
        Command::Verify(a) => {
            assert_eq!(a.backup.as_deref(), Some("b1"));
            assert_eq!(a.level, Some(2));
            assert_eq!(a.from_destination.as_deref(), Some("s3-primary"));
        }
        other => panic!("expected Verify, got {other:?}"),
    }
}

#[test]
fn verify_level_out_of_range_is_rejected() {
    assert!(Cli::try_parse_from(["dbbackup", "verify", "--level", "4"]).is_err());
}

#[test]
fn parses_restore() {
    let c = parse(&[
        "dbbackup",
        "restore",
        "--backup",
        "b1",
        "--from-destination",
        "s3-primary",
        "--decrypt-only",
        "--with-recovery-code",
    ]);
    match c.command {
        Command::Restore(a) => {
            assert_eq!(a.backup, "b1");
            assert_eq!(a.from_destination, "s3-primary");
            assert!(a.to.is_none());
            assert!(a.decrypt_only);
            assert!(a.with_recovery_code);
        }
        other => panic!("expected Restore, got {other:?}"),
    }
}

#[test]
fn parses_rotate_key() {
    let c = parse(&["dbbackup", "rotate-key", "--new-recovery-code"]);
    match c.command {
        Command::RotateKey(a) => {
            assert!(a.new_recovery_code);
            assert!(!a.re_register_dpapi);
        }
        other => panic!("expected RotateKey, got {other:?}"),
    }
}

#[test]
fn parses_register_dpapi() {
    let c = parse(&["dbbackup", "register-dpapi"]);
    assert!(matches!(c.command, Command::RegisterDpapi));
}

#[test]
fn parses_keys_status() {
    let c = parse(&["dbbackup", "keys", "status"]);
    match c.command {
        Command::Keys { action } => assert!(matches!(action, KeysAction::Status)),
        other => panic!("expected Keys, got {other:?}"),
    }
}

#[test]
fn parses_catalog_rebuild() {
    let c = parse(&["dbbackup", "catalog", "rebuild", "--from", "s3-primary"]);
    match c.command {
        Command::Catalog { action } => match action {
            CatalogAction::Rebuild(a) => assert_eq!(a.from_destination, "s3-primary"),
        },
        other => panic!("expected Catalog, got {other:?}"),
    }
}

#[test]
fn parses_prune_with_defaults() {
    let c = parse(&["dbbackup", "prune", "--dry-run"]);
    match c.command {
        Command::Prune(a) => {
            assert_eq!(a.prune_config, PathBuf::from("prune.toml"));
            assert!(a.dry_run);
        }
        other => panic!("expected Prune, got {other:?}"),
    }
}

#[test]
fn parses_schedule() {
    let c = parse(&[
        "dbbackup",
        "schedule",
        "--job",
        "prod-pg-nightly",
        "--remove",
    ]);
    match c.command {
        Command::Schedule(a) => {
            assert_eq!(a.job, "prod-pg-nightly");
            assert!(a.remove);
        }
        other => panic!("expected Schedule, got {other:?}"),
    }
}

#[test]
fn global_config_flag_accepts_windows_path_with_spaces() {
    let c = parse(&[
        "dbbackup",
        "--config",
        r"D:\My Backups\dbbackup.toml",
        "doctor",
    ]);
    assert_eq!(c.config, PathBuf::from(r"D:\My Backups\dbbackup.toml"));
    assert_eq!(c.verbose, 0);
}

#[test]
fn verbose_flag_counts() {
    let c = parse(&["dbbackup", "-vv", "doctor"]);
    assert_eq!(c.verbose, 2);
}
