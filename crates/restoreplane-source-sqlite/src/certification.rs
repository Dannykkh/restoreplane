use restoreplane_core::{
    CapabilityEvidence, CapabilityOutcome, CertificationIdentity, Engine, NativeToolIdentity,
    ProviderCapability, ProviderCertification, ProviderContractError, ProviderLimitation,
    ToolTrust,
};

pub const SQLITE_PROVIDER_LIMITATIONS: [(&str, &str); 2] = [
    (
        "SQLITE_LOCAL_REGULAR_FILE_ONLY",
        "Wave 1 supports local regular files; network shares and virtual filesystems require separate certification.",
    ),
    (
        "SQLITE_ENCRYPTED_DATABASE_UNSUPPORTED",
        "SQLCipher and other encrypted SQLite variants require a separately certified provider.",
    ),
];

pub fn sqlite_certification(
    source_revision: &str,
    build_id: &str,
    recorded_at_ms: i64,
) -> Result<ProviderCertification, ProviderContractError> {
    let identity = CertificationIdentity::new(
        source_revision,
        build_id,
        std::env::consts::OS,
        std::env::consts::ARCH,
        "sqlite-online-backup-conformance",
        recorded_at_ms,
    )?;
    let tool = NativeToolIdentity::new(
        "sqlite-online-backup",
        "embedded://rusqlite/0.41.0",
        format!("rusqlite 0.41.0 / SQLite {}", rusqlite::version()),
        build_id,
        ToolTrust::LockedDependency {
            package: "rusqlite".to_owned(),
            version: "0.41.0".to_owned(),
            registry_checksum: "9c026b4444579e8d754556c6c75d040d69c4a44283cdb648ab8a8d5965c2cfa8"
                .to_owned(),
        },
    )?;
    let evidence_path = |capability: ProviderCapability| {
        let locator = match capability {
            ProviderCapability::Probe | ProviderCapability::Discovery => {
                "crates/restoreplane-service/tests/connection_operation_worker.rs"
            }
            _ => "crates/restoreplane-source-sqlite/tests/conformance.rs",
        };
        CapabilityEvidence::new(capability, CapabilityOutcome::Passed, locator, None)
    };
    let capabilities = ProviderCapability::REQUIRED
        .into_iter()
        .map(evidence_path)
        .collect::<Result<Vec<_>, _>>()?;
    let limitations = SQLITE_PROVIDER_LIMITATIONS
        .into_iter()
        .map(|(code, detail)| ProviderLimitation::new(code, detail))
        .collect::<Result<Vec<_>, _>>()?;
    ProviderCertification::evaluate(
        identity,
        Engine::Sqlite,
        format!("SQLite {}", rusqlite::version()),
        format!("bundled-sqlite-{}", rusqlite::version()),
        vec![tool],
        capabilities,
        limitations,
    )
}
