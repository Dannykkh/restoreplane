#[allow(dead_code)]
#[path = "../src/package_certification.rs"]
mod package_certification;

use std::fs;
use std::path::{Path, PathBuf};

use dbbackup_catalog::OperationalStore;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use package_certification::{
    bootstrap_verified_package_with_probe, verified_launch_dependencies,
    PackageCertificationBootstrapError, ProcessToolVersionProbe, ToolVersionProbe,
};

const NOW_MS: i64 = 2_000_000_000;
const FIXTURE_SERVICE_SID: &str = "S-1-5-80-111-222-333-444-555";
const CAPABILITIES: [&str; 9] = [
    "probe",
    "discovery",
    "tool_preflight",
    "backup",
    "verify_l1",
    "verify_l2",
    "restore_create_new",
    "validation",
    "restore_l3",
];

#[test]
fn fresh_standalone_store_imports_exact_package_tools_and_engine_matrix() {
    let fixture = Fixture::new();
    assert!(!fixture
        .store
        .certification()
        .has_trusted_tool("pg_dump")
        .expect("empty tool inventory"));
    assert_eq!(
        fixture
            .store
            .console_query()
            .engine_matrix(NOW_MS)
            .expect("empty engine matrix")
            .certification_total_count,
        0
    );

    let outcome = bootstrap_verified_package_with_probe(
        &fixture.store,
        &fixture.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &fixture.owner_sid,
        &FixtureVersionProbe,
    )
    .expect("bootstrap an exact signed-package projection");

    assert!(!outcome.deduplicated());
    assert_eq!(outcome.tool_count(), 8);
    assert_eq!(outcome.certification_count(), 3);
    assert_eq!(
        fixture
            .store
            .certification()
            .verified_package_manifest_sha256()
            .expect("durable package manifest identity"),
        sha256_file(&fixture.install_root.join("package-manifest.json"))
    );
    for tool in [
        "pg_dump",
        "pg_dumpall",
        "pg_restore",
        "psql",
        "mysqldump",
        "mysql",
        "mariadb-dump",
        "mariadb",
    ] {
        assert!(fixture
            .store
            .certification()
            .has_trusted_tool(tool)
            .expect("trusted package tool"));
    }
    let matrix = fixture
        .store
        .console_query()
        .engine_matrix(NOW_MS)
        .expect("package engine matrix");
    assert_eq!(matrix.certification_total_count, 3);
    for engine in ["postgresql", "mysql", "mariadb"] {
        let row = matrix
            .rows
            .iter()
            .find(|row| row.engine == engine)
            .expect("engine row");
        assert_eq!(row.support_status, "supported");
        assert_eq!(row.capabilities.len(), 9);
    }
}

#[test]
fn launch_dependency_closure_is_bound_to_imported_manifest_and_rejects_extras() {
    let fixture = Fixture::new();
    bootstrap_verified_package_with_probe(
        &fixture.store,
        &fixture.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &fixture.owner_sid,
        &FixtureVersionProbe,
    )
    .expect("bootstrap package trust");
    let executable = fixture.install_root.join("tools/pg/pg18/pg_dump.exe");
    let executable_sha256 =
        dbbackup_runtime::hash_executable(&executable).expect("fixture executable digest");
    let dependencies = verified_launch_dependencies(
        &fixture.store,
        &fixture.install_root,
        &executable,
        &executable_sha256,
    )
    .expect("exact audited dependency closure");
    assert_eq!(dependencies.len(), 1);
    assert!(dependencies[0].path().ends_with("libpq.dll"));

    fs::write(
        fixture.install_root.join("tools/pg/pg18/injected.dll"),
        b"post-start injection",
    )
    .expect("inject unmanifested dependency");
    let error = verified_launch_dependencies(
        &fixture.store,
        &fixture.install_root,
        &executable,
        &executable_sha256,
    )
    .expect_err("unmanifested bundle member must block launch");
    assert_eq!(error.code(), "PACKAGE_NATIVE_TOOL_CONTRACT_INVALID");
}

