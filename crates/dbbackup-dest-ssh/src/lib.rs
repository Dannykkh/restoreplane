// crates/dbbackup-dest-ssh/src/lib.rs
//! S07 — SshDest(russh 0.62 SFTP, rename-commit) + Tunnel(direct-tcpip) + STRICT host-key 핀.
//!
//! 두 사용처:
//!   1. Tunnel(터널) — 바스티온 뒤 DB에 닿는 경유 채널(direct-tcpip → 로컬 소켓 브리지). Destination이
//!      아니라 BackupSource가 DB에 닿는 경로를 여는 인프라. → [`tunnel`]
//!   2. SshDest(목적지) — 백업본이 착지하는 원격 SFTP 저장처. S06 Destination 구현. → [`dest`]
//!
//! 공통 하중 = host-key 검증(STRICT 핀). OpenDAL sftp는 Unix 전용이라 Windows에서 빌드 불가 →
//! 반드시 russh 경로로 SFTP를 처리한다(구조적 결정).
//!
//! [편차/실측 — russh 0.62 API]
//! - Handler는 native async fn in trait(RPITIT) → `#[async_trait]` 미사용(handler.rs).
//! - `check_server_key(&mut self, &PublicKey) -> Result<bool, Error>`, `Ok(false)`면 연결 거부.
//! - `channel_open_direct_tcpip(host: Into<String>, port: u32, orig: Into<String>, orig_port: u32)`.
//! - `authenticate_password/publickey`는 `AuthResult` 반환(`.success()`), `decode_secret_key` +
//!   `PrivateKeyWithHashAlg::new(Arc<PrivateKey>, Option<HashAlg>)`로 개인키 로드.
//! - 지문은 `PublicKey::fingerprint(HashAlg::Sha256).to_string()` = "SHA256:<base64>"(ssh-keygen 정합).
//! - russh-sftp는 stream-agnostic: `SftpSession::new(channel.into_stream())`.

pub mod connect;
pub mod delete;
pub mod dest;
pub mod handler;
pub mod hostkey;
pub mod source;
pub mod tunnel;

#[cfg(test)]
mod dest_caps_tests;

pub use connect::{ssh_connect, SshAuth};
pub use delete::{SshDeleteCfg, SshDeleteConfigError, SshDeleteOperatorV2};
pub use dest::SshDest;
pub use handler::PinnedClientHandler;
pub use hostkey::{fingerprints_match, HostKeyError, HostKeyPolicy};
pub use source::{manifest_relative_key, SshManifestSource, SshReaderCfg};
pub use tunnel::{open_tunnel, TunnelHandle};
