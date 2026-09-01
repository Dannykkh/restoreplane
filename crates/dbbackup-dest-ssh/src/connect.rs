// crates/dbbackup-dest-ssh/src/connect.rs
//! S07 §3 — 연결 + 인증. 바스티온이든 SFTP 서버든 공통 경로.
//!
//! 인증 자료(passphrase/password/개인키 바이트)는 zeroize::Zeroizing로만 취급하고 **로그/에러에 절대
//! 넣지 않는다**(R7/R12/M6). host/port/user만 로깅 대상.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use russh::client::{self, AuthResult};
use zeroize::Zeroizing;

use crate::handler::PinnedClientHandler;
use crate::hostkey::HostKeyPolicy;

const RETENTION_SSH_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const RETENTION_SSH_AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const RETENTION_SSH_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum SshConnectTimeoutError {
    #[error("SSH_DELETE_CONNECT_TIMEOUT")]
    Connect,
    #[error("SSH_DELETE_AUTH_TIMEOUT")]
    Authentication,
}

#[derive(Clone, Copy)]
struct SshConnectBounds {
    connect: Duration,
    authentication: Duration,
    inactivity: Duration,
}

impl SshConnectBounds {
    const fn retention() -> Self {
        Self {
            connect: RETENTION_SSH_CONNECT_TIMEOUT,
            authentication: RETENTION_SSH_AUTH_TIMEOUT,
            inactivity: RETENTION_SSH_INACTIVITY_TIMEOUT,
        }
    }
}

/// 공개키(파일+선택 passphrase) 또는 password 인증. 시크릿은 Zeroizing.
#[derive(Clone)]
pub enum SshAuth {
    PublicKey {
        identity_pem: Zeroizing<String>,
        passphrase: Option<Zeroizing<String>>,
    },
    Password {
        password: Zeroizing<String>,
    },
}

/// 바스티온/ SFTP 서버 공통 접속·인증. host key 검증(check_server_key)이 KEX 중 호출되어 불일치면 실패.
pub async fn ssh_connect(
    host: &str,
    port: u16,
    user: &str,
    policy: HostKeyPolicy,
    auth: SshAuth,
) -> anyhow::Result<client::Handle<PinnedClientHandler>> {
    ssh_connect_with_bounds(host, port, user, policy, auth, None).await
}

pub(crate) async fn ssh_connect_for_retention(
    host: &str,
    port: u16,
    user: &str,
    policy: HostKeyPolicy,
    auth: SshAuth,
) -> anyhow::Result<client::Handle<PinnedClientHandler>> {
    ssh_connect_with_bounds(
        host,
        port,
        user,
        policy,
        auth,
        Some(SshConnectBounds::retention()),
    )
    .await
}

async fn ssh_connect_with_bounds(
    host: &str,
    port: u16,
    user: &str,
    policy: HostKeyPolicy,
    auth: SshAuth,
    bounds: Option<SshConnectBounds>,
) -> anyhow::Result<client::Handle<PinnedClientHandler>> {
    let mut config = client::Config::default();
    if let Some(bounds) = bounds {
        config.inactivity_timeout = Some(bounds.inactivity);
    }
    let config = Arc::new(config);
    let handler = PinnedClientHandler::new(policy);

    let connect = client::connect(config, (host, port), handler);
    let mut handle = if let Some(bounds) = bounds {
        bound_ssh_phase(bounds.connect, SshConnectTimeoutError::Connect, connect)
            .await
            .map_err(anyhow::Error::new)??
    } else {
        connect.await?
    };

    let authed: AuthResult = match auth {
        SshAuth::PublicKey {
            identity_pem,
            passphrase,
        } => {
            let key = russh::keys::decode_secret_key(
                identity_pem.as_str(),
                passphrase.as_ref().map(|s| s.as_str()),
            )?;
            let key = russh::keys::PrivateKeyWithHashAlg::new(Arc::new(key), None);
            let authentication = handle.authenticate_publickey(user, key);
            if let Some(bounds) = bounds {
                bound_ssh_phase(
                    bounds.authentication,
                    SshConnectTimeoutError::Authentication,
                    authentication,
                )
                .await
                .map_err(anyhow::Error::new)??
            } else {
                authentication.await?
            }
        }
        SshAuth::Password { password } => {
            let authentication = handle.authenticate_password(user, password.as_str().to_string());
            if let Some(bounds) = bounds {
                bound_ssh_phase(
                    bounds.authentication,
                    SshConnectTimeoutError::Authentication,
                    authentication,
                )
                .await
                .map_err(anyhow::Error::new)??
            } else {
                authentication.await?
            }
        }
    };

    // 인증 실패는 hard fail. 시크릿은 메시지에 넣지 않는다(host/port/user만).
    anyhow::ensure!(authed.success(), "SSH 인증 실패: {host}:{port} user={user}");
    Ok(handle)
}

async fn bound_ssh_phase<T>(
    timeout: Duration,
    timeout_error: SshConnectTimeoutError,
    future: impl Future<Output = T>,
) -> Result<T, SshConnectTimeoutError> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| timeout_error)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn pending_connect_and_auth_phases_return_stable_timeout_codes() {
        for expected in [
            SshConnectTimeoutError::Connect,
            SshConnectTimeoutError::Authentication,
        ] {
            let error = bound_ssh_phase(
                Duration::from_millis(10),
                expected,
                std::future::pending::<()>(),
            )
            .await
            .expect_err("pending SSH phase must time out");
            assert_eq!(error, expected);
        }
    }
}