#[test]
fn missing_or_tampered_certification_document_leaves_catalog_empty() {
    for mutation in [
        PackageMutation::MissingCertification,
        PackageMutation::TamperedCertification,
    ] {
        let fixture = Fixture::new();
        fixture.mutate(mutation);

        let error = bootstrap_verified_package_with_probe(
            &fixture.store,
            &fixture.install_root,
            NOW_MS,
            FIXTURE_SERVICE_SID,
            &fixture.owner_sid,
            &FixtureVersionProbe,
        )
        .expect_err("unverified package must fail closed");
        assert!(matches!(
            error,
            PackageCertificationBootstrapError::PackageFileMissing
                | PackageCertificationBootstrapError::PackageFileSizeMismatch
                | PackageCertificationBootstrapError::PackageFileDigestMismatch
        ));
        assert_catalog_empty(&fixture.store);
    }
}

#[test]
fn semantic_contract_failure_and_version_probe_failure_are_atomic() {
    let semantic = Fixture::new();
    semantic.rewrite_certification(|document| {
        document["certifications"][0]["scopeModes"] = json!(["whole_database"]);
    });
    let semantic_error = bootstrap_verified_package_with_probe(
        &semantic.store,
        &semantic.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &semantic.owner_sid,
        &FixtureVersionProbe,
    )
    .expect_err("incomplete scope contract must fail closed");
    assert_eq!(
        semantic_error.code(),
        "PACKAGE_CERTIFICATION_CONTRACT_INVALID"
    );
    assert_catalog_empty(&semantic.store);

    let probe = Fixture::new();
    let probe_error = bootstrap_verified_package_with_probe(
        &probe.store,
        &probe.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &probe.owner_sid,
        &FailingVersionProbe,
    )
    .expect_err("unprobed binary must fail closed");
    assert_eq!(probe_error.code(), "PACKAGE_TOOL_VERSION_PROBE_FAILED");
    assert_catalog_empty(&probe.store);
}

#[test]
fn restart_revalidates_the_package_and_imports_idempotently() {
    let fixture = Fixture::new();
    let first = bootstrap_verified_package_with_probe(
        &fixture.store,
        &fixture.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &fixture.owner_sid,
        &FixtureVersionProbe,
    )
    .expect("initial bootstrap");
    let replay = bootstrap_verified_package_with_probe(
        &fixture.store,
        &fixture.install_root,
        NOW_MS + 1,
        FIXTURE_SERVICE_SID,
        &fixture.owner_sid,
        &FixtureVersionProbe,
    )
    .expect("restart bootstrap");

    assert!(!first.deduplicated());
    assert!(replay.deduplicated());
    assert_eq!(
        fixture
            .store
            .console_query()
            .engine_matrix(NOW_MS + 1)
            .expect("engine matrix")
            .certification_total_count,
        3
    );
}

#[test]
fn duplicate_scope_mode_and_wrong_native_major_fail_closed() {
    let duplicate_scope = Fixture::new();
    duplicate_scope.rewrite_certification(|document| {
        document["certifications"][0]["scopeModes"] =
            json!(["whole_database", "explicit_tables", "whole_database"]);
    });
    let duplicate_error = bootstrap_verified_package_with_probe(
        &duplicate_scope.store,
        &duplicate_scope.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &duplicate_scope.owner_sid,
        &FixtureVersionProbe,
    )
    .expect_err("duplicate scope mode must fail closed");
    assert_eq!(
        duplicate_error.code(),
        "PACKAGE_CERTIFICATION_CONTRACT_INVALID"
    );
    assert_catalog_empty(&duplicate_scope.store);

    let wrong_major = Fixture::new();
    let version_error = bootstrap_verified_package_with_probe(
        &wrong_major.store,
        &wrong_major.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &wrong_major.owner_sid,
        &WrongMariaMajorProbe,
    )
    .expect_err("wrong audited tool major must fail closed");
    assert_eq!(version_error.code(), "PACKAGE_TOOL_VERSION_PROBE_FAILED");
    assert_catalog_empty(&wrong_major.store);
}

