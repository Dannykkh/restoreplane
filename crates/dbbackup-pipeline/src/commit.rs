// crates/dbbackup-pipeline/src/commit.rs
//! S05 §5~6 — Commit Protocol. 절단된 덤프가 "유효한 백업"으로 커밋되는 것을 막는다.
//! 크리티컬 구간(복사~커밋)에 `?` 조기반환 금지, 모든 실패를 단일 정리 경로(스풀 삭제)로 수렴(P0-3/R4).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dbbackup_secrets::Dek;

use crate::artifact::{DumpArtifact, DumpDir, DumpStream};
use crate::chain::EncryptChain;

/// 파이프라인 설정(S05 §7).
#[derive(Clone, Debug)]
pub struct PipelineCfg {
    pub zstd_level: i32,
    pub max_parallel_dumps: usize,
    pub max_parallel_destinations: usize,
    pub min_free_disk: u64,
    pub upload_retries: u32,
    pub fatal_patterns: Vec<String>,
}

impl Default for PipelineCfg {
    fn default() -> Self {
        Self {
            zstd_level: 3,
            max_parallel_dumps: 1,
            max_parallel_destinations: 4,
            min_free_disk: 0,
            upload_retries: 3,
            fatal_patterns: vec![
                "server version mismatch".to_string(),
                "aborting because of server version mismatch".to_string(),
            ],
        }
    }
}

/// 커밋된 스풀의 진실 소스 필드(→ S08 Manifest).
#[derive(Clone, Debug)]
pub struct SpoolCommit {
    pub spool_path: std::path::PathBuf,
    pub plaintext_blake3: [u8; 32],
    pub ciphertext_blake3: [u8; 32],
    pub plaintext_bytes: u64,
    pub ciphertext_bytes: u64,
    pub stderr_tail: String,
}

#[derive(Clone, Default)]
pub struct PipelineCancellationToken {
    inner: Arc<PipelineCancellationState>,
}

#[derive(Default)]
struct PipelineCancellationState {
    cancelled: AtomicBool,
    notify: tokio::sync::Notify,
}

impl PipelineCancellationToken {
    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::AcqRel) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.inner.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

/// 치명 stderr 패턴 검사(exit=0이어도 이 패턴이 보이면 FAIL, R2/T4).
pub fn is_fatal_stderr(stderr_tail: &str, fatal_patterns: &[String]) -> bool {
    let hay = stderr_tail.to_ascii_lowercase();
    fatal_patterns
        .iter()
        .any(|p| hay.contains(&p.to_ascii_lowercase()))
}

/// M2: 평문 TempDir 안전 삭제(tar가 체인에 다 들어간 뒤). v1은 표준 삭제.
fn secure_wipe_tempdir(dir: tempfile::TempDir) {
    // TempDir::close는 실패해도 drop이 재시도한다. 잔여 시간 최소화가 목적.
    let _ = dir.close();
}

/// Stream 아티팩트를 인코딩·커밋. spawn_blocking 안에서 호출(동기 체인).
/// ===== CRITICAL REGION: 스트림 복사 → 커밋. 이 구간엔 `?` 금지(P0-3). =====
fn drive_stream_blocking(
    reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    spool_path: std::path::PathBuf,
    dek: Dek,
    zstd_level: i32,
) -> anyhow::Result<([u8; 32], [u8; 32], u64, u64)> {
    use tokio_util::io::SyncIoBridge;
    let mut src = SyncIoBridge::new(reader); // AsyncRead → std::io::Read (런타임 핸들 필요)
    let mut chain = EncryptChain::new(&spool_path, &dek, zstd_level)?;

    let copied: anyhow::Result<u64> =
        std::io::copy(&mut src, chain.writer()).map_err(anyhow::Error::from);

    let outcome = match copied {
        Ok(_) => chain.commit(), // finish inner→outer(R3)
        Err(e) => {
            chain.abort_hint(); // 복사 실패 → commit 자체를 시도 안 함
            Err(e)
        }
    };
    // ===== CRITICAL REGION END =====

    match outcome {
        Ok(v) => Ok(v),
        Err(e) => {
            // 절단/실패 스풀이 "유효한 백업"으로 남지 않게 삭제(핵심).
            let _ = std::fs::remove_file(&spool_path);
            Err(e)
        }
    }
}

