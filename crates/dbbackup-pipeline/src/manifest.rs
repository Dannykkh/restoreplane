// crates/dbbackup-pipeline/src/manifest.rs
//! 서명 매니페스트(S08 부분집합). 슬라이스 1 필드: backup_id, engine, plaintext_blake3, 크기,
//! dek_wrap_gen, dump_flags 등. dbbackup-secrets ed25519 서명으로 출처를 인증한다(C3).
//!
//! canonical 직렬화: 구조체 필드 순서를 고정한 serde_json(structs는 필드 순서 보존)을 canonical로
//! 간주한다(슬라이스 1 단순화 — 정식 JCS는 후속 슬라이스).

use dbbackup_core::Engine;
use dbbackup_secrets::WrapSet;
use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::commit::SpoolCommit;
use crate::source::DumpPlanPart;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub backup_id: Uuid,
    pub job: String,
    pub source_alias: String,
    pub engine: Engine,
    pub dump_tool: String,
    pub dump_tool_version: String,
    pub dump_format: String,
    pub dump_flags: String,
    /// 평문 blake3(hex, 압축 이전).
    pub plaintext_blake3: String,
    pub plaintext_bytes: u64,
    pub ciphertext_bytes: u64,
    pub dek_wrap_gen: u32,
    /// 스풀 객체 파일명(목적지 key).
    pub spool_object: String,
    /// per-backup DEK를 봉투 최상위 키(KEK)로 감싼 WrapSet(동봉). 키스토어로 KEK를 unlock하면
    /// 이 WrapSet에서 DEK를 복구할 수 있다. 서명에 포함되어 변조가 감지된다.
    /// 하위호환: 옛 매니페스트(WrapSet 미동봉)는 None으로 파싱되며, None이면 직렬화에서 생략한다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wrap_set: Option<WrapSet>,
}

impl Manifest {
    pub fn from_commit(
        part: &DumpPlanPart,
        engine: Engine,
        commit: &SpoolCommit,
        dek_wrap_gen: u32,
    ) -> Self {
        Manifest {
            backup_id: part.backup_id,
            job: part.job.clone(),
            source_alias: part.source_alias.clone(),
            engine,
            dump_tool: part.dump_tool.to_string(),
            dump_tool_version: part.dump_tool_version.clone(),
            dump_format: part.dump_format.to_string(),
            dump_flags: part.dump_flags.clone(),
            plaintext_blake3: data_encoding::HEXLOWER.encode(&commit.plaintext_blake3),
            plaintext_bytes: commit.plaintext_bytes,
            ciphertext_bytes: commit.ciphertext_bytes,
            dek_wrap_gen,
            spool_object: commit
                .spool_path
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            wrap_set: None,
        }
    }

    /// KEK로 감싼 per-backup DEK WrapSet을 동봉한다(서명 대상에 포함됨).
    pub fn with_wrap_set(mut self, wrap_set: WrapSet) -> Self {
        self.wrap_set = Some(wrap_set);
        self
    }

    /// canonical 직렬화(서명 대상 바이트).
    pub fn canonical_bytes(&self) -> anyhow::Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
}

/// 디스크에 쓰이는 서명 봉투(매니페스트 JSON + ed25519 서명 + 공개키 핀).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedManifest {
    pub manifest: Manifest,
    /// ed25519 서명(hex, 64B).
    pub signature_ed25519: String,
    /// 검증용 공개키(hex, 32B) — 복원 시 config 핀과 대조.
    pub verifying_key: String,
}

impl SignedManifest {
    pub fn create(manifest: Manifest, sk: &SigningKey) -> anyhow::Result<Self> {
        let canonical = manifest.canonical_bytes()?;
        let sig: Signature = dbbackup_secrets::sign_manifest(sk, &canonical);
        Ok(SignedManifest {
            manifest,
            signature_ed25519: data_encoding::HEXLOWER.encode(&sig.to_bytes()),
            verifying_key: data_encoding::HEXLOWER.encode(sk.verifying_key().as_bytes()),
        })
    }

    /// 핀된 공개키로 서명을 검증한다(미서명/불일치면 Err).
    pub fn verify(&self, vk: &VerifyingKey) -> anyhow::Result<()> {
        let canonical = self.manifest.canonical_bytes()?;
        let sig_bytes = data_encoding::HEXLOWER
            .decode(self.signature_ed25519.as_bytes())
            .map_err(|e| anyhow::anyhow!("서명 hex 디코드 실패: {e}"))?;
        let sig_arr: [u8; 64] = sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("서명 길이가 64B가 아님"))?;
        let sig = Signature::from_bytes(&sig_arr);
        dbbackup_secrets::verify_manifest(vk, &canonical, &sig)
            .map_err(|e| anyhow::anyhow!("매니페스트 서명 검증 실패: {e}"))
    }

    pub fn to_json(&self) -> anyhow::Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }
}

/// KEK로 감싼 DEK WrapSet을 동봉해 서명 매니페스트를 쓴다(run/verify 봉투 경로의 정본).
pub fn write_signed_manifest_with_wrapset(
    spool_path: &std::path::Path,
    manifest: Manifest,
    wrap_set: WrapSet,
    sk: &SigningKey,
) -> anyhow::Result<(std::path::PathBuf, SignedManifest)> {
    write_signed_manifest(spool_path, manifest.with_wrap_set(wrap_set), sk)
}

/// 스풀 객체 옆에 서명 매니페스트 JSON을 쓴다(`<spool>.manifest.json`).
pub fn write_signed_manifest(
    spool_path: &std::path::Path,
    manifest: Manifest,
    sk: &SigningKey,
) -> anyhow::Result<(std::path::PathBuf, SignedManifest)> {
    let signed = SignedManifest::create(manifest, sk)?;
    let mut manifest_path = spool_path.as_os_str().to_os_string();
    manifest_path.push(".manifest.json");
    let manifest_path = std::path::PathBuf::from(manifest_path);
    std::fs::write(&manifest_path, signed.to_json()?)?;
    Ok((manifest_path, signed))
}