#[test]
fn unknown_fields_and_oversized_certification_json_are_denied_before_import() {
    let unknown = Fixture::new();
    unknown.rewrite_certification(|document| {
        document["unexpectedField"] = json!(true);
    });
    let unknown_error = bootstrap_verified_package_with_probe(
        &unknown.store,
        &unknown.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &unknown.owner_sid,
        &FixtureVersionProbe,
    )
    .expect_err("unknown fields must be denied");
    assert_eq!(
        unknown_error.code(),
        "PACKAGE_CERTIFICATION_DOCUMENT_INVALID"
    );
    assert_catalog_empty(&unknown.store);

    let oversized = Fixture::new();
    oversized.rewrite_certification(|document| {
        document["unexpectedField"] = json!("x".repeat(300 * 1024));
    });
    let oversized_error = bootstrap_verified_package_with_probe(
        &oversized.store,
        &oversized.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &oversized.owner_sid,
        &FixtureVersionProbe,
    )
    .expect_err("oversized certification JSON must be denied before parsing");
    assert_eq!(oversized_error.code(), "PACKAGE_JSON_LIMIT_EXCEEDED");
    assert_catalog_empty(&oversized.store);
}

#[cfg(windows)]
#[test]
fn protected_install_root_rejects_broad_unknown_or_deny_aces_before_probe() {
    for extra_ace in [
        "(A;OICI;0x00000002;;;WD)",
        "(A;OICI;0x00000002;;;BU)",
        "(A;OICI;0x00000002;;;AU)",
        "(A;OICI;FA;;;S-1-5-21-101-202-303-404)",
        "(D;OICI;0x00000002;;;WD)",
    ] {
        let fixture = Fixture::new();
        let mut dacl = fixture_exact_dacl(&fixture.owner_sid, true);
        dacl.push_str(extra_ace);
        set_fixture_dacl(&fixture.install_root, &dacl, true);

        let error = bootstrap_verified_package_with_probe(
            &fixture.store,
            &fixture.install_root,
            NOW_MS,
            FIXTURE_SERVICE_SID,
            &fixture.owner_sid,
            &NeverCalledVersionProbe,
        )
        .expect_err("broad CreateFiles must be rejected before any executable probe");

        assert_eq!(error.code(), "PACKAGE_INSTALL_ROOT_ACL_INVALID");
        assert_catalog_empty(&fixture.store);
    }
}

#[cfg(windows)]
#[test]
fn protected_package_tree_rejects_nested_broad_create_files_before_probe() {
    let fixture = Fixture::new();
    let mut dacl = fixture_exact_dacl(&fixture.owner_sid, true);
    dacl.push_str("(A;OICI;0x00000002;;;S-1-5-21-101-202-303-404)");
    set_fixture_dacl(&fixture.install_root.join("tools/pg/pg18"), &dacl, true);

    let error = bootstrap_verified_package_with_probe(
        &fixture.store,
        &fixture.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &fixture.owner_sid,
        &NeverCalledVersionProbe,
    )
    .expect_err("nested package ACL drift must be rejected before any executable probe");

    assert_eq!(error.code(), "PACKAGE_INSTALL_ROOT_ACL_INVALID");
    assert_catalog_empty(&fixture.store);
}

#[cfg(windows)]
#[test]
fn actual_tools_tree_rejects_broad_writable_empty_directory_before_probe() {
    let fixture = Fixture::new();
    let injected = fixture
        .install_root
        .join("tools/mysql/mysql8/empty-injected");
    fs::create_dir(&injected).expect("create unmanifested empty directory");
    let mut dacl = fixture_exact_dacl(&fixture.owner_sid, true);
    dacl.push_str("(A;OICI;FA;;;S-1-5-21-101-202-303-404)");
    set_fixture_dacl(&injected, &dacl, true);

    let error = bootstrap_verified_package_with_probe(
        &fixture.store,
        &fixture.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &fixture.owner_sid,
        &NeverCalledVersionProbe,
    )
    .expect_err("empty-directory ACL drift must be rejected before any executable probe");

    assert_eq!(error.code(), "PACKAGE_INSTALL_ROOT_ACL_INVALID");
    assert_catalog_empty(&fixture.store);
}

#[cfg(windows)]
#[test]
fn exact_install_root_acl_rejects_service_full_control_before_probe() {
    let fixture = Fixture::new();
    let dacl = format!(
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{})(A;OICI;FA;;;{FIXTURE_SERVICE_SID})",
        fixture.owner_sid
    );
    set_fixture_dacl(&fixture.install_root, &dacl, true);

    let error = bootstrap_verified_package_with_probe(
        &fixture.store,
        &fixture.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &fixture.owner_sid,
        &NeverCalledVersionProbe,
    )
    .expect_err("service FullControl must be rejected before any executable probe");

    assert_eq!(error.code(), "PACKAGE_INSTALL_ROOT_ACL_INVALID");
    assert_catalog_empty(&fixture.store);
}

