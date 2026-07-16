// 13개 최상위 커맨드 + keys status + catalog rebuild 파싱 고정 테스트.
// dbbackup-cli는 lib 타깃이 없는 바이너리 크레이트라 src/cli.rs를 #[path]로 직접 포함한다.
#[path = "../src/cli.rs"]
#[allow(dead_code)]
mod cli;

use std::path::PathBuf;

use clap::Parser;
use cli::{CatalogAction, Cli, Command, DestKindArg, EngineArg, KeysAction};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).expect("CLI must parse")
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
fn parses_verify_with_level() {
    let c = parse(&["dbbackup", "verify", "--backup", "b1", "--level", "2"]);
    match c.command {
        Command::Verify(a) => {
            assert_eq!(a.backup.as_deref(), Some("b1"));
            assert_eq!(a.level, Some(2));
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
