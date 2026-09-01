// crates/restoreplane-pipeline/src/lib.rs
//! restoreplane-pipeline — S05 Pipeline + Commit Protocol(옵션 C) + S06 Destination(LocalDest) +
//! S03 BackupSource 공용 계약. 슬라이스 1의 하중 리스크를 StubSource로 DB 없이 증명한다.
//!
//! 인코딩 체인: 평문 → blake3 tee(평문 해시) → zstd → chacha20poly1305 STREAM(백업별 DEK) → 로컬 스풀.
//! 커밋 후 암호문+서명 매니페스트를 목적지별로 독립 업로드(팬아웃, 부분 성공 벡터).
//!
//! 섹션 대비 주요 편차(코드 주석에 국소적으로도 표기):
//! 1) payload 암호화는 age가 아니라 restoreplane-secrets의 chacha20poly1305 STREAM(P0-5 유지).
//! 2) LocalDest/Destination은 opendal 대신 std::fs로 구현(S3/opendal은 다음 슬라이스).
//! 3) S05/S06/S03가 별도 크레이트로 쪼개지는 대신, 공용 계약을 이 크레이트에 모아 seam을 닫는다.

pub mod artifact;
pub mod chain;
pub mod commit;
pub mod crypto;
pub mod dest;
pub mod fanout;
pub mod manifest;
pub mod manifest_v2;
pub mod repair;
pub mod retention;
pub mod source;
pub mod stub_source;

pub use artifact::{
    DumpArtifact, DumpDir, DumpExit, DumpStream, NativeChildProcess, NativeDumpStream,
};
pub use commit::{
    is_fatal_stderr, preflight_disk_guard, run_artifact, run_artifact_with_cancellation,
    run_directory_pipeline, run_directory_pipeline_with_cancellation, run_stream_pipeline,
    run_stream_pipeline_with_cancellation, PipelineCancellationToken, PipelineCfg, SpoolCommit,
};
pub use crypto::decrypt_and_decompress;
pub use dest::{
    assert_job_immutability, upload_resumable, DestCaps, DestError, DestWriter, Destination,
    DestinationReaderV2, ImmutabilityKind, LocalDest, ReaderObjectMetadataV2, ReaderObjectProbeV2,
    RequiredImmutability, UploadError, UploadReceipt, WriterBackend,
};
pub use fanout::{
    fan_out, publish_set_manifest_v2, DestOutcome, EncodedPartSourceV2, ReplicaEndpointV2,
    ReplicaPublishError, ReplicaPublishOutcomeV2, MAX_SET_MANIFEST_BYTES_V2,
};
pub use manifest::{
    signed_manifest_path, write_signed_manifest, write_signed_manifest_with_wrapset, Manifest,
    SignedManifest,
};
pub use manifest_v2::{
    EncodingEvidenceV2, ManifestPartV2, ManifestV2Error, NativeToolIdentityV2, ReaderStatusV2,
    RecoveryOriginV2, ReplicaCommitmentV2, ReplicaRequirementV2, SetManifestV2,
    SignedSetManifestV2, SET_MANIFEST_SCHEMA_VERSION,
};
pub use repair::{repair_replica_v2, RepairSourceV2};
pub use retention::{
    evaluate_retention_v2, execute_retention_delete_v2, ConfirmedRetentionDeleteV2, DeleteErrorV2,
    DeleteObjectDispositionV2, DeleteObjectObservationV2, DestinationDeleteV2,
    LocalDeleteOperatorV2, RetentionCandidateV2, RetentionDecisionV2,
    RetentionDeleteAuthorizationV2, RetentionDeletePlanV2, RetentionDeleteReportV2,
    RetentionDispositionV2, RetentionError, RetentionObjectRoleV2, RetentionObjectV2,
    RetentionReasonV2,
};
pub use source::{
    BackupSource, DumpPartKind, DumpPlan, DumpPlanPart, ExtensionInfo, PlanWarning,
    SourceManifestEvidence,
};
pub use stub_source::StubSource;