#[cfg(windows)]
#[test]
fn unprotected_install_root_dacl_is_rejected_before_probe() {
    let fixture = Fixture::new();
    let dacl = fixture_exact_dacl(&fixture.owner_sid, true).replacen("D:P", "D:", 1);
    set_fixture_dacl(&fixture.install_root, &dacl, false);

    let error = bootstrap_verified_package_with_probe(
        &fixture.store,
        &fixture.install_root,
        NOW_MS,
        FIXTURE_SERVICE_SID,
        &fixture.owner_sid,
        &NeverCalledVersionProbe,
    )
    .expect_err("an inheritable install-root DACL must fail closed");

    assert_eq!(error.code(), "PACKAGE_INSTALL_ROOT_ACL_INVALID");
    assert_catalog_empty(&fixture.store);
}

#[cfg(windows)]
#[test]
fn production_version_probe_executes_an_absolute_binary_without_path_resolution() {
    let git = std::env::split_paths(&std::env::var_os("PATH").expect("test PATH"))
        .map(|directory| directory.join("git.exe"))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| fs::canonicalize(candidate).ok())
        .expect("absolute Git executable installed on the Windows test runner");
    let git_sha256: [u8; 32] = Sha256::digest(fs::read(&git).expect("read Git executable")).into();

    let version = ProcessToolVersionProbe::default()
        .probe(&git, git_sha256, &[])
        .expect("bounded exact-path process probe");
    assert!(version.to_ascii_lowercase().contains("git version"));
    let mut wrong_sha256 = git_sha256;
    wrong_sha256[0] ^= 0xff;
    assert_eq!(
        ProcessToolVersionProbe::default()
            .probe(&git, wrong_sha256, &[])
            .expect_err("digest mismatch must reject the process before resume")
            .code(),
        "PACKAGE_TOOL_VERSION_PROBE_FAILED"
    );
    assert_eq!(
        ProcessToolVersionProbe::default()
            .probe(Path::new("git.exe"), [0_u8; 32], &[])
            .expect_err("PATH-resolved executable must be rejected")
            .code(),
        "PACKAGE_TOOL_VERSION_PROBE_FAILED"
    );
}

fn assert_catalog_empty(store: &OperationalStore) {
    assert_eq!(
        store
            .console_query()
            .settings()
            .expect("settings")
            .inventory_counts
            .tools,
        0
    );
    assert_eq!(
        store
            .console_query()
            .engine_matrix(NOW_MS)
            .expect("engine matrix")
            .certification_total_count,
        0
    );
}

struct FixtureVersionProbe;

impl ToolVersionProbe for FixtureVersionProbe {
    fn probe(
        &self,
        executable: &Path,
        _expected_sha256: [u8; 32],
        _trusted_dependencies: &[(PathBuf, [u8; 32])],
    ) -> Result<String, PackageCertificationBootstrapError> {
        let name = executable
            .file_name()
            .and_then(|value| value.to_str())
            .expect("fixture executable name")
            .to_ascii_lowercase();
        if name.starts_with("pg_") || name == "psql.exe" {
            Ok(format!("{name} (PostgreSQL) 18.4"))
        } else if name == "mysql.exe" || name == "mysqldump.exe" {
            Ok(format!("{name} Ver 8.4 Distrib 8.4"))
        } else if name == "mariadb.exe" || name == "mariadb-dump.exe" {
            Ok(format!("{name} Ver 15.1 Distrib 11.8"))
        } else {
            Err(PackageCertificationBootstrapError::ToolVersionProbeFailed)
        }
    }
}

struct FailingVersionProbe;

impl ToolVersionProbe for FailingVersionProbe {
    fn probe(
        &self,
        _executable: &Path,
        _expected_sha256: [u8; 32],
        _trusted_dependencies: &[(PathBuf, [u8; 32])],
    ) -> Result<String, PackageCertificationBootstrapError> {
        Err(PackageCertificationBootstrapError::ToolVersionProbeFailed)
    }
}

struct NeverCalledVersionProbe;

impl ToolVersionProbe for NeverCalledVersionProbe {
    fn probe(
        &self,
        _executable: &Path,
        _expected_sha256: [u8; 32],
        _trusted_dependencies: &[(PathBuf, [u8; 32])],
    ) -> Result<String, PackageCertificationBootstrapError> {
        panic!("ACL drift must be rejected before any trusted executable probe")
    }
}

