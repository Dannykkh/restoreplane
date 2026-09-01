// S09 9.6 — 검증 결과를 db-schema.md verifications 테이블에 기록한다(레벨/결과/버전/행수/스키마해시/노트).
// "verified"는 항상 레벨과 함께 저장한다(VerifyOutcome::label 규율).
// rusqlite는 catalog 크레이트가 재노출하는 것을 쓴다(verify는 rusqlite를 직접 의존하지 않는다).

use restoreplane_catalog::rusqlite::params;
use restoreplane_catalog::writer::CatalogWriterHandle;

use crate::verify::{VerifyOutcome, VerifyResult};

pub fn record_verification(
    writer: &CatalogWriterHandle,
    backup_id: &str,
    o: &VerifyOutcome,
) -> anyhow::Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    let level = o.level as i64;
    let result = if o.result == VerifyResult::Pass {
        "pass"
    } else {
        "fail"
    };
    let rtv = o.restore_target_version.clone();
    let rows = o.restored_rows.map(|r| r as i64);
    let shash = o.schema_hash.clone();
    let notes = o.notes.clone();
    let bid = backup_id.to_string();

    writer.run(move |conn| {
        conn.execute(
            "INSERT INTO verifications
             (id, backup_id, verified_at, level, result, restore_target_version,
              restored_rows, schema_hash, notes)
             VALUES (?1,?2, strftime('%Y-%m-%dT%H:%M:%SZ','now'), ?3,?4,?5,?6,?7,?8)",
            params![id, bid, level, result, rtv, rows, shash, notes],
        )?;
        Ok(())
    })
}
