use std::path::PathBuf;
use std::time::Duration;

use restoreplane_catalog::{ConnectionExecutionBinding, OperationalStore, SelectedDatabaseTarget};
use restoreplane_source_mysql::binary::{resolve_bundled_metadata_client, ClientKind};
use restoreplane_source_mysql::meta::{MySqlCatalogProbe, MysqlCliMetaConn};
use restoreplane_source_mysql::version::{ServerKind, ServerVersionInfo};
use restoreplane_source_pg::{probe_catalog, probe_tables, PgConnCfg, PgSourceError};
use restoreplane_source_sqlite::{probe_sqlite_file, SqliteProviderError};
use zeroize::Zeroizing;

use crate::trusted_tool::verify_trusted_bundled_tool;

use super::{
    ConnectionInspection, ConnectionInspector, ConnectionOperationError, DatabaseCredentials,
    DatabaseObjectInspection, DiscoveredDatabase, DiscoveredDatabaseObject,
    SelectedStructureInspection,
};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const MYSQL_SYSTEM_SCHEMAS: [&str; 4] =
    ["information_schema", "mysql", "performance_schema", "sys"];
type MySqlMetadataClientBinding = (PathBuf, Option<[u8; 32]>, Vec<(PathBuf, [u8; 32])>);

pub struct NativeConnectionInspector {
    timeout: Duration,
    tool_root: Option<PathBuf>,
    trusted_tool_store: Option<OperationalStore>,
}

impl Default for NativeConnectionInspector {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            tool_root: None,
            trusted_tool_store: None,
        }
    }
}

