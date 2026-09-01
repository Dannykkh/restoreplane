use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

const REDACTION_VERSION: u32 = 1;
const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARCHIVE_ENTRY_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARCHIVE_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const REDACTED: &str = "[REDACTED]";

#[derive(Clone, Debug)]
pub struct SupportBundleInput {
    build_id: String,
    revision: String,
    diagnostics: Value,
    engine_matrix: Value,
    activity: Value,
    config_metadata: Value,
}

impl SupportBundleInput {
    pub fn new(
        build_id: impl Into<String>,
        revision: impl Into<String>,
        diagnostics: Value,
        engine_matrix: Value,
        activity: Value,
        config_metadata: Value,
    ) -> Result<Self, SupportBundleError> {
        let input = Self {
            build_id: build_id.into(),
            revision: revision.into(),
            diagnostics,
            engine_matrix,
            activity,
            config_metadata,
        };
        input.validate()?;
        Ok(input)
    }

    fn validate(&self) -> Result<(), SupportBundleError> {
        if self.build_id.trim().is_empty()
            || self.build_id.len() > 256
            || self.revision.trim().is_empty()
            || self.revision.len() > 256
        {
            return Err(SupportBundleError::InputInvalid);
        }
        let values = [
            &self.diagnostics,
            &self.engine_matrix,
            &self.activity,
            &self.config_metadata,
        ];
        let mut total = self.build_id.len().saturating_add(self.revision.len());
        for value in values {
            total = total.saturating_add(
                serde_json::to_vec(value)
                    .map_err(|_| SupportBundleError::Encoding)?
                    .len(),
            );
            if total > MAX_INPUT_BYTES {
                return Err(SupportBundleError::InputTooLarge);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SupportBundleBuilder {
    output_root: PathBuf,
    forbidden_literals: Vec<String>,
}

impl SupportBundleBuilder {
    pub fn new(output_root: PathBuf) -> Result<Self, SupportBundleError> {
        if output_root.as_os_str().is_empty() {
            return Err(SupportBundleError::InputInvalid);
        }
        Ok(Self {
            output_root,
            forbidden_literals: Vec::new(),
        })
    }

    pub fn with_canary_for_verification(
        mut self,
        canary: impl Into<String>,
    ) -> Result<Self, SupportBundleError> {
        let canary = canary.into();
        if canary.is_empty() || canary.len() > 4_096 {
            return Err(SupportBundleError::InputInvalid);
        }
        self.forbidden_literals.push(canary);
        Ok(self)
    }

    pub fn create(
        &self,
        input: &SupportBundleInput,
        generated_at_ms: i64,
    ) -> Result<SupportBundleReport, SupportBundleError> {
        if generated_at_ms < 0 {
            return Err(SupportBundleError::InputInvalid);
        }
        input.validate()?;
        let raw = serde_json::to_vec(input).map_err(|_| SupportBundleError::Encoding)?;
        self.reject_forbidden(&raw)?;

        ensure_safe_output_root(&self.output_root)?;
        std::fs::create_dir_all(&self.output_root).map_err(|_| SupportBundleError::Io)?;
        ensure_safe_output_root(&self.output_root)?;

        let bundle_id = Uuid::new_v4();
        let final_path = self.output_root.join(format!(
            "support-bundle-{generated_at_ms}-{bundle_id}.tar.zst"
        ));
        let temporary_path = self.output_root.join(format!(".{bundle_id}.partial"));

        let entries = build_entries(input, generated_at_ms)?;
        for bytes in entries.values() {
            self.reject_forbidden(bytes)?;
        }
        let write_result = write_archive(&temporary_path, &entries)
            .and_then(|_| self.verify_archive(&temporary_path))
            .and_then(|_| {
                std::fs::rename(&temporary_path, &final_path).map_err(|_| SupportBundleError::Io)
            });
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&temporary_path);
            return Err(error);
        }

        let checksum_blake3 = checksum_file(&final_path)?;
        Ok(SupportBundleReport {
            path: final_path,
            checksum_blake3,
            entry_count: entries.len(),
            redaction_version: REDACTION_VERSION,
        })
    }

    fn reject_forbidden(&self, bytes: &[u8]) -> Result<(), SupportBundleError> {
        if self
            .forbidden_literals
            .iter()
            .any(|literal| contains_bytes(bytes, literal.as_bytes()))
        {
            return Err(SupportBundleError::CanaryDetected);
        }
        Ok(())
    }

    fn verify_archive(&self, path: &Path) -> Result<(), SupportBundleError> {
        let file = File::open(path).map_err(|_| SupportBundleError::Io)?;
        let decoder = zstd::Decoder::new(file).map_err(|_| SupportBundleError::Archive)?;
        let mut archive = tar::Archive::new(decoder);
        let mut total = 0_usize;
        let mut entry_count = 0_usize;
        for entry in archive.entries().map_err(|_| SupportBundleError::Archive)? {
            let mut entry = entry.map_err(|_| SupportBundleError::Archive)?;
            let path = entry.path().map_err(|_| SupportBundleError::Archive)?;
            if path.components().count() != 1 {
                return Err(SupportBundleError::Archive);
            }
            let size =
                usize::try_from(entry.size()).map_err(|_| SupportBundleError::ArchiveTooLarge)?;
            if size > MAX_ARCHIVE_ENTRY_BYTES {
                return Err(SupportBundleError::ArchiveTooLarge);
            }
            total = total.saturating_add(size);
            if total > MAX_ARCHIVE_TOTAL_BYTES {
                return Err(SupportBundleError::ArchiveTooLarge);
            }
            let mut bytes = Vec::with_capacity(size);
            entry
                .read_to_end(&mut bytes)
                .map_err(|_| SupportBundleError::Archive)?;
            self.reject_forbidden(&bytes)?;
            entry_count = entry_count.saturating_add(1);
        }
        if entry_count != 5 {
            return Err(SupportBundleError::Archive);
        }
        Ok(())
    }
}

impl Serialize for SupportBundleInput {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct RawInput<'a> {
            build_id: &'a str,
            revision: &'a str,
            diagnostics: &'a Value,
            engine_matrix: &'a Value,
            activity: &'a Value,
            config_metadata: &'a Value,
        }
        RawInput {
            build_id: &self.build_id,
            revision: &self.revision,
            diagnostics: &self.diagnostics,
            engine_matrix: &self.engine_matrix,
            activity: &self.activity,
            config_metadata: &self.config_metadata,
        }
        .serialize(serializer)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SupportBundleReport {
    path: PathBuf,
    checksum_blake3: String,
    entry_count: usize,
    redaction_version: u32,
}

impl SupportBundleReport {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn checksum_blake3(&self) -> &str {
        &self.checksum_blake3
    }

    pub const fn entry_count(&self) -> usize {
        self.entry_count
    }

    pub const fn redaction_version(&self) -> u32 {
        self.redaction_version
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum SupportBundleError {
    #[error("SUPPORT_BUNDLE_INPUT_INVALID")]
    InputInvalid,
    #[error("SUPPORT_BUNDLE_INPUT_TOO_LARGE")]
    InputTooLarge,
    #[error("SUPPORT_BUNDLE_CANARY_DETECTED")]
    CanaryDetected,
    #[error("SUPPORT_BUNDLE_ENCODING_FAILED")]
    Encoding,
    #[error("SUPPORT_BUNDLE_IO_FAILED")]
    Io,
    #[error("SUPPORT_BUNDLE_ARCHIVE_INVALID")]
    Archive,
    #[error("SUPPORT_BUNDLE_ARCHIVE_TOO_LARGE")]
    ArchiveTooLarge,
}

impl SupportBundleError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InputInvalid => "SUPPORT_BUNDLE_INPUT_INVALID",
            Self::InputTooLarge => "SUPPORT_BUNDLE_INPUT_TOO_LARGE",
            Self::CanaryDetected => "SUPPORT_BUNDLE_CANARY_DETECTED",
            Self::Encoding => "SUPPORT_BUNDLE_ENCODING_FAILED",
            Self::Io => "SUPPORT_BUNDLE_IO_FAILED",
            Self::Archive => "SUPPORT_BUNDLE_ARCHIVE_INVALID",
            Self::ArchiveTooLarge => "SUPPORT_BUNDLE_ARCHIVE_TOO_LARGE",
        }
    }
}

fn build_entries(
    input: &SupportBundleInput,
    generated_at_ms: i64,
) -> Result<BTreeMap<String, Vec<u8>>, SupportBundleError> {
    let mut entries = BTreeMap::new();
    entries.insert(
        "activity.json".to_owned(),
        encode_redacted(&input.activity)?,
    );
    entries.insert(
        "config-metadata.json".to_owned(),
        encode_redacted(&input.config_metadata)?,
    );
    entries.insert(
        "diagnostics.json".to_owned(),
        encode_redacted(&input.diagnostics)?,
    );
    entries.insert(
        "engine-matrix.json".to_owned(),
        encode_redacted(&input.engine_matrix)?,
    );

    let checksums = entries
        .iter()
        .map(|(name, bytes)| (name.clone(), blake3::hash(bytes).to_hex().to_string()))
        .collect::<BTreeMap<_, _>>();
    let included = entries.keys().cloned().collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "schemaVersion": 1,
        "generatedAtMs": generated_at_ms,
        "buildId": input.build_id,
        "revision": input.revision,
        "redactionVersion": REDACTION_VERSION,
        "included": included,
        "excluded": [
            "secret values",
            "raw stdout",
            "raw stderr",
            "process environment",
            "connection strings",
            "backup content"
        ],
        "checksumsBlake3": checksums,
    });
    entries.insert(
        "manifest.json".to_owned(),
        serde_json::to_vec_pretty(&manifest).map_err(|_| SupportBundleError::Encoding)?,
    );
    Ok(entries)
}

