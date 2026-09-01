// restoreplane-verify — S09: Restorer + Verifier(L1-L3, Docker 샌드박스).
//
// 두 개의 독립 방어선(red-team C3):
//   1) 출처 인증 게이트: 복원/검증 전 목적지 매니페스트의 ed25519 서명을 재검증(미서명/위조 즉시 거부).
//      restoreplane-catalog gate + restoreplane-pipeline SignedManifest를 재사용한다(병렬 매니페스트 금지, H5).
//   2) 격리 샌드박스: 서명이 유효해도 덤프는 적대적 입력으로 취급 — --network none/비특권/비슈퍼유저
//      /--no-privileges/--local-infile=0 일회용 컨테이너에서만 실제 복원(심층 방어).
//
// 복호 체인은 restoreplane-pipeline 인코딩 체인의 정확한 역순이며 청크 상수를 pipeline::chain에서 공유한다.
// L1(파일 무결성)은 Docker 없이 동작하는 "복원 가능성" 증명이다. L2(pg_restore)/L3(Docker)는 외부
// 런타임이 필요하므로 해당 테스트는 #[ignore]로 게이트한다.

pub mod asserts;
pub mod cadence;
pub mod decrypt;
pub mod inputs;
pub mod l1_v2;
pub mod policy;
pub mod record;
pub mod restorer;
pub mod sandbox;
pub mod sqlscan;
pub mod unlock;
pub mod verify;

pub use cadence::{levels_due, CadenceInput, WEEKLY_SECONDS};
pub use decrypt::{decrypt_and_decompress_bytes, materialize_dump, PlaintextDump};
pub use inputs::{load_trusted_inputs, RestoreInputError, RestoreInputs};
pub use l1_v2::{
    verify_l1_from_reader_v2, L1PartEvidenceV2, L1V2Error, L1VerificationReportV2,
    L1VerifyLimitsV2, PartDekProviderV2, SingleDekProviderV2,
};
pub use policy::{
    RequestedVerificationLevel, VerificationPolicy, VerificationPolicyError, VerificationSchedule,
};
pub use record::record_verification;
pub use restorer::{RestoreOutcome, RestoreTarget, Restorer};
pub use sandbox::{ImagePolicy, Sandbox, SandboxSpec};
pub use sqlscan::scan_dangerous_sql;
pub use unlock::{unwrap_dek, UnlockMode};
pub use verify::{Verifier, VerifyLevel, VerifyOutcome, VerifyResult};
