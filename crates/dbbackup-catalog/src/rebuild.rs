// S08 8.7 — catalog rebuild --from <dest> (P0-6).
// 카탈로그가 통째로 소실/조작돼도 목적지에 동봉된 서명 매니페스트만으로 backups/artifacts를 재구성한다.
// 서명 검증 실패/미서명 매니페스트는 카탈로그에 반영하지 않고 리포트로만 남긴다(FR4).

use crate::gate::{verify_bytes, ManifestVerifier};
use crate::source::ManifestSource;
use crate::upsert::upsert_backup_from_manifest_conn;
use crate::writer::CatalogWriterHandle;

pub struct RebuildArgs {
    pub job_filter: Option<String>,
    pub dry_run: bool,
    /// 재구성된 artifacts에 기록할 목적지 이름/종류(walking skeleton: 단일 로컬 목적지).
    pub dest_name: String,
    pub dest_kind: String,
}

impl Default for RebuildArgs {
    fn default() -> Self {
        Self {
            job_filter: None,
            dry_run: false,
            dest_name: "local".to_string(),
            dest_kind: "local".to_string(),
        }
    }
}

#[derive(Debug, Default)]
pub struct RebuildReport {
    pub accepted: usize,
    /// 미서명/파싱 실패/서명 위조로 배제된 매니페스트 키.
    pub rejected_unsigned_or_tampered: Vec<String>,
}

/// 목적지의 매니페스트만으로 카탈로그를 재구성한다.
pub async fn catalog_rebuild(
    source: &dyn ManifestSource,
    verifier: &dyn ManifestVerifier,
    writer: &CatalogWriterHandle,
    args: RebuildArgs,
) -> anyhow::Result<RebuildReport> {
    let keys = source.list_manifests(args.job_filter.as_deref()).await?;

    let mut report = RebuildReport::default();
    // (manifest, key) — 서명 검증 통과분만.
    let mut accepted = Vec::new();

    for key in &keys {
        let bytes = match source.read_bytes(key).await {
            Ok(b) => b,
            Err(_) => {
                report.rejected_unsigned_or_tampered.push(key.clone());
                continue;
            }
        };
        // 서명 게이트: 미서명/위조는 여기서 배제(카탈로그에 절대 반영하지 않는다).
        match verify_bytes(&bytes, verifier) {
            Ok(m) => accepted.push(m),
            Err(_) => report.rejected_unsigned_or_tampered.push(key.clone()),
        }
    }

    report.accepted = accepted.len();

    if args.dry_run {
        return Ok(report);
    }

    // 단일 writer 트랜잭션 배치로 반영(짧은 트랜잭션 다수).
    let dest_name = args.dest_name.clone();
    let dest_kind = args.dest_kind.clone();
    writer.run(move |conn| {
        for m in &accepted {
            upsert_backup_from_manifest_conn(conn, m, &dest_name, &dest_kind)?;
        }
        Ok(())
    })?;

    Ok(report)
}
