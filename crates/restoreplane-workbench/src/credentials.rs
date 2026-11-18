use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use restoreplane_domain::workbench::Scope;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

use crate::driver::DriverLogin;
use crate::session::{InvocationContext, NativePrincipal};
use crate::store::{reject_symlink, UserRootPolicy};
use crate::WorkbenchError;

const PURPOSE: &str = "restoreplane.workbench.credential.v1";
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;

#[async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn resolve(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        reference: Uuid,
    ) -> Result<DriverLogin, WorkbenchError>;
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CredentialInfo {
    pub credential_ref: Uuid,
    pub label: String,
    pub username: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialFile {
    owner_id: String,
    scope: Scope,
    reference: Uuid,
    label: String,
    username: String,
    secret: String,
}
impl Drop for CredentialFile {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}

#[derive(Clone)]
pub struct LocalCredentials {
    root: PathBuf,
    owner_id: String,
}

impl LocalCredentials {
    pub fn open(
        root: &Path,
        principal: &NativePrincipal,
        policy: &dyn UserRootPolicy,
    ) -> Result<Self, WorkbenchError> {
        principal.ensure_standard()?;
        policy.ensure_private_root(root)?;
        reject_symlink(root)?;
        Ok(Self {
            root: root.into(),
            owner_id: principal.owner_id().into(),
        })
    }

    pub fn put(
        &self,
        context: &InvocationContext,
        scope: Scope,
        reference: Option<Uuid>,
        label: String,
        username: String,
        secret: Zeroizing<String>,
    ) -> Result<Uuid, WorkbenchError> {
        self.authorize(context)?;
        if label.len() > 256
            || username.len() > 1024
            || secret.len() > 32 * 1024
            || reference.is_some_and(|id| id.is_nil())
        {
            return Err(WorkbenchError::InvalidInput);
        }
        let reference = reference.unwrap_or_else(Uuid::new_v4);
        if self.path(reference).exists() {
            let existing = self.read(reference)?;
            if existing.scope != scope {
                return Err(WorkbenchError::Forbidden);
            }
        }
        let file = CredentialFile {
            owner_id: self.owner_id.clone(),
            scope,
            reference,
            label,
            username,
            secret: secret.to_string(),
        };
        let plaintext =
            Zeroizing::new(serde_json::to_vec(&file).map_err(|_| WorkbenchError::InvalidInput)?);
        if plaintext.len() > MAX_CREDENTIAL_BYTES {
            return Err(WorkbenchError::Limit);
        }
        let ciphertext = restoreplane_secrets::protect_current_user(&plaintext, PURPOSE)
            .map_err(|_| WorkbenchError::Io)?;
        let destination = self.path(reference);
        reject_symlink(&destination)?;
        let temporary = self.root.join(format!("{}.pending", Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| WorkbenchError::Io)?;
            file.write_all(&ciphertext)
                .and_then(|_| file.sync_all())
                .map_err(|_| WorkbenchError::Io)?;
            drop(file);
            std::fs::rename(&temporary, destination).map_err(|_| WorkbenchError::Io)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result?;
        Ok(reference)
    }

    pub fn list(
        &self,
        context: &InvocationContext,
        scope: &Scope,
    ) -> Result<Vec<CredentialInfo>, WorkbenchError> {
        self.authorize(context)?;
        let mut items = Vec::new();
        for (index, entry) in std::fs::read_dir(&self.root)
            .map_err(|_| WorkbenchError::Io)?
            .take(1001)
            .enumerate()
        {
            // Refuse an incomplete listing instead of returning a truncated success.
            if index == 1000 {
                return Err(WorkbenchError::Limit);
            }
            let entry = entry.map_err(|_| WorkbenchError::Io)?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(stem) = name.strip_suffix(".credential") else {
                continue;
            };
            let Ok(reference) = Uuid::parse_str(stem) else {
                continue;
            };
            let file = self.read(reference)?;
            if &file.scope == scope {
                items.push(CredentialInfo {
                    credential_ref: reference,
                    label: file.label.clone(),
                    username: file.username.clone(),
                });
            }
            if items.len() > 1000 {
                return Err(WorkbenchError::Limit);
            }
        }
        items.sort_by_key(|item| item.credential_ref);
        Ok(items)
    }

    pub fn delete(
        &self,
        context: &InvocationContext,
        reference: Uuid,
    ) -> Result<bool, WorkbenchError> {
        self.authorize(context)?;
        match std::fs::remove_file(self.path(reference)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(WorkbenchError::Io),
        }
    }

    fn path(&self, reference: Uuid) -> PathBuf {
        self.root.join(format!("{reference}.credential"))
    }
    fn authorize(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        context.principal().ensure_standard()?;
        if context.principal().owner_id() != self.owner_id {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(())
    }
    fn read(&self, reference: Uuid) -> Result<CredentialFile, WorkbenchError> {
        let path = self.path(reference);
        reject_symlink(&path)?;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| WorkbenchError::Io)?
            .take((MAX_CREDENTIAL_BYTES + 8192 + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| WorkbenchError::Io)?;
        if bytes.len() > MAX_CREDENTIAL_BYTES + 8192 {
            return Err(WorkbenchError::Limit);
        }
        let plaintext = restoreplane_secrets::unprotect_current_user(&bytes, PURPOSE)
            .map_err(|_| WorkbenchError::Io)?;
        let file: CredentialFile =
            serde_json::from_slice(&plaintext).map_err(|_| WorkbenchError::InvalidInput)?;
        if file.owner_id != self.owner_id || file.reference != reference {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(file)
    }
}

#[async_trait]
impl CredentialProvider for LocalCredentials {
    async fn resolve(
        &self,
        context: &InvocationContext,
        scope: &Scope,
        reference: Uuid,
    ) -> Result<DriverLogin, WorkbenchError> {
        self.authorize(context)?;
        let provider = self.clone();
        let file = tokio::task::spawn_blocking(move || provider.read(reference))
            .await
            .map_err(|_| WorkbenchError::Io)??;
        if &file.scope != scope {
            return Err(WorkbenchError::Forbidden);
        }
        Ok(DriverLogin {
            username: file.username.clone(),
            secret: Zeroizing::new(file.secret.clone()),
        })
    }
}