struct WrongMariaMajorProbe;

impl ToolVersionProbe for WrongMariaMajorProbe {
    fn probe(
        &self,
        executable: &Path,
        expected_sha256: [u8; 32],
        trusted_dependencies: &[(PathBuf, [u8; 32])],
    ) -> Result<String, PackageCertificationBootstrapError> {
        let output =
            FixtureVersionProbe.probe(executable, expected_sha256, trusted_dependencies)?;
        if executable
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|name| name.starts_with("mariadb"))
        {
            Ok("mariadb Ver 15.1 Distrib 10.11-MariaDB".to_owned())
        } else {
            Ok(output)
        }
    }
}

#[derive(Clone, Copy)]
enum PackageMutation {
    MissingCertification,
    TamperedCertification,
}

struct Fixture {
    _temp_dir: TempDir,
    install_root: PathBuf,
    owner_sid: String,
    store: OperationalStore,
}

impl Fixture {
    fn new() -> Self {
        let temp_dir = TempDir::new().expect("temporary package root");
        let install_root = temp_dir.path().join("install");
        fs::create_dir_all(&install_root).expect("create install root");
        let store = OperationalStore::open(temp_dir.path().join("operational.db"))
            .expect("open standalone store");
        write_fixture_package(&install_root);
        let owner_sid = fixture_owner_sid(&install_root);
        protect_fixture_package_tree(&install_root, &owner_sid);
        Self {
            _temp_dir: temp_dir,
            install_root,
            owner_sid,
            store,
        }
    }

    fn mutate(&self, mutation: PackageMutation) {
        match mutation {
            PackageMutation::MissingCertification => {
                fs::remove_file(self.install_root.join("provider-certifications.json"))
                    .expect("remove provider certifications")
            }
            PackageMutation::TamperedCertification => fs::write(
                self.install_root.join("provider-certifications.json"),
                b"tampered",
            )
            .expect("tamper provider certifications"),
        }
    }

    fn rewrite_certification(&self, mutate: impl FnOnce(&mut Value)) {
        let path = self.install_root.join("provider-certifications.json");
        let mut document: Value =
            serde_json::from_slice(&fs::read(&path).expect("read certification"))
                .expect("parse certification");
        mutate(&mut document);
        write_json(&path, &document);
        rewrite_package_manifest(&self.install_root);
    }
}

