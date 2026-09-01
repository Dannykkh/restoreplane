// crates/dbbackup-pipeline/src/artifact.rs
//! S05 §1 타입 정의. BackupSource(S03/S04)가 넘겨주는 덤프 출력.
//! Stream/Directory 두 아티팩트가 동일 인코딩 체인을 통과한다(R11).

use futures::future::BoxFuture;
use futures::FutureExt;

/// BackupSource가 반환하는 덤프 산출물. 둘 다 동일 인코딩 체인을 통과한다.
pub enum DumpArtifact {
    /// pg_dump -Fc -f - / mysqldump ... → stdout 파이프.
    Stream(DumpStream),
    /// mydumper / pg_dump -Fd → 디렉토리(TempDir), tar로 말아 체인 투입.
    Directory(DumpDir),
    /// A streamed native dump with the PID captured directly from the spawned child. The service
    /// enriches this PID from the OS and durably fences it before consuming stdout.
    NativeStream(NativeDumpStream),
}

pub struct NativeDumpStream {
    pub stream: DumpStream,
    pub process: NativeChildProcess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeChildProcess {
    process_id: i64,
    process_start_time_ms: i64,
    job_object_key: String,
    executable_path: std::path::PathBuf,
    executable_sha256: [u8; 32],
}

impl NativeChildProcess {
    pub fn supervised(
        process_id: i64,
        process_start_time_ms: i64,
        job_object_key: impl Into<String>,
        executable_path: std::path::PathBuf,
        executable_sha256: [u8; 32],
    ) -> anyhow::Result<Self> {
        let job_object_key = job_object_key.into();
        anyhow::ensure!(
            process_id > 0
                && u32::try_from(process_id).is_ok()
                && process_start_time_ms >= 0
                && job_object_key.starts_with(r"Local\DbBackup.Job.")
                && !job_object_key.starts_with(r"Local\DbBackup.Job.Unowned.")
                && job_object_key.len() <= 256
                && !job_object_key.contains('\0')
                && executable_path.is_absolute(),
            "invalid supervised child identity"
        );
        Ok(Self {
            process_id,
            process_start_time_ms,
            job_object_key,
            executable_path,
            executable_sha256,
        })
    }

    pub const fn process_id(&self) -> i64 {
        self.process_id
    }

    pub const fn process_start_time_ms(&self) -> i64 {
        self.process_start_time_ms
    }

    pub fn job_object_key(&self) -> &str {
        &self.job_object_key
    }

    pub fn executable_path(&self) -> &std::path::Path {
        &self.executable_path
    }

    pub const fn executable_sha256(&self) -> &[u8; 32] {
        &self.executable_sha256
    }
}

impl DumpArtifact {
    pub fn native_process(&self) -> Option<&NativeChildProcess> {
        match self {
            Self::NativeStream(native) => Some(&native.process),
            Self::Stream(_) | Self::Directory(_) => None,
        }
    }

    /// Installs a one-shot durable exit observer without changing provider wait semantics.
    pub fn observe_native_exit<F>(&mut self, observer: F)
    where
        F: FnOnce(&DumpExit) + Send + 'static,
    {
        let Self::NativeStream(native) = self else {
            return;
        };
        let wait = std::mem::replace(
            &mut native.stream.wait,
            async { anyhow::bail!("native dump wait was replaced") }.boxed(),
        );
        native.stream.wait = async move {
            let result = wait.await;
            if let Ok(exit) = result.as_ref() {
                observer(exit);
            }
            result
        }
        .boxed();
    }
}

/// 스트림 아티팩트: AsyncRead + 자식 종료 대기 핸들.
pub struct DumpStream {
    /// 덤프 stdout(tokio AsyncRead). 동기 체인은 SyncIoBridge로 소비한다.
    pub reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    /// 자식 종료를 대기 → exit status + stderr tail. 스트림 EOF와 별개로 확인(R2).
    pub wait: BoxFuture<'static, anyhow::Result<DumpExit>>,
    /// Requests termination of the producer. Cancellation still awaits `wait`, so no child or
    /// encoder task is detached from the commit decision.
    pub abort: Option<Box<dyn FnOnce() + Send + 'static>>,
}

/// 디렉토리 아티팩트: 평문 TempDir(tar 직후 안전 삭제, M2) + 종료 대기.
pub struct DumpDir {
    pub root: tempfile::TempDir,
    pub wait: BoxFuture<'static, anyhow::Result<DumpExit>>,
    pub abort: Option<Box<dyn FnOnce() + Send + 'static>>,
}

#[cfg(test)]
mod native_process_tests {
    use super::NativeChildProcess;

    #[test]
    fn native_process_identity_requires_a_real_supervisor_job() {
        let executable = std::env::current_exe().expect("absolute test executable");
        let identity = NativeChildProcess::supervised(
            42,
            1_234,
            r"Local\DbBackup.Job.11111111-1111-4111-8111-111111111111",
            executable.clone(),
            [0xab; 32],
        )
        .expect("supervised identity");

        assert_eq!(identity.process_id(), 42);
        assert_eq!(identity.process_start_time_ms(), 1_234);
        assert_eq!(
            identity.job_object_key(),
            r"Local\DbBackup.Job.11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(identity.executable_path(), executable);
        assert_eq!(identity.executable_sha256(), &[0xab; 32]);
        assert!(NativeChildProcess::supervised(
            42,
            1_234,
            r"Local\DbBackup.Job.Unowned.11111111-1111-4111-8111-111111111111",
            std::env::current_exe().expect("absolute test executable"),
            [0xab; 32],
        )
        .is_err());
    }
}

/// 자식 프로세스 종료 상태. EOF ≠ 성공이므로 이 값으로 최종 성패를 판정한다(R2).
#[derive(Debug, Clone)]
pub struct DumpExit {
    pub code: Option<i32>,
    pub success: bool,
    pub stderr_tail: String,
}
