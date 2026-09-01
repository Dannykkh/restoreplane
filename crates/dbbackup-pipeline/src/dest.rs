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
    /// 원격 목적지(S3/SSH) 백엔드가 매핑해 넘기는 일반 오류. key는 로그/영수증용(시크릿 아님).
    #[error("목적지 백엔드 오류({key}): {msg}")]
    Backend { key: String, msg: String },
    #[error("목적지 객체 키가 안전하지 않음: {key}")]
    InvalidKey { key: String },
    #[error("목적지 읽기 상한 초과({key}): max_bytes={max_bytes}")]
    ReadLimitExceeded { key: String, max_bytes: u64 },
}

impl DestError {
    /// 외부 목적지 크레이트(S3/SSH)가 자신의 오류를 DestError로 매핑할 때 쓰는 헬퍼.
    pub fn backend(key: impl Into<String>, msg: impl std::fmt::Display) -> Self {
        DestError::Backend {
            key: key.into(),
            msg: msg.to_string(),
        }
    }

    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Io { source, .. } => matches!(
                source.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::NotConnected
            ),
            Self::Backend { .. } => true,
            Self::WriterClosed
            | Self::ImmutabilityUnmet { .. }
            | Self::InvalidKey { .. }
            | Self::ReadLimitExceeded { .. } => false,
        }
    }
}

/// 업로드 성공 영수증(→ S08 artifacts).
#[derive(Clone, Debug)]
pub struct UploadReceipt {
    pub key: String,
    pub bytes: u64,
    pub version: Option<String>,
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
        match self {
            Self::Dest(error) => error.is_retryable(),
            Self::Io(error) => matches!(
                error.kind(),
                std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::NotConnected
            ),
            Self::NotAttempted | Self::TaskPanic(_) => false,
        }
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
                matches!(
                    caps.immutability,
                    ObjectLockGovernance | ObjectLockCompliance
                )
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderObjectProbeV2 {
    pub bytes: u64,
    pub blake3: String,
    pub version: Option<String>,
}

impl ReaderObjectProbeV2 {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self {
            bytes: bytes.len() as u64,
            blake3: blake3::hash(bytes).to_hex().to_string(),
            version: None,
        }
    }
}

/// Metadata-only observation for large objects whose expected digest is
/// authenticated by an atomic signed commit marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReaderObjectMetadataV2 {
    pub bytes: u64,
    pub version: Option<String>,
}

#[async_trait]
pub trait DestinationReaderV2: Send + Sync {
    async fn probe_object(&self, key: &str) -> Result<ReaderObjectProbeV2, DestError>;
    async fn read_bytes_bounded(&self, key: &str, max_bytes: u64) -> Result<Vec<u8>, DestError>;

    /// Compatibility implementations may hash the object, while production
    /// readers override this with a backend metadata request.
    async fn stat_object(&self, key: &str) -> Result<ReaderObjectMetadataV2, DestError> {
        let probe = self.probe_object(key).await?;
        Ok(ReaderObjectMetadataV2 {
            bytes: probe.bytes,
            version: probe.version,
        })
    }

    async fn copy_object_to(
        &self,
        key: &str,
        writer: &mut DestWriter,
        max_bytes: u64,
    ) -> Result<ReaderObjectProbeV2, DestError> {
        let bytes = self.read_bytes_bounded(key, max_bytes).await?;
        writer.write_all(&bytes).await?;
        Ok(ReaderObjectProbeV2::from_bytes(&bytes))
    }
}

/// multipart-only-commit 규율의 백엔드 경계.
///
/// LocalDest는 `.part` 임시파일+rename, S3Dest는 opendal 멀티파트 close/abort, SshDest는 SFTP
/// 임시키+rename으로 각각 이 규율을 구현한다. 백엔드는 별도 크레이트(dbbackup-dest-s3/-ssh)에
/// 살 수 있으므로(순환의존 방지) trait object로 추상화한다. **commit()만이 유일한 커밋 지점**이고,
/// 실패/미완료 경로는 abort() 또는 Drop이 부분 객체를 노출 없이 정리한다.
#[async_trait]
pub trait WriterBackend: Send {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), DestError>;
    /// 완전 성공 경로에서만 호출 → 원자적 커밋(rename/CompleteMultipartUpload). 이후에야 객체가 보인다.
    async fn commit(self: Box<Self>) -> Result<UploadReceipt, DestError>;
    /// 실패 경로: 부분 객체가 최종 위치에 보이지 않게 정리.
    async fn abort(self: Box<Self>);
    /// commit 없이 drop될 때 경고 로그에 쓸 key(시크릿 아님).
    fn key_for_log(&self) -> String;
}

