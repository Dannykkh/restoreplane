//! SFTP read path for catalog rebuild, restore, and verification.
//!
//! Reader configuration is intentionally separate from [`crate::dest::SshDest`], the backup
//! writer. Each operation opens a fresh pinned SSH/SFTP session and never follows symlinks.

use async_trait::async_trait;
use dbbackup_catalog::ManifestSource;
use dbbackup_pipeline::{
    DestError, DestWriter, DestinationReaderV2, ReaderObjectMetadataV2, ReaderObjectProbeV2,
};
use russh_sftp::client::SftpSession;
use tokio::io::AsyncReadExt;

use crate::{ssh_connect, HostKeyPolicy, SshAuth};

#[derive(Clone)]
pub struct SshReaderCfg {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub host_key_fingerprint: String,
    pub auth: SshAuth,
    pub remote_dir: String,
}

pub struct SshManifestSource {
    cfg: SshReaderCfg,
}

impl SshManifestSource {
    pub fn new(cfg: SshReaderCfg) -> Self {
        Self { cfg }
    }

    async fn open_sftp(
        &self,
    ) -> anyhow::Result<(
        russh::client::Handle<crate::PinnedClientHandler>,
        SftpSession,
    )> {
        let policy = HostKeyPolicy::new(
            self.cfg.host_key_fingerprint.clone(),
            format!("{}:{}", self.cfg.host, self.cfg.port),
        );
        let handle = ssh_connect(
            &self.cfg.host,
            self.cfg.port,
            &self.cfg.user,
            policy,
            self.cfg.auth.clone(),
        )
        .await?;
        let channel = handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;
        Ok((handle, sftp))
    }
}

/// Convert an SFTP path into a manifest key only when it is strictly below `remote_root`.
#[doc(hidden)]
pub fn manifest_relative_key(remote_root: &str, path: &str) -> Option<String> {
    let root = remote_root.trim_end_matches('/');
    let relative = if root.is_empty() {
        path.trim_start_matches('/')
    } else {
        path.strip_prefix(root)?.strip_prefix('/')?
    };
    if relative.is_empty()
        || (!relative.ends_with(".manifest.json") && !relative.ends_with("set-manifest.json"))
        || relative
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return None;
    }
    Some(relative.to_string())
}

fn remote_object_path(remote_root: &str, key: &str) -> anyhow::Result<String> {
    anyhow::ensure!(
        !key.starts_with('/')
            && !key.contains('\\')
            && !key
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".."),
        "유효하지 않은 목적지 상대 key: {key}"
    );
    let root = remote_root.trim_end_matches('/');
    Ok(if root.is_empty() {
        key.to_string()
    } else {
        format!("{root}/{key}")
    })
}

#[async_trait]
impl ManifestSource for SshManifestSource {
    async fn list_manifests(&self, job_filter: Option<&str>) -> anyhow::Result<Vec<String>> {
        let (_handle, sftp) = self.open_sftp().await?;
        let root = self.cfg.remote_dir.trim_end_matches('/').to_string();
        let start = if root.is_empty() {
            ".".to_string()
        } else {
            root.clone()
        };
        let mut pending = vec![start];
        let mut keys = Vec::new();

        while let Some(dir) = pending.pop() {
            for entry in sftp.read_dir(&dir).await? {
                let file_type = entry.file_type();
                if file_type.is_dir() {
                    pending.push(entry.path());
                } else if file_type.is_file() {
                    if let Some(key) = manifest_relative_key(&root, &entry.path()) {
                        if job_filter.is_none_or(|job| key.starts_with(job)) {
                            keys.push(key);
                        }
                    }
                }
            }
        }

        keys.sort();
        Ok(keys)
    }

    async fn read_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        let path = remote_object_path(&self.cfg.remote_dir, key)?;
        let (_handle, sftp) = self.open_sftp().await?;
        Ok(sftp.read(path).await?)
    }
}

#[async_trait]
impl DestinationReaderV2 for SshManifestSource {
    async fn stat_object(&self, key: &str) -> Result<ReaderObjectMetadataV2, DestError> {
        let path = remote_object_path(&self.cfg.remote_dir, key)
            .map_err(|error| DestError::backend(key, error))?;
        let (_handle, sftp) = self
            .open_sftp()
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let metadata = sftp
            .metadata(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        Ok(ReaderObjectMetadataV2 {
            bytes: metadata.len(),
            version: None,
        })
    }

    async fn probe_object(&self, key: &str) -> Result<ReaderObjectProbeV2, DestError> {
        let path = remote_object_path(&self.cfg.remote_dir, key)
            .map_err(|error| DestError::backend(key, error))?;
        let (_handle, sftp) = self
            .open_sftp()
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let mut file = sftp
            .open(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut hasher = blake3::Hasher::new();
        let mut total = 0_u64;
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|error| DestError::backend(&path, error))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            hasher.update(&buffer[..read]);
        }
        Ok(ReaderObjectProbeV2 {
            bytes: total,
            blake3: hasher.finalize().to_hex().to_string(),
            version: None,
        })
    }

    async fn read_bytes_bounded(&self, key: &str, max_bytes: u64) -> Result<Vec<u8>, DestError> {
        let path = remote_object_path(&self.cfg.remote_dir, key)
            .map_err(|error| DestError::backend(key, error))?;
        let (_handle, sftp) = self
            .open_sftp()
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let size = sftp
            .metadata(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?
            .len();
        if size > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        let capacity = usize::try_from(size).map_err(|_| DestError::ReadLimitExceeded {
            key: key.to_owned(),
            max_bytes,
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        let mut file = sftp
            .open(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        file.read_to_end(&mut bytes)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        Ok(bytes)
    }

    async fn copy_object_to(
        &self,
        key: &str,
        writer: &mut DestWriter,
        max_bytes: u64,
    ) -> Result<ReaderObjectProbeV2, DestError> {
        let path = remote_object_path(&self.cfg.remote_dir, key)
            .map_err(|error| DestError::backend(key, error))?;
        let (_handle, sftp) = self
            .open_sftp()
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let size = sftp
            .metadata(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?
            .len();
        if size > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        let mut file = sftp
            .open(&path)
            .await
            .map_err(|error| DestError::backend(&path, error))?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut hasher = blake3::Hasher::new();
        let mut total = 0_u64;
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|error| DestError::backend(&path, error))?;
            if read == 0 {
                break;
            }
            total = total.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
            if total > max_bytes {
                return Err(DestError::ReadLimitExceeded {
                    key: key.to_owned(),
                    max_bytes,
                });
            }
            writer.write_all(&buffer[..read]).await?;
            hasher.update(&buffer[..read]);
        }
        Ok(ReaderObjectProbeV2 {
            bytes: total,
            blake3: hasher.finalize().to_hex().to_string(),
            version: None,
        })
    }
}
