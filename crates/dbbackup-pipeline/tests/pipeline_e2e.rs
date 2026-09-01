// crates/dbbackup-pipeline/tests/pipeline_e2e.rs
//! DB 없이 전체 백업 경로를 구동하는 end-to-end 테스트.
//! StubSource → 인코딩 체인(blake3 tee → zstd → chacha20poly1305 STREAM) → LocalDest 스풀 →
//! 서명 매니페스트 → 팬아웃. Commit Protocol의 실패 경로(절단-as-success 방지)도 검증한다.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

use dbbackup_pipeline::{
    assert_job_immutability, decrypt_and_decompress, fan_out, run_artifact,
    run_artifact_with_cancellation, write_signed_manifest, BackupSource, Destination, DumpArtifact,
    DumpExit, DumpStream, LocalDest, Manifest, PipelineCancellationToken, PipelineCfg,
    RequiredImmutability, SpoolCommit, StubSource,
};
use dbbackup_secrets::{generate_signing_keypair, Dek};

/// 청크 경계를 반드시 넘기는 결정적 payload(STREAM_CHUNK=64KiB 초과).
fn deterministic_payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + 7) as u8).collect()
}

struct DeterministicStream {
    remaining: u64,
    state: u64,
}

impl DeterministicStream {
    const fn new(bytes: u64) -> Self {
        Self {
            remaining: bytes,
            state: 0x4d59_5df4_d0f3_3173,
        }
    }
}

impl AsyncRead for DeterministicStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let take = usize::try_from(self.remaining.min(buffer.remaining() as u64))
            .expect("bounded read length");
        if take == 0 {
            return Poll::Ready(Ok(()));
        }
        let mut state = self.state;
        let output = buffer.initialize_unfilled_to(take);
        for byte in output.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state >> 24) as u8;
        }
        buffer.advance(take);
        self.state = state;
        self.remaining -= take as u64;
        Poll::Ready(Ok(()))
    }
}

#[cfg(windows)]
fn peak_working_set_bytes() -> usize {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..PROCESS_MEMORY_COUNTERS::default()
    };
    // SAFETY: GetCurrentProcess returns a pseudo-handle valid in this process and
    // counters points to an initialized buffer whose byte size is passed exactly.
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
        .expect("query process memory counters");
    }
    counters.PeakWorkingSetSize
}

