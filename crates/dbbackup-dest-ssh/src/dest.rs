// crates/dbbackup-dest-ssh/src/dest.rs
//! S07 §5-§7 — SshDest: S06 Destination 구현(SFTP rename-commit).
//!
//! SFTP엔 진짜 멀티파트가 없으므로 S06 multipart-only-commit을 **임시키 write→flush→rename**으로
//! 구현한다(R6). 실패 시 임시키 삭제 → 최종 key엔 절단 객체가 절대 나타나지 않는다.
//!
//! 불변성: SshDest는 Object Lock을 강제 못 한다. caps.immutability는 None(일반 SFTP) 또는
//! ServerAppendOnly(서버측 append-only가 **외부 보장**될 때만). 어느 쪽도 Governance/Compliance가
//! 아니므로 Object Lock을 요구하는 Job은 이 목적지에서 반드시 실패한다(S06 assert_job_immutability, R9).

use async_trait::async_trait;
use dbbackup_pipeline::{
    DestCaps, DestError, DestWriter, Destination, ImmutabilityKind, UploadReceipt, WriterBackend,
};
use russh_sftp::client::SftpSession;
use tokio::io::AsyncWriteExt;

use crate::connect::{ssh_connect, SshAuth};
use crate::hostkey::HostKeyPolicy;

pub struct SshDest {
    name: String,
    host: String,
    port: u16,
    user: String,
    policy: HostKeyPolicy,
    auth: SshAuth,
    remote_dir: String,
    /// 서버측 append-only가 외부 보장될 때만 true(R8). 자가주장일 뿐 SshDest가 검증할 수는 없다.
    append_only: bool,
}

impl SshDest {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        policy: HostKeyPolicy,
        auth: SshAuth,
        remote_dir: impl Into<String>,
        append_only: bool,
    ) -> Self {
        Self {
            name: name.into(),
            host: host.into(),
            port,
            user: user.into(),
            policy,
            auth,
            remote_dir: remote_dir.into(),
            append_only,
        }
    }

    /// 매 업로드마다 새 SSH 연결 → session 채널 → sftp 서브시스템 → SftpSession.
    /// Handle을 함께 반환해 업로드 동안 연결이 살아있게 한다(drop 시 세션 종료).
    async fn open_sftp(
        &self,
    ) -> anyhow::Result<(
        russh::client::Handle<crate::handler::PinnedClientHandler>,
        SftpSession,
    )> {
        let handle = ssh_connect(
            &self.host,
            self.port,
            &self.user,
            self.policy.clone(),
            self.auth.clone(),
        )
        .await?;
        let channel = handle.channel_open_session().await?;
        channel.request_subsystem(true, "sftp").await?;
        let sftp = SftpSession::new(channel.into_stream()).await?;
        Ok((handle, sftp))
    }

    /// 연결/인증/host-key 핀/원격 디렉터리 write 권한 확인. WORM 자가주장은 검증 불가이므로
    /// append_only=true여도 "외부 보장 필요" 경고를 남긴다(오인 방지, R9).
    pub async fn self_test(&self) -> anyhow::Result<()> {
        let (_handle, sftp) = self.open_sftp().await?;
        let probe = format!(
            "{}/.selftest-{}",
            self.remote_dir.trim_end_matches('/'),
            uuid::Uuid::new_v4()
        );
        let mut f = sftp.create(&probe).await?;
        f.write_all(b"ok").await?;
        f.flush().await?;
        f.shutdown().await.ok();
        let _ = sftp.remove_file(&probe).await; // probe 정리(writer가 지울 수 있음 — WORM 아님)
        if self.append_only {
            tracing::warn!(dest = %self.name, "append_only=true는 자가주장 — 서버측 append-only가 외부 보장돼야 함(R9)");
        }
        Ok(())
    }
}

