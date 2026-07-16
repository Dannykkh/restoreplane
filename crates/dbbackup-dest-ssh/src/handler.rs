// crates/dbbackup-dest-ssh/src/handler.rs
//! S07 §2 — client::Handler 구현. accept-any 금지(H6).
//!
//! russh 0.62는 저수준이라 Handler를 직접 구현해 host key 수용 여부를 스스로 결정해야 한다. 기본값에
//! 의존하거나 무조건 Ok(true)를 반환하면 accept-any = MITM. 유일한 보안 책임 = host key 핀 검증.
//!
//! [편차/실측] russh 0.62 Handler는 async-trait이 아니라 **native async fn in trait**(RPITIT)이다.
//! 따라서 이 impl에는 `#[async_trait]`를 붙이지 않는다(붙이면 시그니처 불일치로 컴파일 실패).

use russh::client::{self, Session};
use russh::keys::ssh_key::PublicKey;
use russh::ChannelId;

use crate::hostkey::HostKeyPolicy;

/// 터널과 SshDest가 공유하는 클라이언트 핸들러.
pub struct PinnedClientHandler {
    policy: HostKeyPolicy,
}

impl PinnedClientHandler {
    pub fn new(policy: HostKeyPolicy) -> Self {
        Self { policy }
    }
}

impl client::Handler for PinnedClientHandler {
    type Error = russh::Error;

    /// 핵심: 이 콜백이 host key 수용 여부를 결정한다. 지문 핀 검증 실패 시 절대 true를 반환하지 않는다.
    /// `Ok(false)`면 russh가 연결을 거부한다(R1/R11).
    async fn check_server_key(&mut self, server_public_key: &PublicKey) -> Result<bool, Self::Error> {
        match self.policy.verify(server_public_key) {
            Ok(()) => Ok(true),
            Err(e) => {
                tracing::error!("{e}"); // 요란히 로깅 후 거부(시크릿 없음)
                Ok(false)
            }
        }
    }

    /// 데이터는 SFTP 서브시스템/터널 스트림으로 처리하므로 여기서는 무시(기본 동작 명시).
    async fn data(&mut self, _channel: ChannelId, _data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        Ok(())
    }
}
