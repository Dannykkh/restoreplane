// S08 8.1 — SQLite 연결 관리(WAL + PRAGMA). writer는 단일 커넥션(writer.rs), reader는 다수 허용.
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

/// db-schema.md DDL 정본. include_str로 컴파일타임 임베드(런타임 파일 의존 없음).
pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// writer 전용 커넥션. 프로세스 전체에서 단 하나만 존재해야 한다(P1-5, FR2).
pub fn open_writer_conn(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    apply_pragmas(&conn)?;
    // CREATE TABLE/INDEX IF NOT EXISTS → 멱등. execute_batch는 결과 행을 무시한다.
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}

/// 읽기 전용 커넥션. WAL 리더끼리는 서로/writer를 막지 않는다.
pub fn open_reader_conn(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // 리더도 busy_timeout을 걸어 writer의 짧은 checkpoint 구간과 겹쳐도 SQLITE_BUSY로 즉사하지 않게.
    conn.busy_timeout(std::time::Duration::from_millis(5000))?;
    Ok(conn)
}

/// journal_mode=WAL 등은 결과 행을 돌려주므로 pragma_update 대신 execute_batch로 설정한다
/// (pragma_update는 "결과 행 반환" 시 에러를 낸다).
fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;\
         PRAGMA busy_timeout=5000;\
         PRAGMA synchronous=NORMAL;\
         PRAGMA foreign_keys=ON;",
    )
}