/// Stream 파이프라인. 인코딩(blocking)과 자식 종료 대기를 함께 몰고, R2 게이트를 적용한다.
pub async fn run_stream_pipeline(
    stream: DumpStream,
    spool_path: std::path::PathBuf,
    dek: Dek,
    cfg: &PipelineCfg,
) -> anyhow::Result<SpoolCommit> {
    run_stream_pipeline_with_cancellation(
        stream,
        spool_path,
        dek,
        cfg,
        PipelineCancellationToken::default(),
    )
    .await
}

pub async fn run_stream_pipeline_with_cancellation(
    stream: DumpStream,
    spool_path: std::path::PathBuf,
    dek: Dek,
    cfg: &PipelineCfg,
    cancellation: PipelineCancellationToken,
) -> anyhow::Result<SpoolCommit> {
    let sp = spool_path.clone();
    let level = cfg.zstd_level;
    let DumpStream {
        reader,
        wait,
        mut abort,
    } = stream;

    let encode = tokio::task::spawn_blocking(move || drive_stream_blocking(reader, sp, dek, level));
    let combined = async { tokio::join!(encode, wait) };
    tokio::pin!(combined);
    let ((encode_res, exit), cancelled) = if cancellation.is_cancelled() {
        request_abort(&mut abort);
        (combined.await, true)
    } else {
        tokio::select! {
            result = &mut combined => (result, false),
            () = cancellation.cancelled() => {
                request_abort(&mut abort);
                (combined.await, true)
            }
        }
    };
    if cancelled {
        let _ = encode_res;
        let _ = exit;
        let _ = std::fs::remove_file(&spool_path);
        anyhow::bail!("PIPELINE_CANCELLED");
    }
    let (plaintext_hash, ciphertext_hash, plain, cipher) = encode_res??; // spawn_blocking join + 내부 Result
    let exit = exit?;

    // R2: EOF에 도달했어도 exit≠0 또는 치명 stderr면 FAIL → 스풀 폐기.
    if !exit.success || is_fatal_stderr(&exit.stderr_tail, &cfg.fatal_patterns) {
        let _ = std::fs::remove_file(&spool_path);
        anyhow::bail!(
            "덤프 실패로 스풀 폐기: code={:?} stderr_tail={}",
            exit.code,
            exit.stderr_tail
        );
    }

    Ok(SpoolCommit {
        spool_path,
        plaintext_blake3: plaintext_hash,
        ciphertext_blake3: ciphertext_hash,
        plaintext_bytes: plain,
        ciphertext_bytes: cipher,
        stderr_tail: exit.stderr_tail,
    })
}

/// Directory 아티팩트 → tar-stream → 체인(R10/R11). 덤프 완료/exit 확인이 tar보다 앞선다(R2).
pub async fn run_directory_pipeline(
    dir: DumpDir,
    spool_path: std::path::PathBuf,
    dek: Dek,
    cfg: &PipelineCfg,
) -> anyhow::Result<SpoolCommit> {
    run_directory_pipeline_with_cancellation(
        dir,
        spool_path,
        dek,
        cfg,
        PipelineCancellationToken::default(),
    )
    .await
}

