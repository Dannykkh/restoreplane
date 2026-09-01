use restoreplane_catalog::rusqlite::{params, Connection};
use restoreplane_catalog::{
    CertificationCapability, EngineCertificationSpec, NativeToolSpec, OperationalStore,
    RepositoryError, VerifiedPackageCertificationSpec,
};
use restoreplane_core::ProviderCapability;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn observed_tool_never_implies_engine_capability_without_current_certification() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    repository
        .record_tool(NativeToolSpec::observed(
            "tool-pg-dump",
            "pg_dump",
            r"C:\Program Files\PostgreSQL\bin\pg_dump.exe",
            "17.5",
            "x86_64",
            "a".repeat(64),
            "system",
            100,
        ))
        .expect("record observed native tool");

    assert!(
        repository
            .effective_capability("postgresql", "backup", 200)
            .expect("query capability")
            .is_none(),
        "inventory presence is not certification evidence"
    );

    repository
        .insert_candidate(
            EngineCertificationSpec::candidate(
                "cert-pg-17",
                "postgresql",
                ">=17,<18",
                "pg_dump",
                ">=17,<18",
                "windows",
                "x86_64",
                "custom",
                "application-consistent",
                "evidence/g1/postgresql-17.json",
                "b".repeat(64),
                300,
            )
            .with_capability(CertificationCapability::passed(
                "backup",
                "evidence/g1/postgresql-17-backup.json",
            ))
            .with_capability(CertificationCapability::passed(
                "probe",
                "evidence/g1/postgresql-17-probe.json",
            ))
            .with_capability(CertificationCapability::passed(
                "discovery",
                "evidence/g1/postgresql-17-discover.json",
            ))
            .with_capability(CertificationCapability::passed(
                "tool_preflight",
                "evidence/g1/postgresql-17-preflight.json",
            ))
            .with_capability(CertificationCapability::passed(
                "verify_l1",
                "evidence/g1/postgresql-17-l1.json",
            ))
            .with_capability(CertificationCapability::limited(
                "verify_l2",
                "schema and row-count oracle only",
                "evidence/g1/postgresql-17-l2.json",
            ))
            .with_capability(CertificationCapability::passed(
                "restore_create_new",
                "evidence/g1/postgresql-17-restore.json",
            ))
            .with_capability(CertificationCapability::passed(
                "validation",
                "evidence/g1/postgresql-17-validate.json",
            ))
            .with_capability(CertificationCapability::passed(
                "restore_l3",
                "evidence/g1/postgresql-17-restore-drill.json",
            )),
        )
        .expect("insert candidate certification");
    assert!(
        repository
            .effective_capability("postgresql", "backup", 350)
            .expect("candidate query")
            .is_none(),
        "candidate status is not public support"
    );

    repository
        .certify("cert-pg-17", 400, 1_000)
        .expect("publish certification");
    let backup = repository
        .effective_capability("postgresql", "backup", 500)
        .expect("query certified backup")
        .expect("current backup certification");
    assert_eq!(backup.support_status(), "passed");
    let l2 = repository
        .effective_capability("postgresql", "verify_l2", 500)
        .expect("query limited L2")
        .expect("current limited certification");
    assert_eq!(l2.support_status(), "limited");
    assert_eq!(
        l2.limitation_text(),
        Some("schema and row-count oracle only")
    );
    assert_eq!(
        repository
            .effective_capability("postgresql", "restore_create_new", 500)
            .expect("restore capability query")
            .expect("complete matrix restore capability")
            .support_status(),
        "passed"
    );
    assert!(
        repository
            .effective_capability("postgresql", "backup", 1_000)
            .expect("expired query")
            .is_none(),
        "expiry boundary is exclusive"
    );
}

