// S09 9.4 — Restorer(복호+복원 기계). 대상: 일회용 샌드박스(기본/검증) 또는 평문 산출(--decrypt-only).
// 복원 순서는 globals→db를 강제한다(PG). 카탈로그 값은 근거로 쓰지 않고, 목적지에서 읽은 암호문만 복호한다.

use dbbackup_catalog::source::ManifestSource;
use dbbackup_pipeline::Manifest;
use dbbackup_secrets::Dek;

use crate::decrypt::{decrypt_and_decompress_bytes, materialize_dump, PlaintextDump};
use crate::inputs::RestoreInputs;
use crate::sandbox::{ImagePolicy, Sandbox, SandboxSpec};

/// 복원 대상.
pub enum RestoreTarget {
    /// 일회용 Docker 컨테이너(C3 기본/검증).
    Sandbox,
    /// 복원 없이 평문 덤프만 저장(escape hatch, M2 경고).
    DecryptOnly(std::path::PathBuf),
}

pub struct RestoreOutcome {
    pub restore_target_version: String,
    pub sandbox: Option<Sandbox>,
}

pub struct Restorer<'a> {
    pub source: &'a dyn ManifestSource,
    pub dek: Dek,
    pub docker_bin: std::path::PathBuf,
    pub image_policy: ImagePolicy,
}

impl<'a> Restorer<'a> {
    /// 목적지에서 매니페스트가 가리키는 암호문 오브젝트를 읽는다.
    pub async fn read_cipher(&self, m: &Manifest) -> anyhow::Result<Vec<u8>> {
        self.source.read_bytes(&m.spool_object).await
    }

    /// 암호문 → 복호 평문 → PlaintextDump.
    pub async fn open_dump(&self, m: &Manifest) -> anyhow::Result<PlaintextDump> {
        let cipher = self.read_cipher(m).await?;
        let plain = decrypt_and_decompress_bytes(&cipher, &self.dek)?;
        materialize_dump(plain, &m.dump_format)
    }

    pub async fn restore(
        &self,
        inputs: &RestoreInputs,
        target: RestoreTarget,
    ) -> anyhow::Result<RestoreOutcome> {
        match target {
            RestoreTarget::DecryptOnly(path) => self.decrypt_only(inputs, &path).await,
            RestoreTarget::Sandbox => self.restore_into_sandbox(inputs).await,
        }
    }

    async fn restore_into_sandbox(&self, inputs: &RestoreInputs) -> anyhow::Result<RestoreOutcome> {
        let m = &inputs.db_manifest;
        // database_name 확장 필드가 없으면 source_alias로 대체(슬라이스 1).
        let spec = SandboxSpec::build(
            m.engine,
            inputs.server_version.as_deref(),
            &m.source_alias,
            &self.image_policy,
        )?;
        let sandbox = Sandbox::launch(&self.docker_bin, spec).await?;

        // (1) PG globals 먼저 — role/tablespace 없으면 DB 복원이 실패(FR4).
        if let Some(gm) = &inputs.globals_manifest {
            let g_plain = self.open_dump(gm).await?;
            sandbox.apply_globals_sql(g_plain).await?;
        }

        // (2) DB 덤프 — custom은 pg_restore --no-privileges(비슈퍼유저) stdin 파이프.
        let db_plain = self.open_dump(m).await?;
        sandbox.restore_db_dump(db_plain, &m.dump_format).await?;

        let target_version = sandbox.server_version().await?;
        Ok(RestoreOutcome {
            restore_target_version: target_version,
            sandbox: Some(sandbox),
        })
    }

    async fn decrypt_only(
        &self,
        inputs: &RestoreInputs,
        out_path: &std::path::Path,
    ) -> anyhow::Result<RestoreOutcome> {
        tracing::warn!(
            path = %out_path.display(),
            "--decrypt-only: 평문 덤프가 디스크에 기록됩니다(M2). 제한 ACL 경로 + 사용 후 삭제 권장."
        );
        let plain = self.open_dump(&inputs.db_manifest).await?;
        match plain {
            PlaintextDump::Bytes(b) => std::fs::write(out_path, b)?,
            PlaintextDump::Directory(_) => anyhow::bail!("디렉토리 포맷은 슬라이스 1 범위 밖"),
        }
        Ok(RestoreOutcome {
            restore_target_version: "n/a(decrypt-only)".to_string(),
            sandbox: None,
        })
    }
}
