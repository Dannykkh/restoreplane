use std::io::{BufRead, BufReader, Read};

use ed25519_dalek::VerifyingKey;
use restoreplane_pipeline::chain::{NONCE_PREFIX, STREAM_CHUNK};
use restoreplane_pipeline::{
    Destination, DestinationReaderV2, LocalDest, ManifestPartV2, SignedSetManifestV2,
    MAX_SET_MANIFEST_BYTES_V2,
};
use restoreplane_secrets::Dek;
use thiserror::Error;
use uuid::Uuid;

const TAG_LEN: usize = 16;
const CIPHER_CHUNK: usize = STREAM_CHUNK + TAG_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct L1VerifyLimitsV2 {
    max_part_ciphertext_bytes: u64,
    max_part_plaintext_bytes: u64,
}

impl L1VerifyLimitsV2 {
    pub fn new(
        max_part_ciphertext_bytes: u64,
        max_part_plaintext_bytes: u64,
    ) -> Result<Self, L1V2Error> {
        if max_part_ciphertext_bytes == 0 || max_part_plaintext_bytes == 0 {
            return Err(L1V2Error::InputInvalid);
        }
        Ok(Self {
            max_part_ciphertext_bytes,
            max_part_plaintext_bytes,
        })
    }
}

pub trait PartDekProviderV2: Send + Sync {
    fn resolve(&self, part_id: Uuid, dek_wrap_generation: u32) -> Result<Dek, L1V2Error>;
}

pub struct SingleDekProviderV2 {
    dek: Dek,
    expected_generation: u32,
}

impl SingleDekProviderV2 {
    pub fn new(dek: Dek, expected_generation: u32) -> Self {
        Self {
            dek,
            expected_generation,
        }
    }
}

