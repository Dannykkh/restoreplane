// crates/dbbackup-pipeline/src/artifact.rs
//! S05 §1 타입 정의. BackupSource(S03/S04)가 넘겨주는 덤프 출력.
//! Stream/Directory 두 아티팩트가 동일 인코딩 체인을 통과한다(R11).

use futures::future::BoxFuture;

/// BackupSource가 반환하는 덤프 산출물. 둘 다 동일 인코딩 체인을 통과한다.
pub enum DumpArtifact {
    /// pg_dump -Fc -f - / mysqldump ... → stdout 파이프.
    Stream(DumpStream),
    /// mydumper / pg_dump -Fd → 디렉토리(TempDir), tar로 말아 체인 투입.
    Directory(DumpDir),
}

/// 스트림 아티팩트: AsyncRead + 자식 종료 대기 핸들.
pub struct DumpStream {
    /// 덤프 stdout(tokio AsyncRead). 동기 체인은 SyncIoBridge로 소비한다.
    pub reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    /// 자식 종료를 대기 → exit status + stderr tail. 스트림 EOF와 별개로 확인(R2).
    pub wait: BoxFuture<'static, anyhow::Result<DumpExit>>,
}

/// 디렉토리 아티팩트: 평문 TempDir(tar 직후 안전 삭제, M2) + 종료 대기.
pub struct DumpDir {
    pub root: tempfile::TempDir,
    pub wait: BoxFuture<'static, anyhow::Result<DumpExit>>,
}

/// 자식 프로세스 종료 상태. EOF ≠ 성공이므로 이 값으로 최종 성패를 판정한다(R2).
#[derive(Debug, Clone)]
pub struct DumpExit {
    pub code: Option<i32>,
    pub success: bool,
    pub stderr_tail: String,
}
