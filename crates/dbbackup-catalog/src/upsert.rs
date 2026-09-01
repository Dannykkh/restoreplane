// S08 8.5 — 매니페스트 → SQLite 캐시 반영(backups + artifacts).
//
// 중요: 여기 채워지는 SQLite 값은 "조회 캐시"일 뿐이다. 복원/검증 판단은 이 테이블 값이 아니라
// 목적지의 서명된 매니페스트를 다시 검증해서 내린다(gate.rs, H5). 파이프라인 Manifest는 슬라이스 1
// 부분집합이라 server_version/charset 등 일부 컬럼은 NULL로 둔다(하위호환).

use dbbackup_core::Engine;
use dbbackup_pipeline::Manifest;
use rusqlite::{params, Connection};

use crate::writer::CatalogWriterHandle;

/// 파이프라인 Manifest에는 타임스탬프가 없다(슬라이스 1). rebuild 재현성을 위해 SQL now()를 쓴다.
const NOW_SQL: &str = "strftime('%Y-%m-%dT%H:%M:%SZ','now')";

fn engine_str(e: &Engine) -> &'static str {
    match e {
        Engine::Postgresql => "postgresql",
        Engine::Mysql => "mysql",
        Engine::Mariadb => "mariadb",
        Engine::Sqlserver => "sqlserver",
        Engine::Sqlite => "sqlite",
    }
}

/// 단일 커넥션/트랜잭션 안에서 backups + artifacts를 upsert한다(overlap 없는 짧은 트랜잭션).
pub fn upsert_backup_from_manifest_conn(
    conn: &mut Connection,
    m: &Manifest,
    dest_name: &str,
    dest_kind: &str,
) -> anyhow::Result<()> {
    let tx = conn.transaction()?;
    let id = m.backup_id.to_string();
    let engine = engine_str(&m.engine);

    tx.execute(
        &format!(
            "INSERT INTO backups
             (id, job, source, engine, kind, pairs_with_globals_backup_id,
              started_at, finished_at, status, server_version, dump_tool,
              dump_tool_version, dump_format, charset, plaintext_blake3,
              plaintext_bytes, cipher_bytes, dek_wrap_gen, manifest_signed, schema_version)
             VALUES (?1,?2,?3,?4,'db',NULL,{NOW_SQL},{NOW_SQL},'success',NULL,?5,?6,?7,NULL,?8,?9,?10,?11,1,1)
             ON CONFLICT(id) DO UPDATE SET
                status=excluded.status,
                finished_at=excluded.finished_at,
                plaintext_blake3=excluded.plaintext_blake3,
                plaintext_bytes=excluded.plaintext_bytes,
                cipher_bytes=excluded.cipher_bytes,
                manifest_signed=excluded.manifest_signed"
        ),
        params![
            id,
            m.job,
            m.source_alias,
            engine,
            m.dump_tool,
            m.dump_tool_version,
            m.dump_format,
            m.plaintext_blake3,
            m.plaintext_bytes as i64,
            m.ciphertext_bytes as i64,
            m.dek_wrap_gen as i64,
        ],
    )?;

    tx.execute(
        &format!(
            "INSERT INTO artifacts
             (backup_id, dest_name, dest_kind, object_key, object_checksum,
              immutability, retention_until, status, uploaded_at)
             VALUES (?1,?2,?3,?4,?5,NULL,NULL,'committed',{NOW_SQL})
             ON CONFLICT(backup_id, dest_name) DO UPDATE SET
                object_checksum=excluded.object_checksum, status='committed'"
        ),
        params![id, dest_name, dest_kind, m.spool_object, m.plaintext_blake3,],
    )?;

    tx.commit()?;
    Ok(())
}

/// writer 액터 경유 비동기(호출자 관점)로 upsert.
pub fn upsert_backup_from_manifest(
    writer: &CatalogWriterHandle,
    m: Manifest,
    dest_name: String,
    dest_kind: String,
) -> anyhow::Result<()> {
    writer.run(move |conn| upsert_backup_from_manifest_conn(conn, &m, &dest_name, &dest_kind))
}