pub async fn run_directory_pipeline_with_cancellation(
    dir: DumpDir,
    spool_path: std::path::PathBuf,
    dek: Dek,
    cfg: &PipelineCfg,
    cancellation: PipelineCancellationToken,
) -> anyhow::Result<SpoolCommit> {
    let DumpDir {
        root,
        mut wait,
        mut abort,
    } = dir;
    let exit = if cancellation.is_cancelled() {
        request_abort(&mut abort);
        let _ = wait.await;
        secure_wipe_tempdir(root);
        let _ = std::fs::remove_file(&spool_path);
        anyhow::bail!("PIPELINE_CANCELLED");
    } else {
        tokio::select! {
            result = &mut wait => result?,
            () = cancellation.cancelled() => {
                request_abort(&mut abort);
                let _ = wait.await;
                secure_wipe_tempdir(root);
                let _ = std::fs::remove_file(&spool_path);
                anyhow::bail!("PIPELINE_CANCELLED");
            }
        }
    };
    if !exit.success || is_fatal_stderr(&exit.stderr_tail, &cfg.fatal_patterns) {
        anyhow::bail!("Directory 덤프 실패: code={:?}", exit.code);
    }
    let sp = spool_path.clone();
    let level = cfg.zstd_level;

    let (plaintext_hash, ciphertext_hash, plain, cipher) =
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut chain = EncryptChain::new(&sp, &dek, level)?;

            // ===== CRITICAL REGION: tar append → 커밋. `?`는 아래 블록으로 수렴 =====
            let tarred: anyhow::Result<()> = (|| {
                let mut builder = tar::Builder::new(chain.writer());
                builder.append_dir_all(".", root.path())?;
                builder.finish()?; // tar 트레일러 — 반드시
                Ok(())
            })();

            let out = match tarred {
                Ok(()) => chain.commit(),
                Err(e) => {
                    chain.abort_hint();
                    Err(e)
                }
            };
            // M2: 평문 TempDir 즉시 안전 삭제(tar가 체인에 다 들어간 뒤).
            secure_wipe_tempdir(root);
            match out {
                Ok(v) => Ok(v),
                Err(e) => {
                    let _ = std::fs::remove_file(&sp);
                    Err(e)
                }
            }
        })
        .await??;

    Ok(SpoolCommit {
        spool_path,
        plaintext_blake3: plaintext_hash,
        ciphertext_blake3: ciphertext_hash,
        plaintext_bytes: plain,
        ciphertext_bytes: cipher,
        stderr_tail: exit.stderr_tail,
    })
}

/// S05 §7 진입점: 아티팩트 종류에 따라 적절한 파이프라인으로 분기.
pub async fn run_artifact(
    artifact: DumpArtifact,
    spool_path: std::path::PathBuf,
    dek: Dek,
    cfg: &PipelineCfg,
) -> anyhow::Result<SpoolCommit> {
    match artifact {
        DumpArtifact::Stream(s) => run_stream_pipeline(s, spool_path, dek, cfg).await,
        DumpArtifact::NativeStream(native) => {
            run_stream_pipeline(native.stream, spool_path, dek, cfg).await
        }
        DumpArtifact::Directory(d) => run_directory_pipeline(d, spool_path, dek, cfg).await,
    }
}

/// 덤프 시작 전 스풀 볼륨 여유 확인(R9).
pub async fn run_artifact_with_cancellation(
    artifact: DumpArtifact,
    spool_path: std::path::PathBuf,
    dek: Dek,
    cfg: &PipelineCfg,
    cancellation: PipelineCancellationToken,
) -> anyhow::Result<SpoolCommit> {
    match artifact {
        DumpArtifact::Stream(stream) => {
            run_stream_pipeline_with_cancellation(stream, spool_path, dek, cfg, cancellation).await
        }
        DumpArtifact::NativeStream(native) => {
            run_stream_pipeline_with_cancellation(native.stream, spool_path, dek, cfg, cancellation)
                .await
        }
        DumpArtifact::Directory(directory) => {
            run_directory_pipeline_with_cancellation(directory, spool_path, dek, cfg, cancellation)
                .await
        }
    }
}

fn request_abort(abort: &mut Option<Box<dyn FnOnce() + Send + 'static>>) {
    if let Some(abort) = abort.take() {
        abort();
    }
}

pub fn preflight_disk_guard(spool_dir: &std::path::Path, min_free: u64) -> anyhow::Result<()> {
    if min_free == 0 {
        return Ok(());
    }
    let free = fs2::available_space(spool_dir)?;
    anyhow::ensure!(
        free >= min_free,
        "스풀 볼륨 여유 부족: free={free} < min_free_disk={min_free}"
    );
    Ok(())
}
