//! Host-key-pinned SFTP delete authority for retention work.
//!
//! The delete credential is configured independently from both the backup writer and restore
//! reader. The operator accepts only an unversioned, safe relative key below one absolute remote
//! root and maps the SFTP v3 status code instead of guessing from error text.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use restoreplane_pipeline::{DeleteErrorV2, DestinationDeleteV2};
use russh_sftp::client::{error::Error as SftpError, SftpSession};
use russh_sftp::protocol::StatusCode;

use crate::connect::{ssh_connect_for_retention, SshConnectTimeoutError};
use crate::{HostKeyPolicy, SshAuth};

const SSH_DELETE_STAGE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct SshDeleteCfg {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub host_key_fingerprint: String,
    pub auth: SshAuth,
    pub remote_dir: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SshDeleteConfigError {
    #[error("invalid SSH delete configuration field: {0}")]
    InvalidField(&'static str),
}

pub struct SshDeleteOperatorV2 {
    remote_dir: String,
    backend: Arc<dyn SftpDeleteBackend>,
}

impl SshDeleteOperatorV2 {
    pub fn new(mut cfg: SshDeleteCfg) -> Result<Self, SshDeleteConfigError> {
        validate_config(&cfg)?;
        cfg.remote_dir = cfg.remote_dir.trim_end_matches('/').to_owned();
        Ok(Self {
            remote_dir: cfg.remote_dir.clone(),
            backend: Arc::new(PinnedSftpDeleteBackend { cfg }),
        })
    }

    #[cfg(test)]
    fn from_backend(remote_dir: impl Into<String>, backend: Arc<dyn SftpDeleteBackend>) -> Self {
        Self {
            remote_dir: remote_dir.into().trim_end_matches('/').to_owned(),
            backend,
        }
    }
}

#[async_trait]
impl DestinationDeleteV2 for SshDeleteOperatorV2 {
    async fn delete_object(&self, key: &str, version: Option<&str>) -> Result<(), DeleteErrorV2> {
        if version.is_some() {
            return Err(terminal_input_error(
                key,
                "SFTP destinations do not support object versions",
            ));
        }
        if !valid_relative_key(key) {
            return Err(terminal_input_error(
                key,
                "invalid relative SFTP object key",
            ));
        }
        let path = format!("{}/{key}", self.remote_dir);
        match self.backend.remove_file(&path).await {
            Ok(()) | Err(SftpDeleteFailure::NotFound) => Ok(()),
            Err(SftpDeleteFailure::Backend { message, retryable }) => Err(DeleteErrorV2::Backend {
                key: key.to_owned(),
                message,
                retryable,
            }),
        }
    }
}

#[async_trait]
trait SftpDeleteBackend: Send + Sync {
    async fn remove_file(&self, path: &str) -> Result<(), SftpDeleteFailure>;
}

struct PinnedSftpDeleteBackend {
    cfg: SshDeleteCfg,
}

#[async_trait]
impl SftpDeleteBackend for PinnedSftpDeleteBackend {
    async fn remove_file(&self, path: &str) -> Result<(), SftpDeleteFailure> {
        let policy = HostKeyPolicy::new(
            self.cfg.host_key_fingerprint.clone(),
            format!("{}:{}", self.cfg.host, self.cfg.port),
        );
        let handle = ssh_connect_for_retention(
            &self.cfg.host,
            self.cfg.port,
            &self.cfg.user,
            policy,
            self.cfg.auth.clone(),
        )
        .await
        .map_err(classify_connect_error)?;
        let channel = bounded_sftp_stage(
            SSH_DELETE_STAGE_TIMEOUT,
            "SSH_DELETE_CHANNEL_OPEN_TIMEOUT",
            handle.channel_open_session(),
        )
        .await?
        .map_err(classify_ssh_error)?;
        bounded_sftp_stage(
            SSH_DELETE_STAGE_TIMEOUT,
            "SSH_DELETE_SUBSYSTEM_TIMEOUT",
            channel.request_subsystem(true, "sftp"),
        )
        .await?
        .map_err(classify_ssh_error)?;
        let sftp = bounded_sftp_stage(
            SSH_DELETE_STAGE_TIMEOUT,
            "SSH_DELETE_SFTP_INIT_TIMEOUT",
            SftpSession::new(channel.into_stream()),
        )
        .await?
        .map_err(classify_sftp_error)?;
        bounded_sftp_stage(
            SSH_DELETE_STAGE_TIMEOUT,
            "SSH_DELETE_REMOVE_TIMEOUT",
            sftp.remove_file(path),
        )
        .await?
        .map_err(classify_sftp_error)
    }
}

#[derive(Debug, PartialEq, Eq)]
enum SftpDeleteFailure {
    NotFound,
    Backend { message: String, retryable: bool },
}

fn classify_connect_error(error: anyhow::Error) -> SftpDeleteFailure {
    if let Some(error) = error.downcast_ref::<SshConnectTimeoutError>() {
        return backend_failure(error.to_string(), true);
    }
    if let Some(error) = error.downcast_ref::<russh::Error>() {
        return backend_failure(error.to_string(), retryable_ssh_error(error));
    }
    if error.downcast_ref::<russh::keys::Error>().is_some()
        || error.to_string().starts_with("SSH 인증 실패:")
    {
        return backend_failure(error.to_string(), false);
    }
    backend_failure(error.to_string(), false)
}

async fn bounded_sftp_stage<T>(
    timeout: Duration,
    timeout_code: &'static str,
    future: impl Future<Output = T>,
) -> Result<T, SftpDeleteFailure> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| backend_failure(timeout_code.to_owned(), true))
}

