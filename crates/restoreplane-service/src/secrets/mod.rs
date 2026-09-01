mod acl;
mod dpapi;
mod store;

use std::io;

use restoreplane_catalog::RepositoryError;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeroize::Zeroizing;

pub use acl::{audit_acl_sddl, AclPolicy};
pub use store::{PublicSecretRef, ServiceSecretStore};

pub const MAX_SECRET_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretPurpose {
    DatabaseAuth,
    TlsClientKey,
    TlsCa,
    TunnelAuth,
    BackupWriter,
    RestoreReader,
    RetentionDelete,
    NotificationAuthentication,
    NotificationSigning,
    SigningKey,
    EncryptionKey,
}

impl SecretPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DatabaseAuth => "database_auth",
            Self::TlsClientKey => "tls_client_key",
            Self::TlsCa => "tls_ca",
            Self::TunnelAuth => "tunnel_auth",
            Self::BackupWriter => "backup_writer",
            Self::RestoreReader => "restore_reader",
            Self::RetentionDelete => "retention_delete",
            Self::NotificationAuthentication => "notification_authentication",
            Self::NotificationSigning => "notification_signing",
            Self::SigningKey => "signing_key",
            Self::EncryptionKey => "encryption_key",
        }
    }
}

pub struct SecretBuffer(Zeroizing<Vec<u8>>);

impl SecretBuffer {
    pub fn new(bytes: Vec<u8>) -> Result<Self, SecretStoreError> {
        if bytes.is_empty() || bytes.len() > MAX_SECRET_BYTES {
            return Err(SecretStoreError::InputInvalid);
        }
        Ok(Self(Zeroizing::new(bytes)))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Debug, Error)]
pub enum SecretStoreError {
    #[error("SECRET_STORE_INPUT_INVALID")]
    InputInvalid,
    #[error("SECRET_STORE_IO")]
    Io(#[source] io::Error),
    #[error("SECRET_STORE_CRYPTO_FAILED")]
    CryptoFailed,
    #[error("SECRET_STORE_METADATA_FAILED")]
    Metadata(#[source] RepositoryError),
    #[error("SECRET_STORE_PROVIDER_INVALID")]
    ProviderInvalid,
    #[error("SECRET_STORE_PURPOSE_MISMATCH")]
    PurposeMismatch,
    #[error("SECRET_STORE_REVOKED")]
    Revoked,
    #[error("SECRET_STORE_INTEGRITY_FAILED")]
    IntegrityFailed,
    #[error("SECRET_STORE_ACL_FAILED")]
    AclFailed,
    #[error("SECRET_STORE_PLATFORM_UNSUPPORTED")]
    PlatformUnsupported,
}

impl SecretStoreError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InputInvalid => "SECRET_STORE_INPUT_INVALID",
            Self::Io(_) => "SECRET_STORE_IO",
            Self::CryptoFailed => "SECRET_STORE_CRYPTO_FAILED",
            Self::Metadata(_) => "SECRET_STORE_METADATA_FAILED",
            Self::ProviderInvalid => "SECRET_STORE_PROVIDER_INVALID",
            Self::PurposeMismatch => "SECRET_STORE_PURPOSE_MISMATCH",
            Self::Revoked => "SECRET_STORE_REVOKED",
            Self::IntegrityFailed => "SECRET_STORE_INTEGRITY_FAILED",
            Self::AclFailed => "SECRET_STORE_ACL_FAILED",
            Self::PlatformUnsupported => "SECRET_STORE_PLATFORM_UNSUPPORTED",
        }
    }
}

impl From<io::Error> for SecretStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<RepositoryError> for SecretStoreError {
    fn from(error: RepositoryError) -> Self {
        Self::Metadata(error)
    }
}