#[async_trait]
impl Destination for SshDest {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> DestCaps {
        DestCaps {
            immutability: if self.append_only {
                ImmutabilityKind::ServerAppendOnly
            } else {
                ImmutabilityKind::None
            },
            checksum_required: false,
            resumable: true,
        }
    }

    /// SFTP 업로드. 임시키에 write 후 성공 시 rename으로 커밋 원자성 확보(R6).
    async fn writer(&self, key: &str) -> Result<DestWriter, DestError> {
        let (handle, sftp) = self
            .open_sftp()
            .await
            .map_err(|e| DestError::backend(key, e))?;
        let final_path = format!("{}/{}", self.remote_dir.trim_end_matches('/'), key);
        let tmp_path = format!("{final_path}.part-{}", uuid::Uuid::new_v4());

        ensure_remote_dir(&sftp, &self.remote_dir)
            .await
            .map_err(|e| DestError::backend(key, e))?;
        let file = sftp
            .create(&tmp_path)
            .await
            .map_err(|e| DestError::backend(&tmp_path, e))?;

        Ok(DestWriter::from_backend(Box::new(SftpBackend {
            _handle: handle,
            sftp,
            file: Some(file),
            tmp_path,
            final_path,
            bytes: 0,
        })))
    }
}

/// S06 DestWriter가 감싸는 SFTP 업로드 상태. commit=rename, abort=임시파일 삭제(R6).
struct SftpBackend {
    _handle: russh::client::Handle<crate::handler::PinnedClientHandler>,
    sftp: SftpSession,
    file: Option<russh_sftp::client::fs::File>,
    tmp_path: String,
    final_path: String,
    bytes: u64,
}

#[async_trait]
impl WriterBackend for SftpBackend {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), DestError> {
        let f = self.file.as_mut().ok_or(DestError::WriterClosed)?;
        f.write_all(buf)
            .await
            .map_err(|e| DestError::backend(&self.tmp_path, e))?;
        self.bytes += buf.len() as u64;
        Ok(())
    }

    /// 완전 성공 경로에서만: flush → 닫기 → 임시→최종 rename(= 유일한 커밋 지점).
    async fn commit(mut self: Box<Self>) -> Result<UploadReceipt, DestError> {
        let mut f = self.file.take().ok_or(DestError::WriterClosed)?;
        f.flush()
            .await
            .map_err(|e| DestError::backend(&self.tmp_path, e))?;
        f.shutdown().await.ok(); // 핸들 닫기(서버가 파일 확정)
        drop(f);
        self.sftp
            .rename(&self.tmp_path, &self.final_path)
            .await
            .map_err(|e| DestError::backend(&self.final_path, e))?;
        Ok(UploadReceipt {
            key: self.final_path.clone(),
            bytes: self.bytes,
            version: None,
        })
    }

    /// 실패 경로: 임시 파일 삭제 → 최종 key엔 아무것도 나타나지 않음.
    async fn abort(mut self: Box<Self>) {
        if let Some(mut f) = self.file.take() {
            let _ = f.shutdown().await;
        }
        if let Err(e) = self.sftp.remove_file(&self.tmp_path).await {
            tracing::warn!(path = %self.tmp_path, "임시 SFTP 파일 삭제 실패(수동 정리 필요): {e}");
        }
    }

    fn key_for_log(&self) -> String {
        self.final_path.clone()
    }
}

/// 원격 디렉터리 존재 보장(mkdir -p 동등). 이미 있으면 무시.
async fn ensure_remote_dir(sftp: &SftpSession, dir: &str) -> anyhow::Result<()> {
    let dir = dir.trim_end_matches('/');
    if dir.is_empty() {
        return Ok(());
    }
    // 상위부터 순차 생성(존재 시 오류는 무시).
    let mut path = String::new();
    for seg in dir.split('/') {
        if seg.is_empty() {
            path.push('/');
            continue;
        }
        if !path.is_empty() && !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(seg);
        let _ = sftp.create_dir(&path).await; // 이미 존재하면 Err → 무시
    }
    Ok(())
}