fn classify_ssh_error(error: russh::Error) -> SftpDeleteFailure {
    let retryable = retryable_ssh_error(&error);
    backend_failure(error.to_string(), retryable)
}

fn retryable_ssh_error(error: &russh::Error) -> bool {
    match error {
        russh::Error::IO(error) => matches!(
            error.kind(),
            std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::ConnectionRefused
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::HostUnreachable
                | std::io::ErrorKind::Interrupted
                | std::io::ErrorKind::NetworkDown
                | std::io::ErrorKind::NetworkUnreachable
                | std::io::ErrorKind::NotConnected
                | std::io::ErrorKind::TimedOut
                | std::io::ErrorKind::WouldBlock
        ),
        russh::Error::ConnectionTimeout
        | russh::Error::KeepaliveTimeout
        | russh::Error::InactivityTimeout
        | russh::Error::Disconnect
        | russh::Error::HUP
        | russh::Error::SendError
        | russh::Error::RecvError
        | russh::Error::Pending
        | russh::Error::Elapsed(_) => true,
        _ => false,
    }
}

fn classify_sftp_error(error: SftpError) -> SftpDeleteFailure {
    match &error {
        SftpError::Status(status) if status.status_code == StatusCode::NoSuchFile => {
            SftpDeleteFailure::NotFound
        }
        SftpError::Status(status)
            if matches!(
                status.status_code,
                StatusCode::NoConnection | StatusCode::ConnectionLost | StatusCode::Failure
            ) =>
        {
            backend_failure(error.to_string(), true)
        }
        SftpError::Timeout | SftpError::Limited(_) | SftpError::IO(_) => {
            backend_failure(error.to_string(), true)
        }
        SftpError::Status(_) | SftpError::UnexpectedPacket | SftpError::UnexpectedBehavior(_) => {
            backend_failure(error.to_string(), false)
        }
    }
}

fn backend_failure(message: String, retryable: bool) -> SftpDeleteFailure {
    SftpDeleteFailure::Backend {
        message: message.chars().take(2_048).collect(),
        retryable,
    }
}

fn terminal_input_error(key: &str, message: &str) -> DeleteErrorV2 {
    DeleteErrorV2::Backend {
        key: key.to_owned(),
        message: message.to_owned(),
        retryable: false,
    }
}

fn validate_config(cfg: &SshDeleteCfg) -> Result<(), SshDeleteConfigError> {
    for (field, value) in [
        ("host", cfg.host.as_str()),
        ("user", cfg.user.as_str()),
        ("host_key_fingerprint", cfg.host_key_fingerprint.as_str()),
        ("remote_dir", cfg.remote_dir.as_str()),
    ] {
        if value.trim().is_empty()
            || value
                .chars()
                .any(|character| matches!(character, '\0' | '\r' | '\n'))
        {
            return Err(SshDeleteConfigError::InvalidField(field));
        }
    }
    if cfg.port == 0 {
        return Err(SshDeleteConfigError::InvalidField("port"));
    }
    let fingerprint = cfg
        .host_key_fingerprint
        .strip_prefix("SHA256:")
        .ok_or(SshDeleteConfigError::InvalidField("host_key_fingerprint"))?;
    if fingerprint.len() != 43
        || !fingerprint
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'+' | b'/'))
    {
        return Err(SshDeleteConfigError::InvalidField("host_key_fingerprint"));
    }
    if !valid_absolute_remote_dir(&cfg.remote_dir) {
        return Err(SshDeleteConfigError::InvalidField("remote_dir"));
    }
    match &cfg.auth {
        SshAuth::Password { password } if password.is_empty() => {
            Err(SshDeleteConfigError::InvalidField("auth"))
        }
        SshAuth::PublicKey { identity_pem, .. } if identity_pem.is_empty() => {
            Err(SshDeleteConfigError::InvalidField("auth"))
        }
        _ => Ok(()),
    }
}