#[test]
fn changed_tool_binary_resets_trust_until_explicit_verification() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    let first = repository
        .record_tool(NativeToolSpec::observed(
            "tool-pg-dump",
            "pg_dump",
            r"C:\tools\pg_dump.exe",
            "17.5",
            "x86_64",
            "c".repeat(64),
            "operator",
            100,
        ))
        .expect("record first observation");
    repository
        .trust_verified_tool(first.id(), "PostgreSQL Global Development Group", 110)
        .expect("trust verified binary");
    assert_eq!(
        repository.tool_trust(first.id()).expect("trusted state"),
        ("trusted".to_owned(), "valid".to_owned())
    );

    let changed = repository
        .record_tool(NativeToolSpec::observed(
            "ignored-new-id",
            "pg_dump",
            r"C:\tools\pg_dump.exe",
            "17.5",
            "x86_64",
            "d".repeat(64),
            "operator",
            200,
        ))
        .expect("record changed binary at same path");
    assert_eq!(changed.id(), first.id());
    assert!(changed.binary_changed());
    assert_eq!(
        repository
            .tool_trust(first.id())
            .expect("reset trust state"),
        ("unreviewed".to_owned(), "not_checked".to_owned())
    );
}

#[test]
fn trusted_tool_lookup_does_not_depend_on_a_bounded_settings_inventory() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    repository
        .record_tool(NativeToolSpec::observed(
            "tool-pg-restore",
            "pg_restore",
            r"C:\tools\pg_restore.exe",
            "17.5",
            "x86_64",
            "9".repeat(64),
            "bundled",
            100,
        ))
        .expect("record tool");
    repository
        .trust_verified_tool(
            "tool-pg-restore",
            "PostgreSQL Global Development Group",
            110,
        )
        .expect("trust tool");

    assert!(repository
        .has_trusted_tool("pg_restore")
        .expect("trusted lookup"));
    assert!(!repository
        .has_trusted_tool("mysqldump")
        .expect("missing lookup"));
}

#[test]
fn trusted_tool_identity_is_bound_to_the_exact_active_path_and_digest() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    let trusted_path = r"C:\RestorePlane\tools\mysql\mysql.exe";
    repository
        .record_tool(NativeToolSpec::observed(
            "tool-mysql-client",
            "mysql",
            trusted_path,
            "mysql 8.4.6",
            "x86_64",
            "a".repeat(64),
            "bundled",
            100,
        ))
        .expect("record bundled mysql client");
    repository
        .trust_verified_tool("tool-mysql-client", "Oracle America, Inc.", 110)
        .expect("trust exact mysql client");

    let identity = repository
        .trusted_tool_identity_at_path("mysql", trusted_path)
        .expect("lookup exact trusted tool");
    assert_eq!(identity.tool_name(), "mysql");
    assert_eq!(identity.executable_path(), trusted_path);
    assert_eq!(identity.file_sha256(), "a".repeat(64));
    assert_eq!(identity.version_text(), "mysql 8.4.6");

    assert!(matches!(
        repository
            .trusted_tool_identity_at_path("mysql", r"C:\other\mysql.exe")
            .expect_err("same tool name at another path is not trusted"),
        RepositoryError::NotFound
    ));

    repository
        .record_tool(NativeToolSpec::observed(
            "ignored-new-id",
            "mysql",
            trusted_path,
            "mysql 8.4.6",
            "x86_64",
            "b".repeat(64),
            "bundled",
            200,
        ))
        .expect("observe changed bytes at trusted path");
    assert!(matches!(
        repository
            .trusted_tool_identity_at_path("mysql", trusted_path)
            .expect_err("changed bytes reset trust before execution"),
        RepositoryError::NotFound
    ));
}

#[test]
fn incomplete_provider_matrix_cannot_be_published_as_certified() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    repository
        .insert_candidate(
            EngineCertificationSpec::candidate(
                "cert-incomplete",
                "postgresql",
                "18.0.0",
                "pg_dump",
                "18.0.0",
                "windows",
                "x86_64",
                "custom",
                "transaction-consistent",
                "artifacts/g2/postgresql-18.json",
                "e".repeat(64),
                100,
            )
            .with_capability(CertificationCapability::passed(
                "backup",
                "artifacts/g2/postgresql-18-backup.json",
            )),
        )
        .expect("insert incomplete candidate");

    let error = repository
        .certify("cert-incomplete", 200, 1_000)
        .expect_err("missing required provider rows must block publication");
    assert_eq!(error.code(), "CERTIFICATION_INPUT_INVALID");
    assert!(repository
        .effective_capability("postgresql", "backup", 300)
        .expect("query candidate")
        .is_none());
}

