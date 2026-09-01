//! S3 read path for catalog rebuild, restore, and verification.
//!
//! This type deliberately accepts a reader-only credential shape instead of reusing [`crate::S3Cfg`],
//! which represents the append-only backup writer identity.

use async_trait::async_trait;
use dbbackup_catalog::ManifestSource;
use dbbackup_pipeline::{
    DestError, DestWriter, DestinationReaderV2, ReaderObjectMetadataV2, ReaderObjectProbeV2,
};
use opendal::{EntryMode, Operator};

use crate::object_key;

#[derive(Clone)]
pub struct S3ReaderCfg {
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

pub struct S3ManifestSource {
    op: Operator,
    prefix: String,
}

impl S3ManifestSource {
    pub fn new(cfg: S3ReaderCfg) -> anyhow::Result<Self> {
        // writer 없이 reader만 만드는 복구 프로세스도 HTTP transport를 명시적으로 설치한다.
        opendal::install_default();

        let mut builder = opendal::services::S3::default()
            .bucket(&cfg.bucket)
            .region(&cfg.region)
            .access_key_id(&cfg.access_key_id)
            .secret_access_key(&cfg.secret_access_key);
        if !cfg.endpoint.is_empty() {
            builder = builder.endpoint(&cfg.endpoint);
        }
        let op = Operator::new(builder)?;
        Ok(Self::from_operator(op, cfg.prefix))
    }

    /// Inject an operator for compatible backends and deterministic contract tests.
    #[doc(hidden)]
    pub fn from_operator(op: Operator, prefix: impl Into<String>) -> Self {
        Self {
            op,
            prefix: prefix.into().trim_matches('/').to_string(),
        }
    }

    fn list_root(&self) -> String {
        if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        }
    }

    fn object_path(&self, key: &str) -> Result<String, DestError> {
        if key.is_empty()
            || key.starts_with('/')
            || key.contains('\\')
            || key
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(DestError::InvalidKey {
                key: key.to_owned(),
            });
        }
        Ok(object_key(&self.prefix, key))
    }
}

#[async_trait]
impl ManifestSource for S3ManifestSource {
    async fn list_manifests(&self, job_filter: Option<&str>) -> anyhow::Result<Vec<String>> {
        let root = self.list_root();
        let entries = self.op.list_with(&root).recursive(true).await?;
        let mut keys = entries
            .into_iter()
            .filter(|entry| entry.metadata().mode() == EntryMode::FILE)
            .filter_map(|entry| {
                let relative = entry.path().strip_prefix(&root)?;
                if !relative.ends_with(".manifest.json") && !relative.ends_with("set-manifest.json")
                {
                    return None;
                }
                if job_filter.is_some_and(|job| !relative.starts_with(job)) {
                    return None;
                }
                Some(relative.to_string())
            })
            .collect::<Vec<_>>();
        keys.sort();
        Ok(keys)
    }

    async fn read_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        Ok(self.op.read(&object_key(&self.prefix, key)).await?.to_vec())
    }
}

#[async_trait]
impl DestinationReaderV2 for S3ManifestSource {
    async fn stat_object(&self, key: &str) -> Result<ReaderObjectMetadataV2, DestError> {
        let path = self.object_path(key)?;
        let metadata = self
            .op
            .stat(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        Ok(ReaderObjectMetadataV2 {
            bytes: metadata.content_length(),
            version: metadata.version().map(ToOwned::to_owned),
        })
    }

    async fn probe_object(&self, key: &str) -> Result<ReaderObjectProbeV2, DestError> {
        let path = self.object_path(key)?;
        let metadata = self
            .op
            .stat(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let size = metadata.content_length();
        let version = metadata.version().map(ToOwned::to_owned);
        let reader = self
            .op
            .reader(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0_u64;
        while offset < size {
            let end = offset.saturating_add(1024 * 1024).min(size);
            let bytes = reader
                .read(offset..end)
                .await
                .map_err(|error| DestError::backend(&path, error))?;
            hasher.update(&bytes.to_vec());
            offset = end;
        }
        Ok(ReaderObjectProbeV2 {
            bytes: size,
            blake3: hasher.finalize().to_hex().to_string(),
            version,
        })
    }

    async fn read_bytes_bounded(&self, key: &str, max_bytes: u64) -> Result<Vec<u8>, DestError> {
        let path = self.object_path(key)?;
        let metadata = self
            .op
            .stat(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let size = metadata.content_length();
        if size > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        self.op
            .read(&path)
            .await
            .map(|buffer| buffer.to_vec())
            .map_err(|error| DestError::backend(&path, error))
    }

    async fn copy_object_to(
        &self,
        key: &str,
        writer: &mut DestWriter,
        max_bytes: u64,
    ) -> Result<ReaderObjectProbeV2, DestError> {
        let path = self.object_path(key)?;
        let metadata = self
            .op
            .stat(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let size = metadata.content_length();
        let version = metadata.version().map(ToOwned::to_owned);
        if size > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        let reader = self
            .op
            .reader(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let mut hasher = blake3::Hasher::new();
        let mut offset = 0_u64;
        while offset < size {
            let end = offset.saturating_add(1024 * 1024).min(size);
            let bytes = reader
                .read(offset..end)
                .await
                .map_err(|error| DestError::backend(&path, error))?
                .to_vec();
            writer.write_all(&bytes).await?;
            hasher.update(&bytes);
            offset = end;
        }
        Ok(ReaderObjectProbeV2 {
            bytes: size,
            blake3: hasher.finalize().to_hex().to_string(),
            version,
        })
    }
}