fn write_fixture_package(root: &Path) {
    write_file(root, "dbbackup-service.exe", b"fixture service");
    write_file(root, "dbbackup.exe", b"fixture cli");

    let bundles = [
        (
            "postgresql-18",
            "pg/pg18",
            18,
            vec![
                "pg/pg18/pg_dump.exe",
                "pg/pg18/pg_dumpall.exe",
                "pg/pg18/pg_restore.exe",
                "pg/pg18/psql.exe",
            ],
            "pg/pg18/libpq.dll",
            "pg/pg18/LICENSE",
        ),
        (
            "mysql-8",
            "mysql/mysql8",
            8,
            vec!["mysql/mysql8/mysqldump.exe", "mysql/mysql8/mysql.exe"],
            "mysql/mysql8/libmysql.dll",
            "mysql/mysql8/LICENSE",
        ),
        (
            "mariadb-11",
            "mysql/mariadb11",
            11,
            vec![
                "mysql/mariadb11/mariadb-dump.exe",
                "mysql/mariadb11/mariadb.exe",
            ],
            "mysql/mariadb11/libmariadb.dll",
            "mysql/mariadb11/COPYING",
        ),
    ];
    let mut audit_bundles = Vec::new();
    let mut audit_files = Vec::new();
    for (id, bundle_root, major, executables, dependency, license) in bundles {
        for relative in executables.iter().copied().chain([dependency, license]) {
            write_file(root, &format!("tools/{relative}"), relative.as_bytes());
            let is_executable = relative.ends_with(".exe");
            audit_files.push(json!({
                "path": relative,
                "sha256": sha256_file(&root.join(format!("tools/{relative}"))),
                "role": if is_executable { "executable" } else if relative.ends_with(".dll") { "dependency" } else { "license" },
                "authenticodeRequired": is_executable,
                "signerThumbprint": if is_executable { "c".repeat(40) } else { String::new() },
                "publisher": format!("{id} fixture publisher")
            }));
        }
        audit_bundles.push(json!({
            "id": id,
            "root": bundle_root,
            "major": major,
            "requiredExecutables": executables,
            "dependencies": [dependency],
            "licenses": [license]
        }));
    }
    write_json(
        &root.join("tools/native-tools-audit.json"),
        &json!({
            "schemaVersion": 1,
            "auditId": "fixture-audit-2026-08-30",
            "approvedAt": "2026-08-30T00:00:00Z",
            "bundles": audit_bundles,
            "files": audit_files
        }),
    );

    let certifications = [
        ("postgresql", "pg_dump", ">=18,<19", "custom"),
        ("mysql", "mysqldump", ">=8,<9", "sql"),
        ("mariadb", "mariadb-dump", ">=11,<12", "sql"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (engine, tool, version, dump_format))| {
        let base = format!("package://provider-certifications.json#/certifications/{index}");
        let capabilities = CAPABILITIES
            .into_iter()
            .map(|capability| {
                json!({
                    "capability": capability,
                    "supportStatus": "passed",
                    "evidenceLocator": format!("{base}/capabilities/{capability}")
                })
            })
            .collect::<Vec<_>>();
        json!({
            "id": format!("package-cert-{engine}-fixture"),
            "engine": engine,
            "serverVersionRange": version,
            "toolName": tool,
            "toolVersionRange": version,
            "operatingSystem": "windows",
            "architecture": "x86_64",
            "dumpFormat": dump_format,
            "consistencyGrade": "transaction_consistent",
            "evidenceLocator": base,
            "evidenceChecksumBlake3": "b".repeat(64),
            "certifiedAtMs": NOW_MS - 10_000,
            "expiresAtMs": NOW_MS + 86_400_000,
            "scopeModes": ["whole_database", "explicit_tables"],
            "capabilities": capabilities
        })
    })
    .collect::<Vec<_>>();
    write_json(
        &root.join("provider-certifications.json"),
        &json!({
            "schemaVersion": 1,
            "sourceRevision": "a".repeat(40),
            "sourceTreeDigest": "b".repeat(64),
            "buildId": "c".repeat(64),
            "generatedAtMs": NOW_MS - 20_000,
            "certifications": certifications
        }),
    );
    rewrite_package_manifest(root);
}

fn rewrite_package_manifest(root: &Path) {
    let mut relative_files = vec![
        "dbbackup-service.exe".to_owned(),
        "dbbackup.exe".to_owned(),
        "provider-certifications.json".to_owned(),
    ];
    collect_relative_files(root, &root.join("tools"), &mut relative_files);
    relative_files.sort();
    let audit: Value = serde_json::from_slice(
        &fs::read(root.join("tools/native-tools-audit.json")).expect("read audit"),
    )
    .expect("parse audit");
    let files = relative_files
        .iter()
        .map(|relative| {
            let path = root.join(relative);
            let is_executable = relative.ends_with(".exe");
            let audit_entry = relative
                .strip_prefix("tools/")
                .and_then(|tool_path| {
                    audit["files"]
                        .as_array()
                        .expect("audit files")
                        .iter()
                        .find(|entry| entry["path"] == tool_path)
                });
            json!({
                "path": relative,
                "sha256": sha256_file(&path),
                "sizeBytes": fs::metadata(&path).expect("file metadata").len(),
                "authenticodeRequired": is_executable,
                "signerRole": if audit_entry.is_some() && is_executable { "vendor" } else if is_executable { "release" } else { "hash_only" },
                "signerThumbprint": audit_entry.and_then(|entry| entry["signerThumbprint"].as_str()).unwrap_or(""),
                "publisher": audit_entry.and_then(|entry| entry["publisher"].as_str()).unwrap_or("DbBackup fixture publisher")
            })
        })
        .collect::<Vec<_>>();
    let audit_path = root.join("tools/native-tools-audit.json");
    write_json(
        &root.join("package-manifest.json"),
        &json!({
            "schemaVersion": 1,
            "channel": "production",
            "version": "0.1.0",
            "minimumReaderVersion": "0.1.0",
            "buildId": "c".repeat(64),
            "revision": "a".repeat(40),
            "sourceTreeDigest": "b".repeat(64),
            "files": files,
            "nativeTools": {
                "auditManifestPath": "tools/native-tools-audit.json",
                "auditManifestSha256": sha256_file(&audit_path),
                "fileCount": audit["files"].as_array().expect("audit files").len()
            }
        }),
    );
}

