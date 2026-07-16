// crates/dbbackup-dest-ssh/src/connect.rs
//! S07 §3 — 연결 + 인증. 바스티온이든 SFTP 서버든 공통 경로.
//!
//! 인증 자료(passphrase/password/개인키 바이트)는 zeroize::Zeroizing로만 취급하고 **로그/에러에 절대
//! 넣지 않는다**(R7/R12/M6). host/port/user만 로깅 대상.

use std::sync::Arc;

use russh::client::{self, AuthResult};
use zeroize::Zeroizing;

use crate::handler::PinnedClientHandler;
use crate::hostkey::HostKeyPolicy;

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
    let config = Arc::new(client::Config::default());
    let handler = PinnedClientHandler::new(policy);

    let mut handle = client::connect(config, (host, port), handler).await?;

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
            handle.authenticate_publickey(user, key).await?
        }
        SshAuth::Password { password } => {
            handle
                .authenticate_password(user, password.as_str().to_string())
                .await?
        }
    };

    // 인증 실패는 hard fail. 시크릿은 메시지에 넣지 않는다(host/port/user만).
    anyhow::ensure!(authed.success(), "SSH 인증 실패: {host}:{port} user={user}");
    Ok(handle)
}
