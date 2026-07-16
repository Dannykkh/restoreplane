// crates/dbbackup-pipeline/src/dest.rs
//! S06 — Destination trait + DestCaps + LocalDest.
//! LocalDest는 opendal 대신 std::fs로 구현한다(슬라이스 1 편차 — opendal/S3는 다음 슬라이스).
//! multipart-only-commit 규율(완전 성공 시에만 최종 객체 노출)은 `.part` 임시파일 + rename으로 구현.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

/// 목적지 불변성 능력(도메인사전과 1:1). Job이 이 값에 assert(P1-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImmutabilityKind {
    None,
    ObjectLockGovernance,
    ObjectLockCompliance,
    ServerAppendOnly,
}

#[derive(Clone, Copy, Debug)]
pub struct DestCaps {
    pub immutability: ImmutabilityKind,
    pub checksum_required: bool,
    pub resumable: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DestError {
    #[error("목적지 IO 오류({key}): {source}")]
    Io { key: String, source: std::io::Error },
    #[error("writer가 이미 close/abort됨")]
    WriterClosed,
    #[error("불변성 요구 미충족: required={required}, have={have:?}")]
    ImmutabilityUnmet { required: String, have: Vec<String> },
}

/// 업로드 성공 영수증(→ S08 artifacts).
#[derive(Clone, Debug)]
pub struct UploadReceipt {
    pub key: String,
    pub bytes: u64,
}

/// 팬아웃 재시도 판정용 업로드 오류.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("아직 시도되지 않음")]
    NotAttempted,
    #[error("목적지 오류: {0}")]
    Dest(#[from] DestError),
    #[error("스풀 읽기 오류: {0}")]
    Io(#[from] std::io::Error),
    #[error("업로드 태스크 패닉: {0}")]
    TaskPanic(String),
}

impl UploadError {
    /// v1: IO 오류만 재시도 가능으로 취급(네트워크 목적지가 붙으면 세분화).
    pub fn is_retryable(&self) -> bool {
        matches!(self, UploadError::Io(_))
    }
}

/// Job이 요구하는 최소 불변성. capabilities()가 이를 만족하지 못하면 요란히 실패(R12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequiredImmutability {
    None,
    AppendOnlyOrLock,
    ObjectLock,
    ComplianceOnly,
}

impl RequiredImmutability {
    pub fn is_satisfied_by(self, caps: DestCaps) -> bool {
        use ImmutabilityKind::*;
        match self {
            RequiredImmutability::None => true,
            RequiredImmutability::AppendOnlyOrLock => matches!(
                caps.immutability,
                ServerAppendOnly | ObjectLockGovernance | ObjectLockCompliance
            ),
            RequiredImmutability::ObjectLock => {
                matches!(caps.immutability, ObjectLockGovernance | ObjectLockCompliance)
            }
            RequiredImmutability::ComplianceOnly => {
                matches!(caps.immutability, ObjectLockCompliance)
            }
        }
    }
}

/// 팬아웃 진입 전 호출. Local-only가 P1을 만족한다고 오인하는 것을 막는다(P1-2/R12).
pub fn assert_job_immutability(
    required: RequiredImmutability,
    dests: &[std::sync::Arc<dyn Destination>],
) -> Result<(), DestError> {
    let ok = dests
        .iter()
        .filter(|d| required.is_satisfied_by(d.capabilities()))
        .count();
    if ok == 0 {
        return Err(DestError::ImmutabilityUnmet {
            required: format!("{required:?}"),
            have: dests
                .iter()
                .map(|d| format!("{}={:?}", d.name(), d.capabilities().immutability))
                .collect(),
        });
    }
    Ok(())
}

#[async_trait]
pub trait Destination: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> DestCaps;
    /// 멀티파트/스트리밍 writer. **commit()에서만 최종 객체가 보인다**(multipart-only-commit, R6).
    async fn writer(&self, key: &str) -> Result<DestWriter, DestError>;
}

/// 로컬 파일 목적지의 writer. `.part` 임시파일에 쓰고 commit 시 rename → 부분 객체 미노출.
pub struct DestWriter {
    final_path: PathBuf,
    part_path: PathBuf,
    file: Option<std::fs::File>,
    bytes: u64,
    committed: bool,
}