/// 목적지 writer. 내부적으로 [`WriterBackend`] trait object를 감싸 Local/S3/SSH를 동형으로 다룬다.
/// 공개 API(`write_all`/`commit`/`abort`)는 백엔드 종류와 무관하게 동일 — S05 팬아웃이 바뀌지 않는다.
pub struct DestWriter {
    backend: Option<Box<dyn WriterBackend>>,
    committed: bool,
}

impl DestWriter {
    /// 외부/내부 목적지가 자신의 백엔드로 DestWriter를 구성한다.
    pub fn from_backend(backend: Box<dyn WriterBackend>) -> Self {
        Self {
            backend: Some(backend),
            committed: false,
        }
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> Result<(), DestError> {
        let b = self.backend.as_mut().ok_or(DestError::WriterClosed)?;
        b.write_all(buf).await
    }

    /// 완전 성공 경로에서만 호출 → 백엔드의 원자적 커밋. 이후에야 객체가 보인다.
    pub async fn commit(mut self) -> Result<UploadReceipt, DestError> {
        let b = self.backend.take().ok_or(DestError::WriterClosed)?;
        let receipt = b.commit().await?;
        self.committed = true;
        Ok(receipt)
    }

    /// 실패 경로: 백엔드가 부분 객체를 노출 없이 정리.
    pub async fn abort(mut self) {
        if let Some(b) = self.backend.take() {
            b.abort().await;
        }
    }
}

impl Drop for DestWriter {
    fn drop(&mut self) {
        if let Some(b) = self.backend.take() {
            if !self.committed {
                // commit 없이 drop → 백엔드 Drop이 부분 객체를 정리(+경고). async abort는 Drop에서 불가.
                tracing::warn!(key = %b.key_for_log(), "DestWriter가 commit 없이 drop — 백엔드가 부분 객체 정리");
            }
        }
    }
}

/// LocalDest의 WriterBackend. `.part` 임시파일에 쓰고 commit 시 rename → 부분 객체 미노출.
struct LocalBackend {
    final_path: PathBuf,
    part_path: PathBuf,
    file: Option<std::fs::File>,
    bytes: u64,
    committed: bool,
}

impl LocalBackend {
    fn io_err(&self, e: std::io::Error) -> DestError {
        DestError::Io {
            key: self.final_path.display().to_string(),
            source: e,
        }
    }
}

#[async_trait]
impl WriterBackend for LocalBackend {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), DestError> {
        use std::io::Write;
        let f = self.file.as_mut().ok_or(DestError::WriterClosed)?;
        f.write_all(buf).map_err(|e| DestError::Io {
            key: self.final_path.display().to_string(),
            source: e,
        })?;
        self.bytes += buf.len() as u64;
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<UploadReceipt, DestError> {
        let mut f = self.file.take().ok_or(DestError::WriterClosed)?;
        let flush_result = {
            use std::io::Write;
            f.flush().and_then(|_| f.sync_all())
        };
        drop(f);
        if let Err(error) = flush_result {
            let _ = std::fs::remove_file(&self.part_path);
            self.committed = true;
            return Err(self.io_err(error));
        }
        if let Err(error) = std::fs::rename(&self.part_path, &self.final_path) {
            let _ = std::fs::remove_file(&self.part_path);
            self.committed = true;
            return Err(self.io_err(error));
        }
        self.committed = true;
        Ok(UploadReceipt {
            key: self.final_path.display().to_string(),
            bytes: self.bytes,
            version: None,
        })
    }

    async fn abort(mut self: Box<Self>) {
        if let Some(f) = self.file.take() {
            drop(f);
        }
        let _ = std::fs::remove_file(&self.part_path);
        self.committed = true; // Drop 중복 정리/경고 방지
    }