fn valid_absolute_remote_dir(path: &str) -> bool {
    path.starts_with('/')
        && !path.contains('\\')
        && !path
            .trim_matches('/')
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

fn valid_relative_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 2_048
        && !key.starts_with('/')
        && !key.contains('\\')
        && !key
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        && !key
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use russh_sftp::protocol::Status;

    use super::*;

    struct RecordingBackend {
        paths: Mutex<Vec<String>>,
        outcomes: Mutex<VecDeque<Result<(), SftpDeleteFailure>>>,
    }

    #[async_trait]
    impl SftpDeleteBackend for RecordingBackend {
        async fn remove_file(&self, path: &str) -> Result<(), SftpDeleteFailure> {
            self.paths.lock().expect("paths").push(path.to_owned());
            self.outcomes
                .lock()
                .expect("outcomes")
                .pop_front()
                .expect("configured outcome")
        }
    }

    #[tokio::test]
    async fn delete_and_missing_replay_are_idempotent_under_the_remote_root() {
        let backend = Arc::new(RecordingBackend {
            paths: Mutex::new(Vec::new()),
            outcomes: Mutex::new(VecDeque::from([Ok(()), Err(SftpDeleteFailure::NotFound)])),
        });
        let delete = SshDeleteOperatorV2::from_backend("/srv/backups/", backend.clone());

        delete
            .delete_object("recovery/payload.bin", None)
            .await
            .expect("delete object");
        delete
            .delete_object("recovery/payload.bin", None)
            .await
            .expect("missing replay");
        assert_eq!(
            *backend.paths.lock().expect("paths"),
            [
                "/srv/backups/recovery/payload.bin",
                "/srv/backups/recovery/payload.bin"
            ]
        );
    }

    #[test]
    fn sftp_status_codes_have_deterministic_retry_semantics() {
        assert_eq!(
            classify_sftp_error(status_error(StatusCode::NoSuchFile)),
            SftpDeleteFailure::NotFound
        );
        assert!(matches!(
            classify_sftp_error(status_error(StatusCode::ConnectionLost)),
            SftpDeleteFailure::Backend {
                retryable: true,
                ..
            }
        ));
        assert!(matches!(
            classify_sftp_error(status_error(StatusCode::PermissionDenied)),
            SftpDeleteFailure::Backend {
                retryable: false,
                ..
            }
        ));
        assert!(matches!(
            classify_sftp_error(status_error(StatusCode::OpUnsupported)),
            SftpDeleteFailure::Backend {
                retryable: false,
                ..
            }
        ));
    }

    #[test]
    fn ssh_transport_timeouts_retry_but_host_key_rejection_does_not() {
        assert!(retryable_ssh_error(&russh::Error::ConnectionTimeout));
        assert!(!retryable_ssh_error(&russh::Error::UnknownKey));
    }

    #[tokio::test]
    async fn pending_channel_sftp_and_remove_stages_are_bounded_and_retryable() {
        for code in [
            "SSH_DELETE_CHANNEL_OPEN_TIMEOUT",
            "SSH_DELETE_SUBSYSTEM_TIMEOUT",
            "SSH_DELETE_SFTP_INIT_TIMEOUT",
            "SSH_DELETE_REMOVE_TIMEOUT",
        ] {
            let error = bounded_sftp_stage(
                Duration::from_millis(10),
                code,
                std::future::pending::<()>(),
            )
            .await
            .expect_err("pending SFTP stage must time out");
            assert_eq!(
                error,
                SftpDeleteFailure::Backend {
                    message: code.to_owned(),
                    retryable: true,
                }
            );
        }
    }

    fn status_error(status_code: StatusCode) -> SftpError {
        SftpError::Status(Status {
            id: 1,
            status_code,
            error_message: "fixture".to_owned(),
            language_tag: "en".to_owned(),
        })
    }
}