impl PartDekProviderV2 for SingleDekProviderV2 {
    fn resolve(&self, _part_id: Uuid, dek_wrap_generation: u32) -> Result<Dek, L1V2Error> {
        if dek_wrap_generation != self.expected_generation {
            return Err(L1V2Error::DekUnavailable);
        }
        Ok(self.dek.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L1PartEvidenceV2 {
    part_id: Uuid,
    ciphertext_bytes: u64,
    ciphertext_blake3: String,
    plaintext_bytes: u64,
    plaintext_blake3: String,
}

impl L1PartEvidenceV2 {
    pub const fn part_id(&self) -> Uuid {
        self.part_id
    }

    pub const fn ciphertext_bytes(&self) -> u64 {
        self.ciphertext_bytes
    }

    pub fn ciphertext_blake3(&self) -> &str {
        &self.ciphertext_blake3
    }

    pub const fn plaintext_bytes(&self) -> u64 {
        self.plaintext_bytes
    }

    pub fn plaintext_blake3(&self) -> &str {
        &self.plaintext_blake3
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct L1VerificationReportV2 {
    recovery_point_id: Uuid,
    destination_fingerprint: String,
    manifest_object_key: String,
    verified_at_ms: i64,
    parts: Vec<L1PartEvidenceV2>,
}

impl L1VerificationReportV2 {
    pub const fn recovery_point_id(&self) -> Uuid {
        self.recovery_point_id
    }

    pub fn destination_fingerprint(&self) -> &str {
        &self.destination_fingerprint
    }

    pub fn manifest_object_key(&self) -> &str {
        &self.manifest_object_key
    }

    pub const fn verified_at_ms(&self) -> i64 {
        self.verified_at_ms
    }

    pub fn parts(&self) -> &[L1PartEvidenceV2] {
        &self.parts
    }

    pub fn verified_part_count(&self) -> usize {
        self.parts.len()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum L1V2Error {
    #[error("L1_V2_INPUT_INVALID")]
    InputInvalid,
    #[error("L1_V2_MANIFEST_READ_FAILED")]
    ManifestReadFailed,
    #[error("L1_V2_MANIFEST_INVALID")]
    ManifestInvalid,
    #[error("L1_V2_SIGNATURE_INVALID")]
    SignatureInvalid,
    #[error("L1_V2_DESTINATION_NOT_DECLARED")]
    DestinationNotDeclared,
    #[error("L1_V2_MANIFEST_KEY_MISMATCH")]
    ManifestKeyMismatch,
    #[error("L1_V2_ENCODING_UNSUPPORTED")]
    EncodingUnsupported,
    #[error("L1_V2_CIPHERTEXT_LIMIT_EXCEEDED")]
    CiphertextLimitExceeded,
    #[error("L1_V2_PAYLOAD_READ_FAILED")]
    PayloadReadFailed,
    #[error("L1_V2_CIPHERTEXT_MISMATCH")]
    CiphertextMismatch,
    #[error("L1_V2_DEK_UNAVAILABLE")]
    DekUnavailable,
    #[error("L1_V2_DECRYPT_FAILED")]
    DecryptFailed,
    #[error("L1_V2_PLAINTEXT_LIMIT_EXCEEDED")]
    PlaintextLimitExceeded,
    #[error("L1_V2_PLAINTEXT_MISMATCH")]
    PlaintextMismatch,
}

impl L1V2Error {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "L1_V2_INPUT_INVALID",
            Self::ManifestReadFailed => "L1_V2_MANIFEST_READ_FAILED",
            Self::ManifestInvalid => "L1_V2_MANIFEST_INVALID",
            Self::SignatureInvalid => "L1_V2_SIGNATURE_INVALID",
            Self::DestinationNotDeclared => "L1_V2_DESTINATION_NOT_DECLARED",
            Self::ManifestKeyMismatch => "L1_V2_MANIFEST_KEY_MISMATCH",
            Self::EncodingUnsupported => "L1_V2_ENCODING_UNSUPPORTED",
            Self::CiphertextLimitExceeded => "L1_V2_CIPHERTEXT_LIMIT_EXCEEDED",
            Self::PayloadReadFailed => "L1_V2_PAYLOAD_READ_FAILED",
            Self::CiphertextMismatch => "L1_V2_CIPHERTEXT_MISMATCH",
            Self::DekUnavailable => "L1_V2_DEK_UNAVAILABLE",
            Self::DecryptFailed => "L1_V2_DECRYPT_FAILED",
            Self::PlaintextLimitExceeded => "L1_V2_PLAINTEXT_LIMIT_EXCEEDED",
            Self::PlaintextMismatch => "L1_V2_PLAINTEXT_MISMATCH",
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn verify_l1_from_reader_v2(
    reader: &dyn DestinationReaderV2,
    manifest_object_key: &str,
    destination_fingerprint: &str,
    expected_verifying_key: &VerifyingKey,
    dek_provider: &dyn PartDekProviderV2,
    limits: L1VerifyLimitsV2,
    verified_at_ms: i64,
) -> Result<L1VerificationReportV2, L1V2Error> {
    if !valid_object_key(manifest_object_key)
        || destination_fingerprint.trim().is_empty()
        || destination_fingerprint.len() > 512
        || verified_at_ms < 0
    {
        return Err(L1V2Error::InputInvalid);
    }
    let manifest_bytes = reader
        .read_bytes_bounded(manifest_object_key, MAX_SET_MANIFEST_BYTES_V2)
        .await
        .map_err(|_| L1V2Error::ManifestReadFailed)?;
    let signed: SignedSetManifestV2 =
        serde_json::from_slice(&manifest_bytes).map_err(|_| L1V2Error::ManifestInvalid)?;
    signed
        .manifest
        .validate()
        .map_err(|_| L1V2Error::ManifestInvalid)?;
    let canonical = signed.to_json().map_err(|_| L1V2Error::ManifestInvalid)?;
    if canonical.as_bytes() != manifest_bytes {
        return Err(L1V2Error::ManifestInvalid);
    }
    signed
        .verify(expected_verifying_key)
        .map_err(|_| L1V2Error::SignatureInvalid)?;
    if verified_at_ms < signed.manifest.committed_at_ms {
        return Err(L1V2Error::InputInvalid);
    }
    let replica = signed
        .manifest
        .replicas
        .iter()
        .find(|replica| replica.destination_fingerprint == destination_fingerprint)
        .ok_or(L1V2Error::DestinationNotDeclared)?;
    if replica.manifest_object_key != manifest_object_key {
        return Err(L1V2Error::ManifestKeyMismatch);
    }

    let staging = tempfile::tempdir().map_err(|_| L1V2Error::PayloadReadFailed)?;
    let local = LocalDest::new("l1-v2-staging", staging.path());
    let mut part_evidence = Vec::with_capacity(signed.manifest.parts.len());
    for part in &signed.manifest.parts {
        verify_encoding_contract(part, limits)?;
        let staging_key = format!("cipher/{}.bin", part.part_id);
        let mut writer = local
            .writer(&staging_key)
            .await
            .map_err(|_| L1V2Error::PayloadReadFailed)?;
        let copied = match reader
            .copy_object_to(&part.object_key, &mut writer, part.ciphertext_bytes)
            .await
        {
            Ok(copied) => copied,
            Err(_) => {
                writer.abort().await;
                return Err(L1V2Error::PayloadReadFailed);
            }
        };
        if copied.bytes != part.ciphertext_bytes || copied.blake3 != part.ciphertext_blake3 {
            writer.abort().await;
            return Err(L1V2Error::CiphertextMismatch);
        }
        writer
            .commit()
            .await
            .map_err(|_| L1V2Error::PayloadReadFailed)?;
        let staged_path = staging.path().join(&staging_key);
        let (ciphertext_bytes, ciphertext_blake3) = hash_file(&staged_path)?;
        if ciphertext_bytes != part.ciphertext_bytes || ciphertext_blake3 != part.ciphertext_blake3
        {
            return Err(L1V2Error::CiphertextMismatch);
        }
        let dek = dek_provider.resolve(part.part_id, part.dek_wrap_generation)?;
        let (plaintext_bytes, plaintext_blake3) =
            decrypt_and_hash_plaintext(&staged_path, &dek, limits.max_part_plaintext_bytes)?;
        if plaintext_bytes != part.plaintext_bytes || plaintext_blake3 != part.plaintext_blake3 {
            return Err(L1V2Error::PlaintextMismatch);
        }
        part_evidence.push(L1PartEvidenceV2 {
            part_id: part.part_id,
            ciphertext_bytes,
            ciphertext_blake3,
            plaintext_bytes,
            plaintext_blake3,
        });
    }

    Ok(L1VerificationReportV2 {
        recovery_point_id: signed.manifest.recovery_point_id,
        destination_fingerprint: destination_fingerprint.to_owned(),
        manifest_object_key: manifest_object_key.to_owned(),
        verified_at_ms,
        parts: part_evidence,
    })
}

fn verify_encoding_contract(
    part: &ManifestPartV2,
    limits: L1VerifyLimitsV2,
) -> Result<(), L1V2Error> {
    if part.compression_format != "zstd" || part.encryption_format != "chacha20poly1305-stream" {
        return Err(L1V2Error::EncodingUnsupported);
    }
    if part.ciphertext_bytes > limits.max_part_ciphertext_bytes {
        return Err(L1V2Error::CiphertextLimitExceeded);
    }
    if part.plaintext_bytes > limits.max_part_plaintext_bytes {
        return Err(L1V2Error::PlaintextLimitExceeded);
    }
    Ok(())
}

fn hash_file(path: &std::path::Path) -> Result<(u64, String), L1V2Error> {
    let mut file = std::fs::File::open(path).map_err(|_| L1V2Error::PayloadReadFailed)?;
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| L1V2Error::PayloadReadFailed)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(L1V2Error::CiphertextLimitExceeded)?;
        hasher.update(&buffer[..read]);
    }
    Ok((total, hasher.finalize().to_hex().to_string()))
}

fn decrypt_and_hash_plaintext(
    path: &std::path::Path,
    dek: &Dek,
    max_plaintext_bytes: u64,
) -> Result<(u64, String), L1V2Error> {
    let file = std::fs::File::open(path).map_err(|_| L1V2Error::PayloadReadFailed)?;
    let mut reader = BufReader::with_capacity(CIPHER_CHUNK * 2, file);
    let mut decryptor = Some(dek.payload_decryptor(&NONCE_PREFIX));
    let mut compressed = Vec::new();
    loop {
        let mut block = vec![0_u8; CIPHER_CHUNK];
        let read = read_fully(&mut reader, &mut block).map_err(|_| L1V2Error::DecryptFailed)?;
        if read == 0 {
            return Err(L1V2Error::DecryptFailed);
        }
        block.truncate(read);
        let more = !reader
            .fill_buf()
            .map_err(|_| L1V2Error::DecryptFailed)?
            .is_empty();
        let plaintext = if more {
            decryptor
                .as_mut()
                .ok_or(L1V2Error::DecryptFailed)?
                .decrypt_next(block.as_slice())
                .map_err(|_| L1V2Error::DecryptFailed)?
        } else {
            decryptor
                .take()
                .ok_or(L1V2Error::DecryptFailed)?
                .decrypt_last(block.as_slice())
                .map_err(|_| L1V2Error::DecryptFailed)?
        };
        compressed.extend_from_slice(&plaintext);
        if !more {
            break;
        }
    }

    let mut decoder =
        zstd::stream::read::Decoder::new(&compressed[..]).map_err(|_| L1V2Error::DecryptFailed)?;
    let mut hasher = blake3::Hasher::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = decoder
            .read(&mut buffer)
            .map_err(|_| L1V2Error::DecryptFailed)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(L1V2Error::PlaintextLimitExceeded)?;
        if total > max_plaintext_bytes {
            return Err(L1V2Error::PlaintextLimitExceeded);
        }
        hasher.update(&buffer[..read]);
    }
    Ok((total, hasher.finalize().to_hex().to_string()))
}

fn read_fully<R: Read>(reader: &mut R, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = reader.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    Ok(filled)
}

fn valid_object_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 2_048
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}