    fn key_for_log(&self) -> String {
        self.final_path.display().to_string()
    }
}

impl Drop for LocalBackend {
    fn drop(&mut self) {
        if self.file.is_some() && !self.committed {
            // commit/abort 없이 drop → 부분 파일이 남을 수 있으므로 정리 시도.
            let _ = std::fs::remove_file(&self.part_path);
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
        Self {
            name: name.into(),
            root: root.into(),
        }
    }

    fn object_path(&self, key: &str) -> Result<PathBuf, DestError> {
        let relative = Path::new(key);
        if key.is_empty()
            || relative.is_absolute()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(DestError::InvalidKey {
                key: key.to_owned(),
            });
        }
        Ok(self.root.join(relative))
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
        let final_path = self.object_path(key)?;
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
        Ok(DestWriter::from_backend(Box::new(LocalBackend {
            final_path,
            part_path,
            file: Some(file),
            bytes: 0,
            committed: false,
        })))
    }
}

#[async_trait]
impl DestinationReaderV2 for LocalDest {
    async fn probe_object(&self, key: &str) -> Result<ReaderObjectProbeV2, DestError> {
        use tokio::io::AsyncReadExt;

        let path = self.object_path(key)?;
        let mut file = tokio::fs::File::open(&path)
            .await
            .map_err(|source| DestError::Io {
                key: key.to_owned(),
                source,
            })?;
        let mut hasher = blake3::Hasher::new();
        let mut total = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|source| DestError::Io {
                    key: key.to_owned(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .ok_or_else(|| DestError::ReadLimitExceeded {
                    key: key.to_owned(),
                    max_bytes: u64::MAX,
                })?;
            hasher.update(&buffer[..read]);
        }
        Ok(ReaderObjectProbeV2 {
            bytes: total,
            blake3: hasher.finalize().to_hex().to_string(),
            version: None,
        })
    }

    async fn read_bytes_bounded(&self, key: &str, max_bytes: u64) -> Result<Vec<u8>, DestError> {
        let path = self.object_path(key)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|source| DestError::Io {
                key: key.to_owned(),
                source,
            })?;
        if metadata.len() > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|source| DestError::Io {
                key: key.to_owned(),
                source,
            })?;
        if bytes.len() as u64 > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        Ok(bytes)
    }

    async fn stat_object(&self, key: &str) -> Result<ReaderObjectMetadataV2, DestError> {
        let path = self.object_path(key)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|source| DestError::Io {
                key: key.to_owned(),
                source,
            })?;
        Ok(ReaderObjectMetadataV2 {
            bytes: metadata.len(),
            version: None,
        })
    }

    async fn copy_object_to(
        &self,
        key: &str,
        writer: &mut DestWriter,
        max_bytes: u64,
    ) -> Result<ReaderObjectProbeV2, DestError> {
        use tokio::io::AsyncReadExt;

        let path = self.object_path(key)?;
        let metadata = tokio::fs::metadata(&path)
            .await
            .map_err(|source| DestError::Io {
                key: key.to_owned(),
                source,
            })?;
        if metadata.len() > max_bytes {
            return Err(DestError::ReadLimitExceeded {
                key: key.to_owned(),
                max_bytes,
            });
        }
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|source| DestError::Io {
                key: key.to_owned(),
                source,
            })?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut hasher = blake3::Hasher::new();
        let mut total = 0_u64;
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|source| DestError::Io {
                    key: key.to_owned(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(u64::try_from(read).unwrap_or(u64::MAX))
                .ok_or_else(|| DestError::ReadLimitExceeded {
                    key: key.to_owned(),
                    max_bytes,
                })?;
            if total > max_bytes {
                return Err(DestError::ReadLimitExceeded {
                    key: key.to_owned(),
                    max_bytes,
                });
            }
            writer.write_all(&buffer[..read]).await?;
            hasher.update(&buffer[..read]);
        }
        Ok(ReaderObjectProbeV2 {
            bytes: total,
            blake3: hasher.finalize().to_hex().to_string(),
            version: None,
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