#[test]
fn engine_matrix_is_complete_data_driven_and_never_promotes_candidate_evidence() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    let mut sqlite = EngineCertificationSpec::candidate(
        "cert-sqlite-031",
        "sqlite",
        "3.45.x bundled",
        "sqlite-online-backup",
        "rusqlite 0.31.0",
        "windows",
        "x86_64",
        "sqlite-database",
        "online-snapshot",
        "artifacts/g4/sqlite.json",
        "f".repeat(64),
        100,
    );
    for capability in ProviderCapability::REQUIRED {
        sqlite = sqlite.with_capability(CertificationCapability::passed(
            capability.as_str(),
            format!("artifacts/g4/sqlite-{}.json", capability.as_str()),
        ));
    }
    repository
        .insert_candidate(sqlite)
        .expect("insert SQLite evidence");
    repository
        .certify("cert-sqlite-031", 200, 2_000)
        .expect("certify complete SQLite matrix");

    repository
        .insert_candidate(
            EngineCertificationSpec::candidate(
                "cert-sqlserver-candidate",
                "sqlserver",
                "version/edition unverified",
                "sqlpackage",
                "170.4.83",
                "windows",
                "x86_64",
                "bacpac",
                "requires-quiesce",
                "artifacts/g4/sqlserver-candidate.json",
                "a".repeat(64),
                300,
            )
            .with_capability(CertificationCapability::passed(
                "tool_preflight",
                "artifacts/g4/sqlserver-tool-contract.json",
            ))
            .with_capability(CertificationCapability::limited(
                "backup",
                "BACPAC fixture round trip has not run; logins, roles, Agent jobs, and encrypted objects are excluded.",
                "artifacts/g4/sqlserver-candidate.json",
            )),
        )
        .expect("insert SQL Server candidate");

    let matrix = fixture
        .store
        .console_query()
        .engine_matrix(500)
        .expect("read engine matrix");
    assert_eq!(matrix.certification_total_count, 2);
    assert!(matrix.complete);
    assert_eq!(matrix.rows.len(), 9);
    let sqlite = matrix
        .rows
        .iter()
        .find(|row| row.engine == "sqlite")
        .expect("SQLite row");
    assert_eq!(sqlite.support_status, "supported");
    assert_eq!(sqlite.capabilities.len(), 9);
    assert!(sqlite
        .capabilities
        .iter()
        .all(|capability| capability.support_status == "passed"));

    let sqlserver = matrix
        .rows
        .iter()
        .find(|row| row.engine == "sqlserver")
        .expect("SQL Server row");
    assert_eq!(sqlserver.support_status, "candidate");
    assert_eq!(sqlserver.capabilities.len(), 9);
    assert_eq!(
        sqlserver
            .capabilities
            .iter()
            .find(|capability| capability.capability == "backup")
            .and_then(|capability| capability.limitation.as_deref()),
        Some(
            "BACPAC fixture round trip has not run; logins, roles, Agent jobs, and encrypted objects are excluded."
        )
    );
    assert_eq!(
        matrix
            .rows
            .iter()
            .find(|row| row.engine == "mongodb")
            .expect("unimplemented engine row")
            .support_status,
        "unavailable"
    );
}

#[test]
fn engine_matrix_reports_an_exact_count_and_incomplete_marker_at_its_database_cap() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    for index in 0..11 {
        repository
            .insert_candidate(
                EngineCertificationSpec::candidate(
                    format!("cert-cap-{index:02}"),
                    "postgresql",
                    "fixture",
                    format!("tool-{index:02}"),
                    "fixture",
                    "windows",
                    "x86_64",
                    "native",
                    "candidate",
                    format!("evidence/cap-{index:02}.json"),
                    format!("{index:064x}"),
                    100 + index,
                )
                .with_capability(CertificationCapability::passed(
                    "tool_preflight",
                    format!("evidence/cap-{index:02}-preflight.json"),
                )),
            )
            .expect("insert capped certification");
    }

    let matrix = fixture
        .store
        .console_query()
        .engine_matrix(500)
        .expect("read capped engine matrix");
    assert_eq!(matrix.certification_total_count, 11);
    assert!(!matrix.complete);
    assert_eq!(
        matrix
            .rows
            .iter()
            .filter(|row| row.certification_id.is_some())
            .count(),
        10
    );
    assert!(serde_json::to_vec(&matrix).expect("serialize matrix").len() < 768 * 1024);
}

