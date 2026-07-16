// crates/dbbackup-pipeline/src/lib.rs
//! dbbackup-pipeline — S05 Pipeline + Commit Protocol(옵션 C) + S06 Destination(LocalDest) +
//! S03 BackupSource 공용 계약. 슬라이스 1의 하중 리스크를 StubSource로 DB 없이 증명한다.
//!
//! 인코딩 체인: 평문 → blake3 tee(평문 해시) → zstd → chacha20poly1305 STREAM(백업별 DEK) → 로컬 스풀.
//! 커밋 후 목적지별 독립 업로드(팬아웃, 부분 성공 벡터).
//!
//! 섹션 대비 주요 편차(코드 주석에 국소적으로도 표기):
//! 1) payload 암호화는 age가 아니라 dbbackup-secrets의 chacha20poly1305 STREAM(P0-5 유지).
//! 2) LocalDest/Destination은 opendal 대신 std::fs로 구현(S3/opendal은 다음 슬라이스).
//! 3) S05/S06/S03가 별도 크레이트로 쪼개지는 대신, 공용 계약을 이 크레이트에 모아 seam을 닫는다.

pub mod artifact;
pub mod chain;
pub mod commit;
pub mod crypto;
pub mod dest;
pub mod fanout;
pub mod manifest;
pub mod source;
pub mod stub_source;

pub use artifact::{DumpArtifact, DumpDir, DumpExit, DumpStream};
pub use commit::{
    is_fatal_stderr, preflight_disk_guard, run_artifact, run_directory_pipeline,
    run_stream_pipeline, PipelineCfg, SpoolCommit,
};
pub use crypto::decrypt_and_decompress;
pub use dest::{
    assert_job_immutability, upload_resumable, DestCaps, DestError, DestWriter, Destination,
    ImmutabilityKind, LocalDest, RequiredImmutability, UploadError, UploadReceipt, WriterBackend,
};
pub use fanout::{fan_out, DestOutcome};
pub use manifest::{
    write_signed_manifest, write_signed_manifest_with_wrapset, Manifest, SignedManifest,
};
pub use source::{BackupSource, DumpPartKind, DumpPlan, DumpPlanPart, ExtensionInfo, PlanWarning};
pub use stub_source::StubSource;
