use std::path::{Path, PathBuf};

use restoreplane_catalog::OperationalStore;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedTrustedTool {
    canonical_path: PathBuf,
    sha256: [u8; 32],
    dependencies: Vec<(PathBuf, [u8; 32])>,
}

impl VerifiedTrustedTool {
    pub(crate) fn canonical_path(&self) -> &Path {
        &self.canonical_path
    }

    pub(crate) const fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }

    pub(crate) fn dependencies(&self) -> &[(PathBuf, [u8; 32])] {
        &self.dependencies
    }

    pub(crate) fn sha256_hex(&self) -> String {
        encode_sha256(&self.sha256)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrustedToolVerificationError;

pub(crate) fn verify_trusted_bundled_tool(
    store: &OperationalStore,
    install_root: &Path,
    tool_name: &str,
    executable_path: &Path,
) -> Result<VerifiedTrustedTool, TrustedToolVerificationError> {
    let mut verified = verify_trusted_tool_identity(store, tool_name, executable_path)?;
    #[cfg(windows)]
    {
        verified.dependencies = crate::package_certification::verified_launch_dependencies(
            store,
            install_root,
            verified.canonical_path(),
            verified.sha256(),
        )
        .map_err(|_| TrustedToolVerificationError)?
        .into_iter()
        .map(|dependency| (dependency.path().to_path_buf(), *dependency.sha256()))
        .collect();
    }
    #[cfg(not(windows))]
    let _ = install_root;
    Ok(verified)
}

fn verify_trusted_tool_identity(
    store: &OperationalStore,
    tool_name: &str,
    executable_path: &Path,
) -> Result<VerifiedTrustedTool, TrustedToolVerificationError> {
    let canonical_path =
        std::fs::canonicalize(executable_path).map_err(|_| TrustedToolVerificationError)?;
    if !canonical_path.is_absolute() || !canonical_path.is_file() {
        return Err(TrustedToolVerificationError);
    }
    let canonical_text = canonical_path
        .to_str()
        .ok_or(TrustedToolVerificationError)?;
    let trusted = store
        .certification()
        .trusted_tool_identity_at_path(tool_name, canonical_text)
        .map_err(|_| TrustedToolVerificationError)?;
    let actual_sha256 = restoreplane_runtime::hash_executable(&canonical_path)
        .map_err(|_| TrustedToolVerificationError)?;
    if encode_sha256(&actual_sha256) != trusted.file_sha256() {
        return Err(TrustedToolVerificationError);
    }
    Ok(VerifiedTrustedTool {
        canonical_path,
        sha256: actual_sha256,
        dependencies: Vec::new(),
    })
}

fn encode_sha256(digest: &[u8; 32]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(64);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use restoreplane_catalog::NativeToolSpec;

    use super::*;

    #[test]
    fn current_bytes_and_exact_path_must_match_the_trusted_package_identity() {
        let temp = tempfile::TempDir::new().expect("fixture root");
        let store =
            OperationalStore::open(temp.path().join("operational.db")).expect("operational store");
        let trusted_path = temp.path().join("mysql.exe");
        std::fs::write(&trusted_path, b"trusted-client-bytes").expect("trusted client fixture");
        let trusted_path = std::fs::canonicalize(trusted_path).expect("canonical client path");
        let trusted_sha256 = encode_sha256(
            &restoreplane_runtime::hash_executable(&trusted_path).expect("hash trusted client"),
        );
        store
            .certification()
            .record_tool(NativeToolSpec::observed(
                "package-mysql-client",
                "mysql",
                trusted_path.to_string_lossy(),
                "mysql 8.4.6",
                "x86_64",
                trusted_sha256.clone(),
                "bundled",
                100,
            ))
            .expect("record package client");
        store
            .certification()
            .trust_verified_tool("package-mysql-client", "Oracle America, Inc.", 110)
            .expect("trust package client");

        let verified = verify_trusted_tool_identity(&store, "mysql", &trusted_path)
            .expect("exact trusted client");
        assert_eq!(verified.canonical_path(), trusted_path);
        assert_eq!(verified.sha256_hex(), trusted_sha256);

        let copied_path = temp.path().join("copied-mysql.exe");
        std::fs::copy(&trusted_path, &copied_path).expect("copy same trusted bytes");
        assert!(verify_trusted_tool_identity(&store, "mysql", &copied_path).is_err());

        std::fs::write(&trusted_path, b"post-start-tamper").expect("mutate client fixture");
        assert!(verify_trusted_tool_identity(&store, "mysql", &trusted_path).is_err());
    }
}