impl DestWriter {
    pub async fn write_all(&mut self, buf: &[u8]) -> Result<(), DestError> {
        use std::io::Write;
        let f = self.file.as_mut().ok_or(DestError::WriterClosed)?;
        f.write_all(buf).map_err(|e| DestError::Io {
            key: self.final_path.display().to_string(),
            source: e,
        })?;
        self.bytes += buf.len() as u64;
        Ok(())
    }

    /// 완전 성공 경로에서만 호출 → rename(원자적 노출). 이후에야 객체가 보인다.
    pub async fn commit(mut self) -> Result<UploadReceipt, DestError> {
        let mut f = self.file.take().ok_or(DestError::WriterClosed)?;
        {
            use std::io::Write;
            f.flush().map_err(|e| self.io_err(e))?;
        }
        f.sync_all().map_err(|e| self.io_err(e))?;
        drop(f);
        std::fs::rename(&self.part_path, &self.final_path).map_err(|e| self.io_err(e))?;
        self.committed = true;
        Ok(UploadReceipt {
            key: self.final_path.display().to_string(),
            bytes: self.bytes,
        })
    }

    /// 실패 경로: 부분 객체(.part)가 최종 위치에 보이지 않게 제거.
    pub async fn abort(mut self) {
        if let Some(f) = self.file.take() {
            drop(f);
        }
        let _ = std::fs::remove_file(&self.part_path);
    }

    fn io_err(&self, e: std::io::Error) -> DestError {
        DestError::Io {
            key: self.final_path.display().to_string(),
            source: e,
        }
    }
}

impl Drop for DestWriter {
    fn drop(&mut self) {
        if self.file.is_some() && !self.committed {
            // commit 없이 drop → 부분 파일이 남을 수 있으므로 정리 시도(+경고).
            let _ = std::fs::remove_file(&self.part_path);
            tracing::warn!(key = %self.final_path.display(), "DestWriter가 commit 없이 drop");
        }
    }
}

/// 로컬 디렉토리 목적지(= 스풀 사본). caps.immutability = None(랜섬웨어 방어 불충분, R3).
pub struct LocalDest {
    name: String,
    root: PathBuf,
}

impl LocalDest {
    pub fn new(name: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self { name: name.into(), root: root.into() }
    }
}

#[async_trait]
impl Destination for LocalDest {
    fn name(&self) -> &str {
        &self.name
    }
    fn capabilities(&self) -> DestCaps {
        DestCaps {
            immutability: ImmutabilityKind::None,
            checksum_required: false,
            resumable: true,
        }
    }
    async fn writer(&self, key: &str) -> Result<DestWriter, DestError> {
        let final_path = self.root.join(key);
        if let Some(parent) = final_path.parent() {
            // root가 실제로는 파일이면(테스트: unwritable dir) 여기서 실패한다.
            std::fs::create_dir_all(parent).map_err(|e| DestError::Io {
                key: key.to_string(),
                source: e,
            })?;
        }
        let part_path = final_path.with_extension("part");
        let file = std::fs::File::create(&part_path).map_err(|e| DestError::Io {
            key: key.to_string(),
            source: e,
        })?;
        Ok(DestWriter {
            final_path,
            part_path,
            file: Some(file),
            bytes: 0,
            committed: false,
        })
    }
}

/// S06 §8 — writer 위의 얇은 헬퍼. 스풀 파일을 목적지 key로 스트리밍, 완전 성공 시에만 commit.
pub async fn upload_resumable(
    dest: &dyn Destination,
    spool: &Path,
    key: &str,
) -> Result<UploadReceipt, UploadError> {
    let mut w = dest.writer(key).await?;
    let mut f = match tokio::fs::File::open(spool).await {
        Ok(f) => f,
        Err(e) => {
            w.abort().await;
            return Err(UploadError::from(e));
        }
    };
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = match tokio::io::AsyncReadExt::read(&mut f, &mut buf).await {
            Ok(n) => n,
            Err(e) => {
                w.abort().await;
                return Err(UploadError::from(e));
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = w.write_all(&buf[..n]).await {
            w.abort().await; // 실패 → 부분 객체 안 보이게(R6)
            return Err(UploadError::from(e));
        }
    }
    w.commit().await.map_err(UploadError::from)
}
