use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use dbbackup_catalog::{OperationalStore, RepositoryError};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

const MAX_ARCHIVE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub struct TelemetryArchiveService {
    store: OperationalStore,
    output_root: PathBuf,
}

impl TelemetryArchiveService {
    pub fn new(
        store: OperationalStore,
        output_root: PathBuf,
    ) -> Result<Self, TelemetryArchiveError> {
        if output_root.as_os_str().is_empty() {
            return Err(TelemetryArchiveError::InputInvalid);
        }
        Ok(Self { store, output_root })
    }

    pub fn archive_before(
        &self,
        cutoff_ms: i64,
        max_rows: usize,
        committed_at_ms: i64,
    ) -> Result<Option<TelemetryArchiveReport>, TelemetryArchiveError> {
        if cutoff_ms < 0 || committed_at_ms < cutoff_ms {
            return Err(TelemetryArchiveError::InputInvalid);
        }
        let repository = self.store.telemetry_maintenance();
        let batch = repository.select_archive_batch(cutoff_ms, max_rows)?;
        if batch.is_empty() {
            return Ok(None);
        }
        let document = TelemetryArchiveDocument {
            schema_version: 1,
            archive_epoch_ms: batch.cutoff_ms(),
            format: "redacted_metadata_v1",
            exclusions: [
                "activity message",
                "activity attributes",
                "notification payload",
                "delivery response summary",
                "secret values",
                "raw stdout",
                "raw stderr",
                "process environment",
            ],
            activity: batch.activity(),
            outbox: batch.outbox(),
        };
        let json =
            serde_json::to_vec_pretty(&document).map_err(|_| TelemetryArchiveError::Encoding)?;
        if json.len() > MAX_ARCHIVE_BYTES {
            return Err(TelemetryArchiveError::ArchiveTooLarge);
        }
        let archive_id = blake3::hash(&json).to_hex().to_string();
        let archive = encode_archive(&json)?;
        if archive.len() > MAX_ARCHIVE_BYTES {
            return Err(TelemetryArchiveError::ArchiveTooLarge);
        }
        let checksum_blake3 = blake3::hash(&archive).to_hex().to_string();
        let file_name = format!("telemetry-archive-{cutoff_ms}-{archive_id}.tar.zst");
        let final_path = self.output_root.join(&file_name);
        persist_archive(&self.output_root, &final_path, &archive)?;
        verify_archive(&final_path, &json)?;

        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "archiveId": archive_id,
            "archiveEpochMs": cutoff_ms,
            "path": file_name,
            "checksumBlake3": checksum_blake3,
            "activityCount": batch.activity().len(),
            "outboxCount": batch.outbox().len(),
            "format": "tar+zstd/redacted_metadata_v1"
        });
        let manifest_json =
            serde_json::to_string(&manifest).map_err(|_| TelemetryArchiveError::Encoding)?;
        let commit =
            repository.commit_archive(&archive_id, &manifest_json, &batch, committed_at_ms)?;
        Ok(Some(TelemetryArchiveReport {
            path: final_path,
            checksum_blake3,
            archived_activity_count: commit.activity_count(),
            archived_outbox_count: commit.outbox_count(),
        }))
    }

    pub fn committed_archive_count(&self) -> Result<usize, TelemetryArchiveError> {
        self.store
            .telemetry_maintenance()
            .archive_record_count()
            .map_err(TelemetryArchiveError::from)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelemetryArchiveReport {
    path: PathBuf,
    checksum_blake3: String,
    archived_activity_count: usize,
    archived_outbox_count: usize,
}

impl TelemetryArchiveReport {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn checksum_blake3(&self) -> &str {
        &self.checksum_blake3
    }

    pub const fn archived_activity_count(&self) -> usize {
        self.archived_activity_count
    }

    pub const fn archived_outbox_count(&self) -> usize {
        self.archived_outbox_count
    }
}

#[derive(Debug, Error)]
pub enum TelemetryArchiveError {
    #[error("TELEMETRY_ARCHIVE_INPUT_INVALID")]
    InputInvalid,
    #[error("TELEMETRY_ARCHIVE_ENCODING_FAILED")]
    Encoding,
    #[error("TELEMETRY_ARCHIVE_IO_FAILED")]
    Io,
    #[error("TELEMETRY_ARCHIVE_INVALID")]
    ArchiveInvalid,
    #[error("TELEMETRY_ARCHIVE_TOO_LARGE")]
    ArchiveTooLarge,
    #[error("{0}")]
    Repository(#[from] RepositoryError),
}

impl TelemetryArchiveError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "TELEMETRY_ARCHIVE_INPUT_INVALID",
            Self::Encoding => "TELEMETRY_ARCHIVE_ENCODING_FAILED",
            Self::Io => "TELEMETRY_ARCHIVE_IO_FAILED",
            Self::ArchiveInvalid => "TELEMETRY_ARCHIVE_INVALID",
            Self::ArchiveTooLarge => "TELEMETRY_ARCHIVE_TOO_LARGE",
            Self::Repository(error) => error.code(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TelemetryArchiveDocument<'a> {
    schema_version: u32,
    archive_epoch_ms: i64,
    format: &'static str,
    exclusions: [&'static str; 8],
    activity: &'a [dbbackup_catalog::repositories::ArchivedActivityEvent],
    outbox: &'a [dbbackup_catalog::repositories::ArchivedNotificationOutbox],
}

fn encode_archive(json: &[u8]) -> Result<Vec<u8>, TelemetryArchiveError> {
    let mut encoded = Vec::new();
    {
        let encoder =
            zstd::Encoder::new(&mut encoded, 3).map_err(|_| TelemetryArchiveError::Encoding)?;
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header
            .set_path("telemetry.json")
            .map_err(|_| TelemetryArchiveError::Encoding)?;
        header.set_size(json.len() as u64);
        header.set_mode(0o600);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append(&header, json)
            .map_err(|_| TelemetryArchiveError::Encoding)?;
        let encoder = builder
            .into_inner()
            .map_err(|_| TelemetryArchiveError::Encoding)?;
        encoder
            .finish()
            .map_err(|_| TelemetryArchiveError::Encoding)?;
    }
    Ok(encoded)
}

fn persist_archive(
    output_root: &Path,
    final_path: &Path,
    archive: &[u8],
) -> Result<(), TelemetryArchiveError> {
    ensure_safe_output_root(output_root)?;
    std::fs::create_dir_all(output_root).map_err(|_| TelemetryArchiveError::Io)?;
    ensure_safe_output_root(output_root)?;
    if final_path.exists() {
        let existing = std::fs::read(final_path).map_err(|_| TelemetryArchiveError::Io)?;
        return if existing == archive {
            Ok(())
        } else {
            Err(TelemetryArchiveError::ArchiveInvalid)
        };
    }
    let temporary = output_root.join(format!(".{}.partial", Uuid::new_v4()));
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| TelemetryArchiveError::Io)?;
        file.write_all(archive)
            .map_err(|_| TelemetryArchiveError::Io)?;
        file.sync_all().map_err(|_| TelemetryArchiveError::Io)?;
        drop(file);
        std::fs::rename(&temporary, final_path).map_err(|_| TelemetryArchiveError::Io)
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

fn verify_archive(path: &Path, expected_json: &[u8]) -> Result<(), TelemetryArchiveError> {
    let file = File::open(path).map_err(|_| TelemetryArchiveError::Io)?;
    let decoder = zstd::Decoder::new(file).map_err(|_| TelemetryArchiveError::ArchiveInvalid)?;
    let mut archive = tar::Archive::new(decoder);
    let mut entries = archive
        .entries()
        .map_err(|_| TelemetryArchiveError::ArchiveInvalid)?;
    let mut entry = entries
        .next()
        .ok_or(TelemetryArchiveError::ArchiveInvalid)?
        .map_err(|_| TelemetryArchiveError::ArchiveInvalid)?;
    let entry_path = entry
        .path()
        .map_err(|_| TelemetryArchiveError::ArchiveInvalid)?;
    if entry_path.as_ref() != Path::new("telemetry.json")
        || usize::try_from(entry.size()).map_err(|_| TelemetryArchiveError::ArchiveTooLarge)?
            > MAX_ARCHIVE_BYTES
    {
        return Err(TelemetryArchiveError::ArchiveInvalid);
    }
    let mut json = Vec::new();
    entry
        .read_to_end(&mut json)
        .map_err(|_| TelemetryArchiveError::ArchiveInvalid)?;
    drop(entry);
    if entries.next().is_some() || json != expected_json {
        return Err(TelemetryArchiveError::ArchiveInvalid);
    }
    Ok(())
}

fn ensure_safe_output_root(path: &Path) -> Result<(), TelemetryArchiveError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(TelemetryArchiveError::InputInvalid);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
                if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return Err(TelemetryArchiveError::InputInvalid);
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(TelemetryArchiveError::InputInvalid)?;
            if parent.as_os_str().is_empty() {
                return Err(TelemetryArchiveError::InputInvalid);
            }
            Ok(())
        }
        Err(_) => Err(TelemetryArchiveError::Io),
    }
}
