// crates/dbbackup-dest-ssh/src/tunnel.rs
//! S07 §4 — Tunnel: 바스티온 direct-tcpip → 로컬 TcpListener 브리지(R3/R4).
//!
//! 바스티온(점프 호스트) 뒤 DB에 닿기 위한 경유 채널이다. russh로 바스티온에 접속한 뒤 매 로컬 연결마다
//! direct-tcpip 채널을 열어 DB(host:port)로 연결하고, 로컬 소켓과 양방향 복사한다. pg_dump/mysqldump는
//! `127.0.0.1:<local_port>`로 접속하면 실제 트래픽이 바스티온을 통해 DB로 흐른다.
//! 이는 Destination이 아니라 BackupSource가 DB에 닿는 경로를 여는 인프라다.

use std::sync::Arc;

use tokio::io::copy_bidirectional;
use tokio::net::TcpListener;

use crate::handler::PinnedClientHandler;

pub struct TunnelHandle {
    pub local_addr: std::net::SocketAddr,
    _accept_task: tokio::task::JoinHandle<()>,
    _keepalive: Arc<russh::client::Handle<PinnedClientHandler>>, // drop 시 세션 종료
}

/// 바스티온에 붙은 handle로 direct-tcpip 채널을 열어 DB를 로컬 소켓으로 노출한다.
/// BackupSource는 반환된 `local_addr`를 DB host:port로 사용한다. dump보다 **먼저** 호출해야 한다(S05 시퀀스).
pub async fn open_tunnel(
    handle: russh::client::Handle<PinnedClientHandler>,
    db_host: String,
    db_port: u16,
    local_bind: &str, // "127.0.0.1:0" = OS가 포트 할당
) -> anyhow::Result<TunnelHandle> {
    let listener = TcpListener::bind(local_bind).await?;
    let local_addr = listener.local_addr()?;
    let handle = Arc::new(handle);
    let h = handle.clone();
    let target = format!("{db_host}:{db_port}");

    let accept_task = tokio::spawn(async move {
        loop {
            let (mut local_sock, peer) = match listener.accept().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("tunnel accept 실패: {e}");
                    break;
                }
            };
            let h2 = h.clone();
            let (dbh, dbp) = (db_host.clone(), db_port);
            tokio::spawn(async move {
                // 매 로컬 연결마다 새 direct-tcpip 채널. 인자: (target_host, target_port, orig_host, orig_port).
                let channel = match h2
                    .channel_open_direct_tcpip(
                        dbh,
                        dbp as u32,
                        peer.ip().to_string(),
                        peer.port() as u32,
                    )
                    .await
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("direct-tcpip open 실패: {e}");
                        return;
                    }
                };
                let mut chan_stream = channel.into_stream(); // AsyncRead + AsyncWrite
                if let Err(e) = copy_bidirectional(&mut local_sock, &mut chan_stream).await {
                    tracing::debug!("tunnel copy 종료: {e}");
                }
            });
        }
    });

    tracing::info!(local = %local_addr, %target, "터널 개통");
    Ok(TunnelHandle {
        local_addr,
        _accept_task: accept_task,
        _keepalive: handle,
    })
}