fn collect_relative_files(root: &Path, directory: &Path, output: &mut Vec<String>) {
    for entry in fs::read_dir(directory).expect("read fixture directory") {
        let entry = entry.expect("fixture directory entry");
        if entry.file_type().expect("fixture file type").is_dir() {
            collect_relative_files(root, &entry.path(), output);
        } else {
            output.push(
                entry
                    .path()
                    .strip_prefix(root)
                    .expect("relative fixture path")
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
}

fn write_file(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture file parent"))
        .expect("create fixture directory");
    fs::write(path, bytes).expect("write fixture file");
}

fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().expect("JSON parent")).expect("create JSON parent");
    fs::write(
        path,
        serde_json::to_vec(value).expect("serialize fixture JSON"),
    )
    .expect("write fixture JSON");
}

fn sha256_file(path: &Path) -> String {
    let bytes = fs::read(path).expect("read hashed fixture");
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(windows)]
fn fixture_owner_sid(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, ERROR_SUCCESS, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID};

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    assert_eq!(
        unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(path.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(&mut owner),
                None,
                None,
                None,
                &mut descriptor,
            )
        },
        ERROR_SUCCESS,
        "query fixture owner"
    );
    let mut owner_text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(owner, &mut owner_text) }.expect("convert fixture owner SID");
    let result = unsafe { owner_text.to_string() }.expect("fixture owner SID text");
    let _ = unsafe { LocalFree(HLOCAL(owner_text.0.cast())) };
    let _ = unsafe { LocalFree(HLOCAL(descriptor.0)) };
    result
}

#[cfg(not(windows))]
fn fixture_owner_sid(_path: &Path) -> String {
    "S-1-5-21-1-2-3-1000".to_owned()
}

#[cfg(windows)]
fn protect_fixture_package_tree(root: &Path, owner_sid: &str) {
    let mut entries = vec![root.to_path_buf()];
    collect_fixture_entries(root, &mut entries);
    for path in entries {
        let sddl = fixture_exact_dacl(owner_sid, path.is_dir());
        set_fixture_dacl(&path, &sddl, true);
    }
}

#[cfg(not(windows))]
fn protect_fixture_package_tree(_root: &Path, _owner_sid: &str) {}

fn fixture_exact_dacl(owner_sid: &str, directory: bool) -> String {
    let flags = if directory { "OICI" } else { "" };
    format!(
        "D:P(A;{flags};FA;;;SY)(A;{flags};FA;;;{owner_sid})(A;{flags};0x001200a9;;;{FIXTURE_SERVICE_SID})"
    )
}

#[cfg(windows)]
fn collect_fixture_entries(directory: &Path, output: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(directory).expect("read fixture ACL tree") {
        let entry = entry.expect("fixture ACL entry");
        let path = entry.path();
        output.push(path.clone());
        if entry.file_type().expect("fixture ACL file type").is_dir() {
            collect_fixture_entries(&path, output);
        }
    }
}

#[cfg(windows)]
fn set_fixture_dacl(path: &Path, sddl: &str, protected: bool) {
    use std::os::windows::ffi::OsStrExt;

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{LocalFree, BOOL, ERROR_SUCCESS, HLOCAL};
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SetNamedSecurityInfoW,
        SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows::Win32::Security::{
        GetSecurityDescriptorDacl, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    let mut path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let sddl: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .expect("convert fixture DACL");
        let protection = if protected {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
        let mut dacl_present = BOOL::default();
        let mut dacl = std::ptr::null_mut();
        let mut dacl_defaulted = BOOL::default();
        GetSecurityDescriptorDacl(
            descriptor,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
        .expect("read converted fixture DACL");
        assert!(dacl_present.as_bool() && !dacl.is_null());
        let result = SetNamedSecurityInfoW(
            PWSTR(path.as_mut_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | protection,
            PSID::default(),
            PSID::default(),
            Some(dacl),
            None,
        );
        let _ = LocalFree(HLOCAL(descriptor.0));
        assert_eq!(result, ERROR_SUCCESS, "apply fixture DACL");
    }
}
