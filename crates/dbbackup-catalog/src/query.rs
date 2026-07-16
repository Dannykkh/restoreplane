//! 조회 전용 read API — GUI 대시보드/이력 UX가 SQLite 캐시를 타입 있게 읽는다.
//!
//! 신뢰 경계 재확인(H5/P0-6): 이 모듈이 돌려주는 값은 "표시용 캐시"다. 복원/검증 가부 판단은
//! 절대 이 캐시로 내리지 않고 목적지 서명 매니페스트 재검증(gate.rs)으로 내린다.

use rusqlite::{Connection, Row};

/// 잡별 최신 백업 1건 + 그 백업의 대표 검증 레벨/결과(있으면).
#[derive(Debug, Clone)]
pub struct BackupRecord {
    pub id: String,
    pub job: String,
    pub source: String,
    pub engine: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub status: String,
    pub plaintext_bytes: Option<i64>,
    pub cipher_bytes: Option<i64>,
    pub manifest_signed: bool,
    pub last_verify_level: Option<i64>,
    pub last_verify_result: Option<String>,
}

fn row_to_record(row: &Row) -> rusqlite::Result<BackupRecord> {
    Ok(BackupRecord {
        id: row.get("id")?,
        job: row.get("job")?,
        source: row.get("source")?,
        engine: row.get("engine")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        status: row.get("status")?,
        plaintext_bytes: row.get("plaintext_bytes")?,
        cipher_bytes: row.get("cipher_bytes")?,
        manifest_signed: row.get::<_, Option<i64>>("manifest_signed")?.unwrap_or(0) != 0,
        last_verify_level: None,
        last_verify_result: None,
    })
}

/// 대표 검증: 레벨이 가장 높은 것, 동률이면 가장 최근 것(L3 통과가 L1보다 강한 신호).
fn attach_latest_verification(conn: &Connection, rec: &mut BackupRecord) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare_cached(
        "SELECT level, result FROM verifications
         WHERE backup_id = ?1
         ORDER BY level DESC, verified_at DESC
         LIMIT 1",
    )?;
    let mut rows = stmt.query([&rec.id])?;
    if let Some(row) = rows.next()? {
        rec.last_verify_level = Some(row.get("level")?);
        rec.last_verify_result = Some(row.get("result")?);
    }
    Ok(())
}

/// 잡별 최신 백업(started_at 최대) 1건씩. 검증 정보도 채운다.
pub fn latest_backup_per_job(conn: &Connection) -> rusqlite::Result<Vec<BackupRecord>> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.job, b.source, b.engine, b.started_at, b.finished_at, b.status,
                b.plaintext_bytes, b.cipher_bytes, b.manifest_signed
         FROM backups b
         JOIN (SELECT job, MAX(started_at) AS mx FROM backups GROUP BY job) t
           ON b.job = t.job AND b.started_at = t.mx
         ORDER BY b.job",
    )?;
    let mut recs: Vec<BackupRecord> = stmt
        .query_map([], row_to_record)?
        .collect::<rusqlite::Result<_>>()?;
    for r in &mut recs {
        attach_latest_verification(conn, r)?;
    }
    Ok(recs)
}

/// 최근 백업 N건(전 잡 통합, started_at 내림차순). 이력 패널용.
pub fn recent_backups(conn: &Connection, limit: u32) -> rusqlite::Result<Vec<BackupRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, job, source, engine, started_at, finished_at, status,
                plaintext_bytes, cipher_bytes, manifest_signed
         FROM backups
         ORDER BY started_at DESC
         LIMIT ?1",
    )?;
    let mut recs: Vec<BackupRecord> = stmt
        .query_map([limit], row_to_record)?
        .collect::<rusqlite::Result<_>>()?;
    for r in &mut recs {
        attach_latest_verification(conn, r)?;
    }
    Ok(recs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::open_writer_conn;

    fn seed(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO backups (id,job,source,engine,kind,started_at,finished_at,status,plaintext_bytes,cipher_bytes,manifest_signed,schema_version)
             VALUES ('b1','nightly','pgmain','postgresql','dump','2026-07-16 03:00:00','2026-07-16 03:05:00','success',1000,300,1,1),
                    ('b2','nightly','pgmain','postgresql','dump','2026-07-17 03:00:00','2026-07-17 03:06:00','success',1100,320,1,1),
                    ('b3','weekly','mysqlbill','mysql','dump','2026-07-12 04:15:00',NULL,'failed',NULL,NULL,0,1);
             INSERT INTO verifications (id,backup_id,verified_at,level,result)
             VALUES ('v1','b2','2026-07-17 04:00:00',1,'pass'),
                    ('v2','b2','2026-07-17 05:00:00',3,'pass');",
        )
        .unwrap();
    }

    #[test]
    fn latest_per_job_picks_newest_and_best_verification() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_writer_conn(&dir.path().join("c.sqlite")).unwrap();
        seed(&conn);

        let recs = latest_backup_per_job(&conn).unwrap();
        assert_eq!(recs.len(), 2, "잡 2개(nightly/weekly) 각 1건");

        let nightly = recs.iter().find(|r| r.job == "nightly").unwrap();
        assert_eq!(nightly.id, "b2", "최신(2026-07-17) 선택");
        assert_eq!(
            nightly.last_verify_level,
            Some(3),
            "레벨 높은 검증(L3) 대표"
        );
        assert_eq!(nightly.last_verify_result.as_deref(), Some("pass"));
        assert!(nightly.manifest_signed);

        let weekly = recs.iter().find(|r| r.job == "weekly").unwrap();
        assert_eq!(weekly.status, "failed");
        assert_eq!(weekly.last_verify_level, None, "검증 없음");
        assert!(!weekly.manifest_signed);
    }

    #[test]
    fn recent_orders_desc_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_writer_conn(&dir.path().join("c.sqlite")).unwrap();
        seed(&conn);

        let recs = recent_backups(&conn, 2).unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].id, "b2", "가장 최근이 먼저");
    }
}
