use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use rand::rngs::OsRng;
use rand::RngCore;
use restoreplane_catalog::{SecretRefRecord, SecretRefRepository, SecretRefSpec};
use restoreplane_domain::SecretRefId;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{dpapi, AclPolicy, SecretBuffer, SecretPurpose, SecretStoreError, MAX_SECRET_BYTES};

const ENTROPY_BYTES: usize = 32;
const BLOB_MAGIC: [u8; 8] = *b"DBSECR1\0";
const BLOB_VERSION: u16 = 1;
const BLOB_HEADER_BYTES: usize = 16;
const MAX_CIPHERTEXT_BYTES: usize = MAX_SECRET_BYTES + 4 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicSecretRef {
    id: SecretRefId,
    provider: String,
    provider_locator: String,
    purpose: String,
    version: i64,
    state: String,
    fingerprint: String,
    rotated_from_id: Option<SecretRefId>,
    created_at_ms: i64,
    retired_at_ms: Option<i64>,
}

impl PublicSecretRef {
    pub const fn id(&self) -> &SecretRefId {
        &self.id
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn provider_locator(&self) -> &str {
        &self.provider_locator
    }

    pub fn purpose(&self) -> &str {
        &self.purpose
    }

    pub const fn version(&self) -> i64 {
        self.version
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

impl From<SecretRefRecord> for PublicSecretRef {
    fn from(record: SecretRefRecord) -> Self {
        Self {
            id: record.id().clone(),
            provider: record.provider().to_owned(),
            provider_locator: record.provider_locator().to_owned(),
            purpose: record.purpose().to_owned(),
            version: record.version(),
            state: record.state().to_owned(),
            fingerprint: record.fingerprint().to_owned(),
            rotated_from_id: record.rotated_from_id().cloned(),
            created_at_ms: record.created_at_ms(),
            retired_at_ms: record.retired_at_ms(),
        }
    }
}

pub struct ServiceSecretStore {
    root: PathBuf,
    objects: PathBuf,
    entropy_path: PathBuf,
    entropy: Zeroizing<Vec<u8>>,
    repository: SecretRefRepository,
    acl: AclPolicy,
}

impl ServiceSecretStore {
    pub fn open(
        root: PathBuf,
        repository: SecretRefRepository,
        acl: AclPolicy,
    ) -> Result<Self, SecretStoreError> {
        ensure_directory(&root)?;
        acl.apply_directory(&root)?;
        let objects = root.join("objects");
        ensure_directory(&objects)?;
        acl.apply_directory(&objects)?;
        let entropy_path = root.join("entropy.bin");
        let entropy = load_or_create_entropy(&entropy_path, &acl)?;
        Ok(Self {
            root,
            objects,
            entropy_path,
            entropy,
            repository,
            acl,
        })
    }

    pub fn create(
        &self,
        purpose: SecretPurpose,
        plaintext: SecretBuffer,
        created_at_ms: i64,
    ) -> Result<PublicSecretRef, SecretStoreError> {
        if created_at_ms < 0 {
            return Err(SecretStoreError::InputInvalid);
        }
        let locator = Uuid::new_v4().to_string();
        let id = SecretRefId::new();
        let fingerprint = self.fingerprint(plaintext.expose())?;
        let ciphertext = dpapi::protect(plaintext.expose(), &self.entropy)?;
        let path = self.write_blob(&locator, 1, &ciphertext)?;
        let result = self.repository.create(SecretRefSpec::windows_dpapi(
            id,
            &locator,
            purpose.as_str(),
            1,
            fingerprint,
            None,
            created_at_ms,
        ));
        match result {
            Ok(record) => Ok(record.into()),
            Err(error) => {
                let _ = fs::remove_file(path);
                Err(error.into())
            }
        }
    }

    pub fn load_or_create_service_key(
        &self,
        purpose: SecretPurpose,
        created_at_ms: i64,
    ) -> Result<(PublicSecretRef, SecretBuffer), SecretStoreError> {
        if !matches!(
            purpose,
            SecretPurpose::SigningKey | SecretPurpose::EncryptionKey
        ) || created_at_ms < 0
        {
            return Err(SecretStoreError::InputInvalid);
        }
        if let Some(record) = self.repository.active_for_purpose(purpose.as_str())? {
            let metadata = PublicSecretRef::from(record);
            let key = self.resolve(metadata.id(), purpose)?;
            if key.expose().len() != 32 {
                return Err(SecretStoreError::IntegrityFailed);
            }
            return Ok((metadata, key));
        }

        let mut key_material = Zeroizing::new(vec![0_u8; 32]);
        OsRng
            .try_fill_bytes(&mut key_material)
            .map_err(|_| SecretStoreError::CryptoFailed)?;
        let metadata = self.create(
            purpose,
            SecretBuffer::new(key_material.to_vec())?,
            created_at_ms,
        )?;
        let key = self.resolve(metadata.id(), purpose)?;
        Ok((metadata, key))
    }

    pub fn rotate(
        &self,
        previous_id: &SecretRefId,
        purpose: SecretPurpose,
        plaintext: SecretBuffer,
        rotated_at_ms: i64,
    ) -> Result<PublicSecretRef, SecretStoreError> {
        let previous = self.repository.get(previous_id)?;
        self.validate_record(&previous, purpose)?;
        if previous.state() != "active" || rotated_at_ms < previous.created_at_ms() {
            return Err(SecretStoreError::InputInvalid);
        }
        let next_version = previous
            .version()
            .checked_add(1)
            .ok_or(SecretStoreError::InputInvalid)?;
        let fingerprint = self.fingerprint(plaintext.expose())?;
        let ciphertext = dpapi::protect(plaintext.expose(), &self.entropy)?;
        let path = self.write_blob(previous.provider_locator(), next_version, &ciphertext)?;
        let next = SecretRefSpec::windows_dpapi(
            SecretRefId::new(),
            previous.provider_locator(),
            purpose.as_str(),
            next_version,
            fingerprint,
            Some(previous_id.clone()),
            rotated_at_ms,
        );
        match self.repository.rotate(previous_id, next, rotated_at_ms) {
            Ok(record) => Ok(record.into()),
            Err(error) => {
                let _ = fs::remove_file(path);
                Err(error.into())
            }
        }
    }

    pub fn resolve(
        &self,
        id: &SecretRefId,
        expected_purpose: SecretPurpose,
    ) -> Result<SecretBuffer, SecretStoreError> {
        let record = self.repository.get(id)?;
        self.validate_record(&record, expected_purpose)?;
        match record.state() {
            "active" | "retired" => {}
            "revoked" => return Err(SecretStoreError::Revoked),
            _ => return Err(SecretStoreError::ProviderInvalid),
        }
        let path = self.blob_path_for(record.provider_locator(), record.version())?;
        let ciphertext = read_blob(&path)?;
        let plaintext = dpapi::unprotect(&ciphertext, &self.entropy)?;
        if plaintext.is_empty() || plaintext.len() > MAX_SECRET_BYTES {
            return Err(SecretStoreError::IntegrityFailed);
        }
        let fingerprint = self.fingerprint(&plaintext)?;
        if !bool::from(
            fingerprint
                .as_bytes()
                .ct_eq(record.fingerprint().as_bytes()),
        ) {
            return Err(SecretStoreError::IntegrityFailed);
        }
        SecretBuffer::new(plaintext.to_vec())
    }

    pub fn revoke(&self, id: &SecretRefId, revoked_at_ms: i64) -> Result<(), SecretStoreError> {
        self.repository.revoke(id, revoked_at_ms)?;
        Ok(())
    }

    pub fn entropy_path(&self) -> &Path {
        &self.entropy_path
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blob_path(&self, metadata: &PublicSecretRef) -> Result<PathBuf, SecretStoreError> {
        if metadata.provider != "windows_dpapi" {
            return Err(SecretStoreError::ProviderInvalid);
        }
        self.blob_path_for(&metadata.provider_locator, metadata.version)
    }

    fn validate_record(
        &self,
        record: &SecretRefRecord,
        purpose: SecretPurpose,
    ) -> Result<(), SecretStoreError> {
        if record.provider() != "windows_dpapi" {
            return Err(SecretStoreError::ProviderInvalid);
        }
        if record.purpose() != purpose.as_str() {
            return Err(SecretStoreError::PurposeMismatch);
        }
        validate_locator(record.provider_locator())?;
        if record.version() <= 0 {
            return Err(SecretStoreError::ProviderInvalid);
        }
        Ok(())
    }

    fn fingerprint(&self, plaintext: &[u8]) -> Result<String, SecretStoreError> {
        let key: &[u8; 32] = self
            .entropy
            .as_slice()
            .try_into()
            .map_err(|_| SecretStoreError::IntegrityFailed)?;
        Ok(blake3::keyed_hash(key, plaintext).to_hex().to_string())
    }

    fn write_blob(
        &self,
        locator: &str,
        version: i64,
        ciphertext: &[u8],
    ) -> Result<PathBuf, SecretStoreError> {
        if ciphertext.is_empty() || ciphertext.len() > MAX_CIPHERTEXT_BYTES {
            return Err(SecretStoreError::CryptoFailed);
        }
        let path = self.blob_path_for(locator, version)?;
        let parent = path.parent().ok_or(SecretStoreError::InputInvalid)?;
        ensure_directory(parent)?;
        self.acl.apply_directory(parent)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        file.write_all(&encode_blob(ciphertext)?)?;
        file.sync_all()?;
        self.acl.apply_file(&path)?;
        Ok(path)
    }

    fn blob_path_for(&self, locator: &str, version: i64) -> Result<PathBuf, SecretStoreError> {
        validate_locator(locator)?;
        if version <= 0 {
            return Err(SecretStoreError::InputInvalid);
        }
        Ok(self
            .objects
            .join(locator)
            .join(format!("v{version:08}.dpapi")))
    }
}

fn ensure_directory(path: &Path) -> Result<(), SecretStoreError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(SecretStoreError::InputInvalid);
        }
    } else {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

fn load_or_create_entropy(
    path: &Path,
    acl: &AclPolicy,
) -> Result<Zeroizing<Vec<u8>>, SecretStoreError> {
    if path.exists() {
        acl.apply_file(path)?;
        return read_entropy(path);
    }
    let mut entropy = Zeroizing::new(vec![0_u8; ENTROPY_BYTES]);
    OsRng
        .try_fill_bytes(&mut entropy)
        .map_err(|_| SecretStoreError::CryptoFailed)?;
    match OpenOptions::new().create_new(true).write(true).open(path) {
        Ok(mut file) => {
            file.write_all(&entropy)?;
            file.sync_all()?;
            acl.apply_file(path)?;
            Ok(entropy)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            acl.apply_file(path)?;
            read_entropy(path)
        }
        Err(error) => Err(error.into()),
    }
}

fn read_entropy(path: &Path) -> Result<Zeroizing<Vec<u8>>, SecretStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() != 32 {
        return Err(SecretStoreError::IntegrityFailed);
    }
    let entropy = Zeroizing::new(fs::read(path)?);
    if entropy.len() != ENTROPY_BYTES {
        return Err(SecretStoreError::IntegrityFailed);
    }
    Ok(entropy)
}

fn validate_locator(locator: &str) -> Result<(), SecretStoreError> {
    let parsed = Uuid::parse_str(locator).map_err(|_| SecretStoreError::ProviderInvalid)?;
    if parsed.to_string() != locator {
        return Err(SecretStoreError::ProviderInvalid);
    }
    Ok(())
}

fn encode_blob(ciphertext: &[u8]) -> Result<Vec<u8>, SecretStoreError> {
    let length: u32 = ciphertext
        .len()
        .try_into()
        .map_err(|_| SecretStoreError::InputInvalid)?;
    let mut output = Vec::with_capacity(BLOB_HEADER_BYTES + ciphertext.len());
    output.extend_from_slice(&BLOB_MAGIC);
    output.extend_from_slice(&BLOB_VERSION.to_le_bytes());
    output.extend_from_slice(&0_u16.to_le_bytes());
    output.extend_from_slice(&length.to_le_bytes());
    output.extend_from_slice(ciphertext);
    Ok(output)
}

fn read_blob(path: &Path) -> Result<Vec<u8>, SecretStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() as usize > BLOB_HEADER_BYTES + MAX_CIPHERTEXT_BYTES
    {
        return Err(SecretStoreError::IntegrityFailed);
    }
    let bytes = fs::read(path)?;
    if bytes.len() < BLOB_HEADER_BYTES
        || bytes[0..8] != BLOB_MAGIC
        || u16::from_le_bytes(bytes[8..10].try_into().expect("fixed header")) != BLOB_VERSION
        || u16::from_le_bytes(bytes[10..12].try_into().expect("fixed header")) != 0
    {
        return Err(SecretStoreError::IntegrityFailed);
    }
    let length = u32::from_le_bytes(bytes[12..16].try_into().expect("fixed header")) as usize;
    if length == 0 || length > MAX_CIPHERTEXT_BYTES || BLOB_HEADER_BYTES + length != bytes.len() {
        return Err(SecretStoreError::IntegrityFailed);
    }
    Ok(bytes[BLOB_HEADER_BYTES..].to_vec())
}