impl NativeConnectionInspector {
    pub const fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            tool_root: None,
            trusted_tool_store: None,
        }
    }

    pub fn with_tool_root(tool_root: PathBuf) -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            tool_root: Some(tool_root),
            trusted_tool_store: None,
        }
    }

    pub fn with_trusted_tool_root(tool_root: PathBuf, store: OperationalStore) -> Self {
        Self {
            timeout: DEFAULT_TIMEOUT,
            tool_root: Some(tool_root),
            trusted_tool_store: Some(store),
        }
    }

    fn mysql_metadata_client_binding(
        &self,
        client_kind: ClientKind,
    ) -> Result<MySqlMetadataClientBinding, ConnectionOperationError> {
        let bundle_base = self.tool_root.as_ref().map(|root| root.join("mysql"));
        let resolved = bundle_base
            .as_deref()
            .ok_or(())
            .and_then(|base| resolve_bundled_metadata_client(client_kind, base).map_err(|_| ()))
            .map_err(|()| {
                ConnectionOperationError::terminal(
                    "CONNECTION_TOOL_MISSING",
                    "tool_missing",
                    "Install the matching trusted MySQL or MariaDB client bundle and retry.",
                )
            })?;
        let Some(store) = &self.trusted_tool_store else {
            return Ok((resolved, None, Vec::new()));
        };
        let install_root = self
            .tool_root
            .as_deref()
            .and_then(|path| path.parent())
            .ok_or_else(|| {
                ConnectionOperationError::terminal(
                    "CONNECTION_TOOL_UNTRUSTED",
                    "tool_untrusted",
                    "Repair the signed native tool package before retrying this connection.",
                )
            })?;
        let tool_name = if client_kind == ClientKind::MariaDb {
            "mariadb"
        } else {
            "mysql"
        };
        let verified = verify_trusted_bundled_tool(store, install_root, tool_name, &resolved)
            .map_err(|_| {
                ConnectionOperationError::terminal(
                    "CONNECTION_TOOL_UNTRUSTED",
                    "tool_untrusted",
                    "Repair the signed native tool package before retrying this connection.",
                )
            })?;
        Ok((
            verified.canonical_path().to_path_buf(),
            Some(*verified.sha256()),
            verified.dependencies().to_vec(),
        ))
    }

    #[cfg(test)]
    fn mysql_metadata_client(
        &self,
        client_kind: ClientKind,
    ) -> Result<PathBuf, ConnectionOperationError> {
        self.mysql_metadata_client_binding(client_kind)
            .map(|(path, _, _)| path)
    }

    fn runtime(&self) -> Result<tokio::runtime::Runtime, ConnectionOperationError> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| {
                ConnectionOperationError::terminal(
                    "CONNECTION_RUNTIME_UNAVAILABLE",
                    "unknown",
                    "Restart the service and retry.",
                )
            })
    }

    fn inspect_postgresql(
        &self,
        profile: &ConnectionExecutionBinding,
        credentials: Option<&DatabaseCredentials>,
        discover: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        let credentials = credentials.ok_or_else(missing_credentials)?;
        let host = profile.host().ok_or_else(invalid_endpoint)?;
        let port = profile.port().ok_or_else(invalid_endpoint)?;
        let config = PgConnCfg {
            host: host.to_owned(),
            port,
            user: credentials.username().to_owned(),
            password: Zeroizing::new(credentials.password().to_owned()),
            admin_database: "postgres".to_owned(),
            tls_mode: profile.tls_mode().to_owned(),
        };
        let runtime = self.runtime()?;
        let result = runtime.block_on(async {
            tokio::time::timeout(self.timeout, probe_catalog(&config, profile.tls_mode())).await
        });
        let probe = match result {
            Ok(Ok(probe)) => probe,
            Ok(Err(error)) => return Err(classify_postgres_error(&error)),
            Err(_) => {
                return Err(ConnectionOperationError::retryable(
                    "CONNECTION_TIMEOUT",
                    "network",
                    "Check the endpoint and firewall, then retry.",
                ))
            }
        };
        let databases = if discover {
            probe
                .databases
                .into_iter()
                .map(|name| DiscoveredDatabase::eligible(format!("database:{name}"), name))
                .collect()
        } else {
            Vec::new()
        };
        Ok(ConnectionInspection::new(
            probe.server_version,
            serde_json::json!({
                "probe": "passed",
                "discover": "supported",
                "tlsMode": profile.tls_mode(),
                "engine": "postgresql",
                "serverIdentity": probe.server_identity,
            }),
            databases,
        ))
    }

    fn inspect_mysql_family(
        &self,
        profile: &ConnectionExecutionBinding,
        credentials: Option<&DatabaseCredentials>,
        discover: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        if !matches!(profile.tls_mode(), "disabled" | "preferred" | "required") {
            return Err(ConnectionOperationError::terminal(
                "CONNECTION_TLS_MODE_UNSUPPORTED",
                "tls",
                "Use disabled, preferred, or required TLS; CA verification requires a configured CA adapter.",
            ));
        }
        let credentials = credentials.ok_or_else(missing_credentials)?;
        let host = profile.host().ok_or_else(invalid_endpoint)?;
        let port = profile.port().ok_or_else(invalid_endpoint)?;
        let client_kind = if profile.engine() == "mariadb" {
            ClientKind::MariaDb
        } else {
            ClientKind::OracleMySql
        };
        let (client, expected_sha256, trusted_dependencies) =
            self.mysql_metadata_client_binding(client_kind)?;
        let connection = MysqlCliMetaConn {
            client,
            expected_sha256,
            trusted_dependencies,
            host: host.to_owned(),
            port,
            user: credentials.username().to_owned(),
            password: Zeroizing::new(credentials.password().to_owned()),
            client_kind,
            tls_mode: profile.tls_mode().to_owned(),
        };
        let runtime = self.runtime()?;
        let result = runtime.block_on(async {
            tokio::time::timeout(self.timeout, connection.probe_catalog()).await
        });
        let probe = match result {
            Ok(Ok(probe)) => probe,
            Ok(Err(_)) => {
                return Err(ConnectionOperationError::retryable(
                    "CONNECTION_CLIENT_OR_NETWORK_FAILED",
                    "network",
                    "Confirm the trusted SQL client is installed and the endpoint is reachable, then retry.",
                ))
            }
            Err(_) => {
                return Err(ConnectionOperationError::retryable(
                    "CONNECTION_TIMEOUT",
                    "network",
                    "Check the endpoint and firewall, then retry.",
                ))
            }
        };
        validate_mysql_family(profile.engine(), &probe)?;
        let databases = if discover {
            probe
                .databases
                .into_iter()
                .map(|name| {
                    if MYSQL_SYSTEM_SCHEMAS.contains(&name.as_str()) {
                        DiscoveredDatabase::excluded(
                            format!("schema:{name}"),
                            name,
                            "system schema",
                        )
                    } else {
                        DiscoveredDatabase::eligible(format!("schema:{name}"), name)
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(ConnectionInspection::new(
            probe.server_version,
            serde_json::json!({
                "probe": "passed",
                "discover": "supported",
                "tlsMode": profile.tls_mode(),
                "engine": profile.engine(),
                "metadataClient": if profile.engine() == "mariadb" { "mariadb" } else { "mysql" },
                "serverIdentity": probe.server_identity,
            }),
            databases,
        ))
    }

    fn inspect_sqlite(
        &self,
        profile: &ConnectionExecutionBinding,
        discover: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        if profile.transport() != "file" {
            return Err(ConnectionOperationError::terminal(
                "CONNECTION_ENDPOINT_INVALID",
                "unknown",
                "SQLite connections require a local file endpoint.",
            ));
        }
        let path = profile.file_path().ok_or_else(invalid_endpoint)?;
        let probe = probe_sqlite_file(std::path::Path::new(path))
            .map_err(|error| classify_sqlite_error(&error))?;
        let databases = if discover {
            let name = probe
                .canonical_path()
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("main")
                .to_owned();
            vec![DiscoveredDatabase::eligible(
                format!("file:{}", probe.object_id()),
                name,
            )]
        } else {
            Vec::new()
        };
        Ok(ConnectionInspection::new(
            format!("SQLite {}", probe.sqlite_version()),
            serde_json::json!({
                "probe": "passed",
                "discover": "single_file",
                "engine": "sqlite",
                "transport": "file",
                "onlineBackup": "supported",
                "fileObjectId": probe.object_id(),
                "journalMode": probe.journal_mode(),
                "pageSize": probe.page_size(),
                "pageCount": probe.page_count(),
                "schemaObjectCount": probe.schema_object_count(),
                "sourceReadonly": probe.readonly(),
                "header": "valid",
            }),
            databases,
        ))
    }
}

impl ConnectionInspector for NativeConnectionInspector {
    fn inspect_selected_structure(
        &self,
        profile: &ConnectionExecutionBinding,
        credentials: Option<&DatabaseCredentials>,
        target: &SelectedDatabaseTarget,
        items: &[restoreplane_domain::workbench_protection::WorkbenchSelectionItem],
    ) -> Result<SelectedStructureInspection, ConnectionOperationError> {
        if profile.engine() != "postgresql" {
            return Err(ConnectionOperationError::terminal(
                "WORKBENCH_STRUCTURE_UNSUPPORTED",
                "unsupported_version",
                "현재 서비스의 TLS 경계에서 이 엔진의 선택 구조 검사를 지원하지 않습니다.",
            ));
        }
        let credentials = credentials.ok_or_else(missing_credentials)?;
        let config = PgConnCfg {
            host: profile.host().ok_or_else(invalid_endpoint)?.into(),
            port: profile.port().ok_or_else(invalid_endpoint)?,
            user: credentials.username().into(),
            password: Zeroizing::new(credentials.password().into()),
            admin_database: target.display_name().into(),
            tls_mode: profile.tls_mode().into(),
        };
        let observations = self.runtime()?.block_on(async {
            tokio::time::timeout(
                self.timeout,
                restoreplane_source_pg::probe_selected_structure(
                    &config,
                    target.display_name(),
                    profile.tls_mode(),
                    items,
                ),
            )
            .await
        });
        match observations {
            Ok(Ok(observed)) => Ok(SelectedStructureInspection {
                items: observed.items,
                server_identity: observed.server_identity,
            }),
            Ok(Err(error)) => Err(classify_postgres_error(&error)),
            Err(_) => Err(scope_timeout()),
        }
    }
    fn inspect(
        &self,
        profile: &ConnectionExecutionBinding,
        credentials: Option<&DatabaseCredentials>,
        discover: bool,
    ) -> Result<ConnectionInspection, ConnectionOperationError> {
        match profile.engine() {
            "postgresql" => self.inspect_postgresql(profile, credentials, discover),
            "mysql" | "mariadb" => self.inspect_mysql_family(profile, credentials, discover),
            "sqlite" => self.inspect_sqlite(profile, discover),
            _ => Err(ConnectionOperationError::terminal(
                "CONNECTION_ENGINE_UNSUPPORTED",
                "unsupported_version",
                "Choose an engine whose probe and discovery adapter is certified in Settings.",
            )),
        }
    }

    fn inspect_database_objects(
        &self,
        profile: &ConnectionExecutionBinding,
        credentials: Option<&DatabaseCredentials>,
        target: &SelectedDatabaseTarget,
    ) -> Result<DatabaseObjectInspection, ConnectionOperationError> {
        let credentials = credentials.ok_or_else(missing_credentials)?;
        let host = profile.host().ok_or_else(invalid_endpoint)?;
        let port = profile.port().ok_or_else(invalid_endpoint)?;
        let runtime = self.runtime()?;
        match profile.engine() {
            "postgresql" => {
                let config = PgConnCfg {
                    host: host.to_owned(),
                    port,
                    user: credentials.username().to_owned(),
                    password: Zeroizing::new(credentials.password().to_owned()),
                    admin_database: target.display_name().to_owned(),
                    tls_mode: profile.tls_mode().to_owned(),
                };
                let result = runtime.block_on(async {
                    tokio::time::timeout(
                        self.timeout,
                        probe_tables(&config, target.display_name(), profile.tls_mode()),
                    )
                    .await
                });
                let observations = match result {
                    Ok(Ok(observations)) => observations,
                    Ok(Err(error)) => return Err(classify_postgres_error(&error)),
                    Err(_) => return Err(scope_timeout()),
                };
                Ok(DatabaseObjectInspection::new(
                    observations
                        .into_iter()
                        .map(|object| {
                            if object.eligible {
                                DiscoveredDatabaseObject::eligible_table(
                                    object.engine_object_id,
                                    object.schema_name,
                                    object.table_name,
                                )
                            } else {
                                DiscoveredDatabaseObject::excluded(
                                    object.engine_object_id,
                                    object.schema_name,
                                    object.table_name,
                                    "partitioned_table",
                                    object
                                        .exclusion_reason
                                        .unwrap_or_else(|| "engine policy".to_owned()),
                                )
                            }
                        })
                        .collect(),
                ))
            }
            "mysql" | "mariadb" => {
                if !matches!(profile.tls_mode(), "disabled" | "preferred" | "required") {
                    return Err(ConnectionOperationError::terminal(
                        "CONNECTION_TLS_MODE_UNSUPPORTED",
                        "tls",
                        "Use disabled, preferred, or required TLS for table discovery.",
                    ));
                }
                let client_kind = if profile.engine() == "mariadb" {
                    ClientKind::MariaDb
                } else {
                    ClientKind::OracleMySql
                };
                let (client, expected_sha256, trusted_dependencies) =
                    self.mysql_metadata_client_binding(client_kind)?;
                let connection = MysqlCliMetaConn {
                    client,
                    expected_sha256,
                    trusted_dependencies,
                    host: host.to_owned(),
                    port,
                    user: credentials.username().to_owned(),
                    password: Zeroizing::new(credentials.password().to_owned()),
                    client_kind,
                    tls_mode: profile.tls_mode().to_owned(),
                };
                let result = runtime.block_on(async {
                    tokio::time::timeout(
                        self.timeout,
                        connection.list_backup_tables(target.display_name()),
                    )
                    .await
                });
                let observations =
                    match result {
                        Ok(Ok(observations)) => observations,
                        Ok(Err(_)) => return Err(ConnectionOperationError::retryable(
                            "BACKUP_SCOPE_DISCOVERY_FAILED",
                            "network",
                            "Confirm catalog permissions and the trusted SQL client, then retry.",
                        )),
                        Err(_) => return Err(scope_timeout()),
                    };
                Ok(DatabaseObjectInspection::new(
                    observations
                        .into_iter()
                        .map(|(schema, table, _engine)| {
                            DiscoveredDatabaseObject::eligible_table(
                                mysql_table_object_id(&schema, &table),
                                schema,
                                table,
                            )
                        })
                        .collect(),
                ))
            }
            _ => Err(ConnectionOperationError::terminal(
                "BACKUP_SCOPE_DISCOVERY_UNSUPPORTED",
                "unsupported_version",
                "Table-level backup scope is supported for PostgreSQL, MySQL, and MariaDB.",
            )),
        }
    }
}

fn mysql_table_object_id(schema: &str, table: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    for value in [schema.as_bytes(), table.as_bytes()] {
        hasher.update(&(value.len() as u64).to_le_bytes());
        hasher.update(value);
    }
    format!("table:{}", hasher.finalize().to_hex())
}

fn scope_timeout() -> ConnectionOperationError {
    ConnectionOperationError::retryable(
        "BACKUP_SCOPE_DISCOVERY_TIMEOUT",
        "network",
        "Check the endpoint and catalog permissions, then retry table discovery.",
    )
}

fn classify_sqlite_error(error: &SqliteProviderError) -> ConnectionOperationError {
    match error {
        SqliteProviderError::ReparsePoint(_) => ConnectionOperationError::terminal(
            error.code(),
            "authorization",
            "Select a regular SQLite file whose path contains no symlink or reparse point.",
        ),
        SqliteProviderError::HeaderInvalid(_) | SqliteProviderError::PathInvalid(_) => {
            ConnectionOperationError::terminal(
                error.code(),
                "unsupported_version",
                "Select a valid SQLite 3 database file.",
            )
        }
        SqliteProviderError::BusyTimeout => ConnectionOperationError::retryable(
            error.code(),
            "network",
            "Release the exclusive SQLite lock or retry after the writer transaction completes.",
        ),
        _ => ConnectionOperationError::terminal(
            error.code(),
            "authorization",
            "Confirm that the service identity can read the SQLite file and its parent path.",
        ),
    }
}

fn validate_mysql_family(
    expected_engine: &str,
    probe: &MySqlCatalogProbe,
) -> Result<(), ConnectionOperationError> {
    let version = ServerVersionInfo::from_raw(&probe.server_version).map_err(|_| {
        ConnectionOperationError::terminal(
            "CONNECTION_SERVER_IDENTITY_INVALID",
            "unsupported_version",
            "Confirm the selected engine matches the server and retry.",
        )
    })?;
    let matches = matches!(
        (expected_engine, version.kind),
        ("mysql", ServerKind::OracleMySql) | ("mariadb", ServerKind::MariaDb)
    );
    if matches {
        Ok(())
    } else {
        Err(ConnectionOperationError::terminal(
            "CONNECTION_ENGINE_MISMATCH",
            "unsupported_version",
            "Change the Connection engine to match the detected server family.",
        ))
    }
}

fn classify_postgres_error(error: &PgSourceError) -> ConnectionOperationError {
    match error {
        PgSourceError::ConnectionFailed { source, .. }
        | PgSourceError::TlsConnectionFailed { source, .. } => {
            if source
                .as_db_error()
                .is_some_and(|db_error| db_error.code().code() == "28P01")
            {
                ConnectionOperationError::terminal(
                    "CONNECTION_AUTH_FAILED",
                    "authentication",
                    "Re-enter the database credential and retry.",
                )
            } else {
                ConnectionOperationError::retryable(
                    "CONNECTION_NETWORK_FAILED",
                    "network",
                    "Check DNS, firewall, server availability, and retry.",
                )
            }
        }
        PgSourceError::TlsConfigurationFailed(_) | PgSourceError::TlsModeUnsupported(_) => {
            ConnectionOperationError::terminal(
                "CONNECTION_TLS_FAILED",
                "tls",
                "Review the TLS mode and trusted certificate chain, then retry.",
            )
        }
        PgSourceError::VersionQueryFailed(_)
        | PgSourceError::CatalogQueryFailed(_)
        | PgSourceError::SelectedStructureUnavailable => ConnectionOperationError::terminal(
            "CONNECTION_AUTHORIZATION_FAILED",
            "authorization",
            "Grant catalog discovery permissions to the database identity and retry.",
        ),
        PgSourceError::VersionParseFailed(_)
        | PgSourceError::NoBundledClientForServer { .. }
        | PgSourceError::ClientTooOld { .. } => ConnectionOperationError::terminal(
            "CONNECTION_VERSION_UNSUPPORTED",
            "unsupported_version",
            "Install a certified tool bundle for the detected server version.",
        ),
        PgSourceError::DumpToolSpawnFailed { .. } => ConnectionOperationError::terminal(
            "CONNECTION_TOOL_MISSING",
            "tool_missing",
            "Install and trust the required PostgreSQL tool bundle.",
        ),
        PgSourceError::DumpToolUntrusted { .. } => ConnectionOperationError::terminal(
            "CONNECTION_TOOL_UNTRUSTED",
            "tool_untrusted",
            "Repair the signed PostgreSQL tool bundle before retrying.",
        ),
    }
}

fn missing_credentials() -> ConnectionOperationError {
    ConnectionOperationError::terminal(
        "CONNECTION_AUTH_REQUIRED",
        "authentication",
        "Store a database username and password for this connection.",
    )
}

fn invalid_endpoint() -> ConnectionOperationError {
    ConnectionOperationError::terminal(
        "CONNECTION_ENDPOINT_INVALID",
        "network",
        "Edit the host and port, then retry.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use restoreplane_catalog::{
        CertificationCapability, EngineCertificationSpec, NativeToolSpec,
        VerifiedPackageCertificationSpec,
    };
    use restoreplane_core::ProviderCapability;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    #[test]
    fn metadata_discovery_prefers_the_bundled_vendor_client() {
        let temp = tempfile::TempDir::new().expect("tool root");
        let mysql_binary = temp
            .path()
            .join("mysql")
            .join("mysql8")
            .join(if cfg!(windows) { "mysql.exe" } else { "mysql" });
        let maria_binary = temp
            .path()
            .join("mysql")
            .join("mariadb11")
            .join(if cfg!(windows) {
                "mariadb.exe"
            } else {
                "mariadb"
            });
        std::fs::create_dir_all(mysql_binary.parent().expect("mysql parent"))
            .expect("mysql directory");
        std::fs::create_dir_all(maria_binary.parent().expect("maria parent"))
            .expect("maria directory");
        std::fs::write(&mysql_binary, b"fixture").expect("mysql fixture");
        std::fs::write(&maria_binary, b"fixture").expect("maria fixture");
        let inspector = NativeConnectionInspector::with_tool_root(temp.path().to_path_buf());

        assert_eq!(
            inspector
                .mysql_metadata_client(ClientKind::OracleMySql)
                .expect("bundled MySQL client"),
            std::fs::canonicalize(mysql_binary).expect("canonical MySQL client")
        );
        assert_eq!(
            inspector
                .mysql_metadata_client(ClientKind::MariaDb)
                .expect("bundled MariaDB client"),
            std::fs::canonicalize(maria_binary).expect("canonical MariaDB client")
        );
    }

    #[test]
    fn production_metadata_client_rejects_post_start_bundle_tampering() {
        let temp = tempfile::TempDir::new().expect("tool root");
        let install_root = temp.path().join("install");
        let tool_root = install_root.join("tools");
        let mysql_binary = tool_root
            .join("mysql")
            .join("mysql8")
            .join(if cfg!(windows) { "mysql.exe" } else { "mysql" });
        std::fs::create_dir_all(mysql_binary.parent().expect("mysql parent"))
            .expect("mysql directory");
        std::fs::write(&mysql_binary, b"trusted mysql fixture").expect("mysql fixture");
        let mysql_binary = std::fs::canonicalize(mysql_binary).expect("canonical MySQL client");
        let digest = restoreplane_runtime::hash_executable(&mysql_binary).expect("mysql digest");
        let digest = digest
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let store =
            OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
        seed_verified_mysql_package(&store, &install_root, &mysql_binary, &digest);
        let inspector = NativeConnectionInspector::with_trusted_tool_root(tool_root, store);
        inspector
            .mysql_metadata_client(ClientKind::OracleMySql)
            .expect("unchanged package client");

        std::fs::write(&mysql_binary, b"tampered mysql fixture").expect("tamper mysql client");
        let error = inspector
            .mysql_metadata_client(ClientKind::OracleMySql)
            .expect_err("tampered package client must be blocked");
        assert_eq!(error.code(), "CONNECTION_TOOL_UNTRUSTED");
        assert!(!error.is_retryable());
    }

    fn seed_verified_mysql_package(
        store: &OperationalStore,
        install_root: &std::path::Path,
        mysql_binary: &std::path::Path,
        mysql_sha256: &str,
    ) {
        let audit_path = install_root.join("tools/native-tools-audit.json");
        let audit = json!({
            "schemaVersion": 1,
            "auditId": "mysql-metadata-client-unit-fixture",
            "approvedAt": "2026-08-30T00:00:00Z",
            "bundles": [{
                "id": "mysql-8",
                "root": "mysql/mysql8",
                "major": 8,
                "requiredExecutables": ["mysql/mysql8/mysql.exe"],
                "dependencies": [],
                "licenses": []
            }],
            "files": [{
                "path": "mysql/mysql8/mysql.exe",
                "sha256": mysql_sha256,
                "role": "executable",
                "authenticodeRequired": true,
                "signerThumbprint": "c".repeat(40),
                "publisher": "Oracle America, Inc."
            }]
        });
        std::fs::write(
            &audit_path,
            serde_json::to_vec(&audit).expect("serialize native audit fixture"),
        )
        .expect("write native audit fixture");
        let audit_sha256 = sha256_fixture_file(&audit_path);
        let manifest_path = install_root.join("package-manifest.json");
        let manifest = json!({
            "schemaVersion": 1,
            "channel": "production",
            "version": "0.1.0",
            "minimumReaderVersion": "0.1.0",
            "buildId": "c".repeat(64),
            "revision": "a".repeat(40),
            "sourceTreeDigest": "b".repeat(64),
            "files": [{
                "path": "tools/mysql/mysql8/mysql.exe",
                "sha256": mysql_sha256,
                "sizeBytes": std::fs::metadata(mysql_binary).expect("mysql metadata").len(),
                "authenticodeRequired": true,
                "signerRole": "vendor",
                "signerThumbprint": "c".repeat(40),
                "publisher": "Oracle America, Inc."
            }, {
                "path": "tools/native-tools-audit.json",
                "sha256": audit_sha256,
                "sizeBytes": std::fs::metadata(&audit_path).expect("audit metadata").len(),
                "authenticodeRequired": false,
                "signerRole": "hash_only",
                "signerThumbprint": "",
                "publisher": "RestorePlane unit fixture"
            }],
            "nativeTools": {
                "auditManifestPath": "tools/native-tools-audit.json",
                "auditManifestSha256": audit_sha256,
                "fileCount": 1
            }
        });
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize package manifest fixture"),
        )
        .expect("write package manifest fixture");
        let manifest_sha256 = sha256_fixture_file(&manifest_path);

        let tool = NativeToolSpec::observed(
            "package-mysql-client",
            "mysql",
            mysql_binary.to_string_lossy(),
            "mysql 8.4.6",
            "x86_64",
            mysql_sha256,
            "bundled",
            100,
        );
        let evidence_base = "package://provider-certifications.json#/certifications/0";
        let mut certification = EngineCertificationSpec::candidate(
            "package-cert-mysql-unit",
            "mysql",
            ">=8,<9",
            "mysql",
            ">=8,<9",
            "windows",
            "x86_64",
            "sql",
            "transaction_consistent",
            evidence_base,
            "b".repeat(64),
            100,
        );
        for capability in ProviderCapability::REQUIRED {
            certification = certification.with_capability(CertificationCapability::passed(
                capability.as_str(),
                format!("{evidence_base}/capabilities/{}", capability.as_str()),
            ));
        }
        store
            .certification()
            .import_verified_package(
                VerifiedPackageCertificationSpec::new(manifest_sha256, 100)
                    .with_tool(tool, "Oracle America, Inc.")
                    .with_certification(certification, 110, 10_000),
            )
            .expect("import verified mysql package fixture");
    }

    fn sha256_fixture_file(path: &std::path::Path) -> String {
        format!(
            "{:x}",
            Sha256::digest(std::fs::read(path).expect("read fixture digest input"))
        )
    }
}