#[test]
fn settings_reports_exact_counts_and_incomplete_when_a_database_cap_is_reached() {
    let fixture = Fixture::new();
    let connection = Connection::open(&fixture.database_path).expect("open fixture writer");
    for index in 0..26 {
        connection
            .execute(
                "INSERT INTO notification_channels(
                    id, display_name, kind, config_json, state,
                    last_probe_status, last_probe_at_ms, deleted_at_ms,
                    created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, 'webhook', '{}', 'ready', NULL, NULL, NULL, ?3, ?3)",
                params![
                    format!("notification-cap-{index:02}"),
                    format!("Notification cap {index:02}"),
                    1_000 + index,
                ],
            )
            .expect("insert notification channel");
    }

    let settings = fixture
        .store
        .console_query()
        .settings()
        .expect("read capped settings");
    assert_eq!(settings.inventory_counts.notification_channels, 26);
    assert_eq!(settings.notification_channels.len(), 25);
    assert!(!settings.complete);
    assert!(
        serde_json::to_vec(&settings)
            .expect("serialize settings")
            .len()
            < 768 * 1024
    );
}

#[test]
fn verified_package_import_publishes_trusted_tools_and_complete_capabilities_atomically() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    let mut certification = EngineCertificationSpec::candidate(
        "package-cert-postgresql-18",
        "postgresql",
        ">=18,<19",
        "pg_dump",
        ">=18,<19",
        "windows",
        "x86_64",
        "custom",
        "transaction-consistent",
        "package://provider-certifications.json#/certifications/0",
        "b".repeat(64),
        1_000,
    );
    for capability in ProviderCapability::REQUIRED {
        certification = certification.with_capability(CertificationCapability::passed(
            capability.as_str(),
            format!(
                "package://provider-certifications.json#/certifications/0/capabilities/{}",
                capability.as_str()
            ),
        ));
    }
    let spec = VerifiedPackageCertificationSpec::new("a".repeat(64), 1_000)
        .with_tool(
            NativeToolSpec::observed(
                "package-tool-pg-dump-18",
                "pg_dump",
                r"C:\Program Files\RestorePlane\tools\pg\bin\pg_dump.exe",
                "pg_dump (PostgreSQL) 18.4",
                "x86_64",
                "c".repeat(64),
                "bundled",
                1_000,
            ),
            "PostgreSQL Global Development Group",
        )
        .with_certification(certification, 1_000, 86_401_000);

    let outcome = repository
        .import_verified_package(spec)
        .expect("atomically import verified package evidence");
    assert!(!outcome.deduplicated());
    assert_eq!(outcome.tool_count(), 1);
    assert_eq!(outcome.certification_count(), 1);
    assert_eq!(
        repository
            .tool_trust("package-tool-pg-dump-18")
            .expect("package tool trust"),
        ("trusted".to_owned(), "valid".to_owned())
    );
    assert_eq!(
        repository
            .effective_capability("postgresql", "backup", 2_000)
            .expect("package capability")
            .expect("published package capability")
            .support_status(),
        "passed"
    );
}