fn encode_redacted(value: &Value) -> Result<Vec<u8>, SupportBundleError> {
    let mut redacted = value.clone();
    redact_value(None, &mut redacted);
    serde_json::to_vec_pretty(&redacted).map_err(|_| SupportBundleError::Encoding)
}

fn redact_value(key: Option<&str>, value: &mut Value) {
    if key.is_some_and(is_sensitive_key) {
        *value = Value::String(REDACTED.to_owned());
        return;
    }
    match value {
        Value::Object(map) => {
            for (child_key, child_value) in map {
                redact_value(Some(child_key), child_value);
            }
        }
        Value::Array(values) => {
            for child in values {
                redact_value(None, child);
            }
        }
        Value::String(text) if looks_secret_bearing(text) => {
            text.clear();
            text.push_str(REDACTED);
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase())
        .collect::<Vec<_>>();
    [
        b"password".as_slice(),
        b"passwd".as_slice(),
        b"secret".as_slice(),
        b"token".as_slice(),
        b"credential".as_slice(),
        b"connectionstring".as_slice(),
        b"privatekey".as_slice(),
        b"rawstdout".as_slice(),
        b"rawstderr".as_slice(),
        b"cookie".as_slice(),
    ]
    .iter()
    .any(|needle| contains_bytes(&normalized, needle))
}

fn looks_secret_bearing(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    [
        "password=",
        "passwd=",
        "pwd=",
        "token=",
        "secret=",
        "authorization: bearer",
        "-----begin private key",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
}

fn write_archive(
    path: &Path,
    entries: &BTreeMap<String, Vec<u8>>,
) -> Result<(), SupportBundleError> {
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|_| SupportBundleError::Io)?;
    let encoder = zstd::Encoder::new(file, 9).map_err(|_| SupportBundleError::Archive)?;
    let mut archive = tar::Builder::new(encoder);
    archive.mode(tar::HeaderMode::Deterministic);
    for (name, bytes) in entries {
        let mut header = tar::Header::new_gnu();
        header
            .set_size(u64::try_from(bytes.len()).map_err(|_| SupportBundleError::ArchiveTooLarge)?);
        header.set_mode(0o600);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive
            .append_data(&mut header, name, bytes.as_slice())
            .map_err(|_| SupportBundleError::Archive)?;
    }
    let encoder = archive
        .into_inner()
        .map_err(|_| SupportBundleError::Archive)?;
    let mut file = encoder.finish().map_err(|_| SupportBundleError::Archive)?;
    file.flush().map_err(|_| SupportBundleError::Io)?;
    file.sync_all().map_err(|_| SupportBundleError::Io)
}

fn checksum_file(path: &Path) -> Result<String, SupportBundleError> {
    let mut file = File::open(path).map_err(|_| SupportBundleError::Io)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|_| SupportBundleError::Io)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn ensure_safe_output_root(path: &Path) -> Result<(), SupportBundleError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(SupportBundleError::InputInvalid);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(SupportBundleError::InputInvalid);
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(SupportBundleError::InputInvalid)?;
            if parent.as_os_str().is_empty() {
                return Err(SupportBundleError::InputInvalid);
            }
            Ok(())
        }
        Err(_) => Err(SupportBundleError::Io),
    }
}