#[cfg(not(windows))]
const fn peak_working_set_bytes() -> usize {
    0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_256_mib_payload_does_not_require_a_payload_sized_input_buffer() {
    const PAYLOAD_BYTES: u64 = 256 * 1_024 * 1_024;
    const MAX_PEAK_WORKING_SET_GROWTH: usize = 96 * 1_024 * 1_024;
    let temp = tempfile::tempdir().expect("temp directory");
    let spool_path = temp.path().join("large-stream.spool");
    let peak_before = peak_working_set_bytes();
    let reader = DeterministicStream::new(PAYLOAD_BYTES);
    let artifact = DumpArtifact::Stream(DumpStream {
        reader: Box::new(reader),
        wait: Box::pin(async {
            Ok(DumpExit {
                code: Some(0),
                success: true,
                stderr_tail: String::new(),
            })
        }),
        abort: None,
    });

    let commit = run_artifact(
        artifact,
        spool_path,
        Dek::generate(),
        &PipelineCfg::default(),
    )
    .await
    .expect("large generated stream commits");

    assert_eq!(commit.plaintext_bytes, PAYLOAD_BYTES);
    assert!(
        commit.ciphertext_bytes > PAYLOAD_BYTES * 95 / 100,
        "deterministic high-entropy input must produce a payload-sized spool"
    );
    assert!(commit.spool_path.exists());
    assert_eq!(
        std::fs::metadata(&commit.spool_path)
            .expect("committed spool metadata")
            .len(),
        commit.ciphertext_bytes
    );
    let peak_growth = peak_working_set_bytes().saturating_sub(peak_before);
    #[cfg(windows)]
    assert!(
        peak_growth <= MAX_PEAK_WORKING_SET_GROWTH,
        "256 MiB payload grew peak working set by {peak_growth} bytes"
    );
    println!(
        "PBE_LARGE_PAYLOAD_EVIDENCE={{\"plaintextBytes\":{},\"ciphertextBytes\":{},\"peakWorkingSetGrowthBytes\":{},\"maxPeakWorkingSetGrowthBytes\":{}}}",
        commit.plaintext_bytes,
        commit.ciphertext_bytes,
        peak_growth,
        MAX_PEAK_WORKING_SET_GROWTH,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_aborts_dump_joins_encoder_and_removes_spool() {
    let temp = tempfile::tempdir().expect("temp directory");
    let spool_path = temp.path().join("cancelled.spool");
    let (reader, held_writer) = tokio::io::duplex(64);
    let (abort_sender, abort_receiver) = tokio::sync::oneshot::channel();
    let aborted = Arc::new(AtomicBool::new(false));
    let abort_observer = Arc::clone(&aborted);
    let artifact = DumpArtifact::Stream(DumpStream {
        reader: Box::new(reader),
        wait: Box::pin(async move {
            let _ = abort_receiver.await;
            drop(held_writer);
            Ok(DumpExit {
                code: None,
                success: false,
                stderr_tail: "cancelled".to_owned(),
            })
        }),
        abort: Some(Box::new(move || {
            abort_observer.store(true, Ordering::Release);
            let _ = abort_sender.send(());
        })),
    });
    let cancellation = PipelineCancellationToken::default();
    let run_cancellation = cancellation.clone();
    let run_spool = spool_path.clone();
    let task = tokio::spawn(async move {
        run_artifact_with_cancellation(
            artifact,
            run_spool,
            Dek::generate(),
            &PipelineCfg::default(),
            run_cancellation,
        )
        .await
    });

    tokio::task::yield_now().await;
    cancellation.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("cancelled pipeline must join promptly")
        .expect("pipeline task must not panic");

    assert!(result.is_err());
    assert!(aborted.load(Ordering::Acquire));
    assert!(!spool_path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_pipeline_stub_to_localdest_roundtrips_and_manifest_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    // 1) StubSource(DB 없음)로 결정적 바이트 방출.
    let payload = deterministic_payload(200 * 1024); // 다중 STREAM 청크
    let source = StubSource::ok("job-a", "stub-db", payload.clone());
    let part = source.plan().await.unwrap().db_parts.remove(0);
    let artifact = source.spawn(&part).unwrap();

    // 2) 백업별 DEK로 파이프라인 구동 → 스풀 커밋.
    let dek = Dek::generate();
    let spool_path = spool_dir.join(format!("{}.spool", part.backup_id));
    let cfg = PipelineCfg::default();
    let commit = run_artifact(artifact, spool_path.clone(), dek.clone(), &cfg)
        .await
        .expect("파이프라인 성공");

    // 스풀 파일 존재.
    assert!(commit.spool_path.exists(), "스풀 파일이 존재해야 함");
    assert_eq!(commit.plaintext_bytes, payload.len() as u64);
    assert!(commit.ciphertext_bytes > 0);

    // 3) 복호 + zstd decode가 원본 바이트를 재현.
    let recovered = decrypt_and_decompress(&commit.spool_path, &dek).unwrap();
    assert_eq!(recovered, payload, "라운드트립이 원본 평문을 재현해야 함");

    // 4) 평문 blake3가 커밋 해시와 일치.
    let expected = blake3::hash(&payload);
    assert_eq!(&commit.plaintext_blake3, expected.as_bytes());
    let encrypted = std::fs::read(&commit.spool_path).unwrap();
    let expected_ciphertext = blake3::hash(&encrypted);
    assert_eq!(
        &commit.ciphertext_blake3,
        expected_ciphertext.as_bytes(),
        "committed ciphertext hash must describe the exact published bytes"
    );

    // 5) 서명 매니페스트 생성 + 검증.
    let (sk, vk) = generate_signing_keypair();
    let manifest = Manifest::from_commit(&part, dbbackup_core::Engine::Postgresql, &commit, 1);
    let (manifest_path, signed) = write_signed_manifest(&commit.spool_path, manifest, &sk).unwrap();
    assert!(
        manifest_path.exists(),
        "매니페스트 JSON이 스풀 옆에 있어야 함"
    );
    signed.verify(&vk).expect("서명 검증 통과");

    // 변조 감지: 매니페스트 필드를 바꾸면 검증 실패.
    let mut tampered = signed.clone();
    tampered.manifest.plaintext_bytes += 1;
    assert!(
        tampered.verify(&vk).is_err(),
        "변조된 매니페스트는 검증 실패해야 함"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_protocol_nonzero_exit_discards_spool() {
    // 자식이 EOF에 도달하더라도 exit≠0이면 스풀을 남기면 안 된다(truncation-as-success 방지, R2).
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let payload = deterministic_payload(128 * 1024);
    let source = StubSource::failing("job-fail", "stub-db", payload);
    let part = source.plan().await.unwrap().db_parts.remove(0);
    let artifact = source.spawn(&part).unwrap();

    let dek = Dek::generate();
    let spool_path = spool_dir.join("fail.spool");
    let cfg = PipelineCfg::default();
    let res = run_artifact(artifact, spool_path.clone(), dek, &cfg).await;

    assert!(res.is_err(), "실패 소스는 커밋되면 안 됨");
    assert!(
        !spool_path.exists(),
        "실패 시 스풀 파일이 남으면 안 됨(커밋 미확정)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fanout_partial_success_one_unwritable_dest() {
    // 목적지 2개 중 하나는 root가 파일이라 쓸 수 없다 → 부분 성공 벡터.
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let payload = deterministic_payload(100 * 1024);
    let source = StubSource::ok("job-fan", "stub-db", payload);
    let part = source.plan().await.unwrap().db_parts.remove(0);
    let artifact = source.spawn(&part).unwrap();

    let dek = Dek::generate();
    let spool_path = spool_dir.join("fan.spool");
    let cfg = PipelineCfg {
        upload_retries: 1,
        ..PipelineCfg::default()
    };
    let commit = run_artifact(artifact, spool_path, dek, &cfg).await.unwrap();

    let (sk, vk) = generate_signing_keypair();
    let manifest = Manifest::from_commit(&part, dbbackup_core::Engine::Postgresql, &commit, 1);
    write_signed_manifest(&commit.spool_path, manifest, &sk).unwrap();

    // 정상 목적지.
    let ok_dir = tmp.path().join("dest_ok");
    std::fs::create_dir_all(&ok_dir).unwrap();
    let ok_dest = Arc::new(LocalDest::new("local-ok", ok_dir.clone())) as Arc<dyn Destination>;

    // 쓸 수 없는 목적지: root 경로에 파일이 존재 → create_dir_all 실패.
    let bad_root = tmp.path().join("dest_bad");
    std::fs::write(&bad_root, b"i am a file, not a dir").unwrap();
    let bad_dest = Arc::new(LocalDest::new("local-bad", bad_root.clone())) as Arc<dyn Destination>;

    let dests = vec![ok_dest, bad_dest];

    // Job 불변성 assert: LocalDest는 None이므로 required=None은 통과, ObjectLock은 실패.
    assert!(assert_job_immutability(RequiredImmutability::None, &dests).is_ok());
    assert!(assert_job_immutability(RequiredImmutability::ObjectLock, &dests).is_err());

    let outcomes = fan_out(&commit, dests, &cfg).await;
    assert_eq!(outcomes.len(), 2);
    let ok_count = outcomes.iter().filter(|o| o.result.is_ok()).count();
    let err_count = outcomes.iter().filter(|o| o.result.is_err()).count();
    assert_eq!(ok_count, 1, "정상 목적지 1개는 성공해야 함");
    assert_eq!(err_count, 1, "쓸 수 없는 목적지 1개는 실패해야 함");

    // 성공한 목적지에 실제 객체가 존재(부분 객체가 아니라 최종 커밋된 파일).
    let landed = ok_dir.join("fan.spool");
    assert!(landed.exists(), "성공 목적지에 최종 객체가 있어야 함");
    let landed_manifest = ok_dir.join("fan.spool.manifest.json");
    assert!(
        landed_manifest.exists(),
        "성공 목적지에는 서명 매니페스트도 함께 있어야 함"
    );
    let signed: dbbackup_pipeline::SignedManifest =
        serde_json::from_slice(&std::fs::read(&landed_manifest).unwrap()).unwrap();
    signed.verify(&vk).expect("목적지 매니페스트 서명 검증");
    // 부분 파일(.part)은 남지 않아야 함.
    assert!(
        !ok_dir.join("fan.part").exists(),
        "커밋 후 .part 임시파일이 남으면 안 됨"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fanout_missing_manifest_does_not_publish_ciphertext() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_path = tmp.path().join("orphan.spool");
    std::fs::write(&spool_path, b"encrypted-backup").unwrap();
    let commit = SpoolCommit {
        spool_path,
        plaintext_blake3: [0; 32],
        ciphertext_blake3: [0; 32],
        plaintext_bytes: 16,
        ciphertext_bytes: 16,
        stderr_tail: String::new(),
    };

    let dest_root = tmp.path().join("dest");
    let dest = Arc::new(LocalDest::new("local", dest_root.clone())) as Arc<dyn Destination>;

    let outcomes = fan_out(&commit, vec![dest], &PipelineCfg::default()).await;

    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].result.is_err(),
        "동반 매니페스트가 없으면 목적지 커밋은 실패해야 함"
    );
    assert!(
        !dest_root.join("orphan.spool").exists(),
        "매니페스트를 읽지 못하면 암호문도 게시하면 안 됨"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fanout_manifest_commit_failure_marks_destination_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let spool_dir = tmp.path().join("spool");
    std::fs::create_dir_all(&spool_dir).unwrap();

    let source = StubSource::ok("job-manifest-fail", "stub-db", deterministic_payload(4096));
    let part = source.plan().await.unwrap().db_parts.remove(0);
    let artifact = source.spawn(&part).unwrap();
    let spool_path = spool_dir.join("manifest-fail.spool");
    let cfg = PipelineCfg::default();
    let commit = run_artifact(artifact, spool_path, Dek::generate(), &cfg)
        .await
        .unwrap();
    let (sk, _) = generate_signing_keypair();
    let manifest = Manifest::from_commit(&part, dbbackup_core::Engine::Postgresql, &commit, 1);
    write_signed_manifest(&commit.spool_path, manifest, &sk).unwrap();

    let dest_root = tmp.path().join("dest");
    std::fs::create_dir_all(dest_root.join("manifest-fail.spool.manifest.json")).unwrap();
    let dest =
        Arc::new(LocalDest::new("manifest-rejected", dest_root.clone())) as Arc<dyn Destination>;

    let outcomes = fan_out(&commit, vec![dest], &cfg).await;

    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].result.is_err(),
        "매니페스트 커밋 실패는 목적지 전체 실패여야 함"
    );
    assert!(
        dest_root.join("manifest-fail.spool").exists(),
        "manifest-last 프로토콜에서는 이미 커밋된 암호문이 고아 객체로 남을 수 있음"
    );
    assert!(
        !dest_root.join("manifest-fail.spool.manifest.part").exists(),
        "매니페스트 커밋 실패 후 부분 객체가 남으면 안 됨"
    );
}