#[test]
fn invalid_verified_package_import_leaves_no_partial_tool_or_certification_rows() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    let incomplete = EngineCertificationSpec::candidate(
        "package-cert-incomplete",
        "postgresql",
        ">=18,<19",
        "pg_dump",
        ">=18,<19",
        "windows",
        "x86_64",
        "custom",
        "transaction-consistent",
        "package://provider-certifications.json#/certifications/0",
        "d".repeat(64),
        1_000,
    )
    .with_capability(CertificationCapability::passed(
        "backup",
        "package://provider-certifications.json#/certifications/0/capabilities/backup",
    ));
    let spec = VerifiedPackageCertificationSpec::new("e".repeat(64), 1_000)
        .with_tool(
            NativeToolSpec::observed(
                "package-tool-before-invalid-cert",
                "pg_dump",
                r"C:\Program Files\RestorePlane\tools\pg\bin\pg_dump.exe",
                "pg_dump (PostgreSQL) 18.4",
                "x86_64",
                "f".repeat(64),
                "bundled",
                1_000,
            ),
            "PostgreSQL Global Development Group",
        )
        .with_certification(incomplete, 1_000, 86_401_000);

    let error = repository
        .import_verified_package(spec)
        .expect_err("incomplete package capability matrix must fail closed");
    assert_eq!(error.code(), "CERTIFICATION_INPUT_INVALID");
    let connection = Connection::open(&fixture.database_path).expect("open fixture writer");
    let tool_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM native_tool_inventory", [], |row| {
            row.get(0)
        })
        .expect("count tools");
    let certification_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM engine_certifications", [], |row| {
            row.get(0)
        })
        .expect("count certifications");
    assert_eq!(tool_count, 0);
    assert_eq!(certification_count, 0);
}

#[test]
fn restarting_with_the_same_verified_package_replays_as_a_deduplicated_import() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();

    let first = repository
        .import_verified_package(complete_postgresql_package_spec())
        .expect("first verified package import");
    let replay = repository
        .import_verified_package(complete_postgresql_package_spec())
        .expect("same package restart import");

    assert!(!first.deduplicated());
    assert!(replay.deduplicated());
    let connection = Connection::open(&fixture.database_path).expect("open fixture writer");
    let tool_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM native_tool_inventory", [], |row| {
            row.get(0)
        })
        .expect("count tools");
    let certification_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM engine_certifications", [], |row| {
            row.get(0)
        })
        .expect("count certifications");
    assert_eq!(tool_count, 1);
    assert_eq!(certification_count, 1);
}

#[test]
fn same_manifest_digest_with_missing_catalog_rows_is_repaired_not_deduplicated() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    repository
        .import_verified_package(complete_postgresql_package_spec())
        .expect("first verified package import");
    let connection = Connection::open(&fixture.database_path).expect("open fixture writer");
    connection
        .execute(
            "UPDATE native_tool_inventory
             SET deleted_at_ms = 2000, trust_status = 'blocked', updated_at_ms = 2000
             WHERE id = 'package-tool-pg-dump-restart'",
            [],
        )
        .expect("simulate missing active package tool projection");
    drop(connection);

    let repaired = repository
        .import_verified_package(complete_postgresql_package_spec())
        .expect("repair package projection from revalidated evidence");

    assert!(!repaired.deduplicated());
    assert_eq!(
        repository
            .tool_trust("package-tool-pg-dump-restart")
            .expect("repaired tool trust"),
        ("trusted".to_owned(), "valid".to_owned())
    );
    assert!(repository
        .has_trusted_tool("pg_dump")
        .expect("active trusted package tool"));
}

#[test]
fn importing_a_new_verified_package_retires_tools_and_certifications_absent_from_it() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    repository
        .import_verified_package(complete_postgresql_package_spec())
        .expect("first package import");

    let replacement = repository
        .import_verified_package(complete_mysql_package_spec())
        .expect("replacement package import");

    assert!(!replacement.deduplicated());
    assert!(!repository
        .has_trusted_tool("pg_dump")
        .expect("retired PostgreSQL tool"));
    assert!(repository
        .effective_capability("postgresql", "backup", 3_000)
        .expect("retired PostgreSQL certification")
        .is_none());
    assert!(repository
        .has_trusted_tool("mysqldump")
        .expect("replacement MySQL tool"));
    assert_eq!(
        repository
            .effective_capability("mysql", "backup", 3_000)
            .expect("replacement MySQL capability")
            .expect("published replacement capability")
            .support_status(),
        "passed"
    );
}

#[test]
fn verified_package_import_cannot_overwrite_an_operator_owned_tool_identity() {
    let fixture = Fixture::new();
    let repository = fixture.store.certification();
    repository
        .record_tool(NativeToolSpec::observed(
            "package-tool-pg-dump-restart",
            "operator-pg-dump",
            r"D:\operator-tools\pg_dump.exe",
            "operator build",
            "x86_64",
            "7".repeat(64),
            "operator",
            500,
        ))
        .expect("seed operator-owned tool identity");

    let error = repository
        .import_verified_package(complete_postgresql_package_spec())
        .expect_err("package import must not take over an operator identity");

    assert_eq!(error.code(), "CERTIFICATION_INPUT_INVALID");
    let connection = Connection::open(&fixture.database_path).expect("open fixture writer");
    let source_kind: String = connection
        .query_row(
            "SELECT source_kind FROM native_tool_inventory
             WHERE id = 'package-tool-pg-dump-restart'",
            [],
            |row| row.get(0),
        )
        .expect("operator tool remains");
    let certification_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM engine_certifications", [], |row| {
            row.get(0)
        })
        .expect("count certifications");
    assert_eq!(source_kind, "operator");
    assert_eq!(certification_count, 0);
}

fn complete_postgresql_package_spec() -> VerifiedPackageCertificationSpec {
    let mut certification = EngineCertificationSpec::candidate(
        "package-cert-postgresql-restart",
        "postgresql",
        ">=18,<19",
        "pg_dump",
        ">=18,<19",
        "windows",
        "x86_64",
        "custom",
        "transaction-consistent",
        "package://provider-certifications.json#/certifications/0",
        "1".repeat(64),
        1_000,
    );
    for capability in ProviderCapability::REQUIRED {
        certification = certification.with_capability(CertificationCapability::passed(
            capability.as_str(),
            format!(
                "package://provider-certifications.json#/certifications/0/capabilities/{}",
                capability.as_str()
            ),
        ));
    }
    VerifiedPackageCertificationSpec::new("2".repeat(64), 1_000)
        .with_tool(
            NativeToolSpec::observed(
                "package-tool-pg-dump-restart",
                "pg_dump",
                r"C:\Program Files\RestorePlane\tools\pg\bin\pg_dump.exe",
                "pg_dump (PostgreSQL) 18.4",
                "x86_64",
                "3".repeat(64),
                "bundled",
                1_000,
            ),
            "PostgreSQL Global Development Group",
        )
        .with_certification(certification, 1_000, 86_401_000)
}

fn complete_mysql_package_spec() -> VerifiedPackageCertificationSpec {
    let mut certification = EngineCertificationSpec::candidate(
        "package-cert-mysql-replacement",
        "mysql",
        ">=8,<9",
        "mysqldump",
        ">=8,<9",
        "windows",
        "x86_64",
        "plain",
        "transaction-consistent",
        "package://provider-certifications.json#/certifications/0",
        "4".repeat(64),
        2_000,
    );
    for capability in ProviderCapability::REQUIRED {
        certification = certification.with_capability(CertificationCapability::passed(
            capability.as_str(),
            format!(
                "package://provider-certifications.json#/certifications/0/capabilities/{}",
                capability.as_str()
            ),
        ));
    }
    VerifiedPackageCertificationSpec::new("5".repeat(64), 2_000)
        .with_tool(
            NativeToolSpec::observed(
                "package-tool-mysqldump-replacement",
                "mysqldump",
                r"C:\Program Files\RestorePlane\tools\mysql\bin\mysqldump.exe",
                "mysqldump  Ver 8.4.11",
                "x86_64",
                "6".repeat(64),
                "bundled",
                2_000,
            ),
            "Oracle Corporation",
        )
        .with_certification(certification, 2_000, 86_402_000)
}

struct Fixture {
    _temp_dir: TempDir,
    database_path: PathBuf,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temp directory");
        let database_path = temp_dir.path().join("operational.db");
        let store = OperationalStore::open(database_path.clone()).expect("open operational store");
        Self {
            _temp_dir: temp_dir,
            database_path,
            store,
        }
    }
}
