// crates/dbbackup-source-mysql/src/engine_scan.rs
//! S04 §4 — PRE-DUMP 엔진 스캔 + charset 캡처(R1/R3/R12). 분류 로직은 순수 함수라 주입된 행으로
//! DB 없이 유닛 테스트한다(T1~T3). 실제 information_schema 조회는 MySqlMetaConn(meta.rs)이 담당.
//!
//! 왜 "사전" 스캔인가: --single-transaction은 InnoDB에서만 REPEATABLE READ 스냅샷을 보장한다.
//! MyISAM 등 비-InnoDB 테이블은 덤프 도중 변경되면 **에러 없이 조용히** 비일관 데이터가 들어가고,
//! 사후엔 알 방법이 없다(§2.1). 그래서 덤프 실행 전에 반드시 스캔해 매니페스트에 기록해야 한다.

use crate::MySqlSourceError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NonTransactionalTable {
    pub schema: String,
    pub table: String,
    pub engine: String,
}

impl NonTransactionalTable {
    /// 매니페스트 표기 형식: "app.legacy_log(MyISAM)".
    pub fn display(&self) -> String {
        format!("{}.{}({})", self.schema, self.table, self.engine)
    }
}

pub const SYSTEM_SCHEMAS: &[&str] =
    &["mysql", "information_schema", "performance_schema", "sys"];

/// db_name이 시스템 스키마면 즉시 거부(R12, 방어적 설정 오류 차단).
pub fn ensure_not_system_schema(db_name: &str) -> Result<(), MySqlSourceError> {
    if SYSTEM_SCHEMAS.contains(&db_name) {
        return Err(MySqlSourceError::SystemSchemaRejected(db_name.to_string()));
    }
    Ok(())
}

/// information_schema.tables 행(schema, table, engine)을 분류해 비-InnoDB BASE TABLE만 남긴다(R1).
/// 실제 SQL에도 필터가 있지만(belt-and-suspenders), 주입 행 테스트를 위해 Rust 쪽에서도 필터한다:
/// - engine이 InnoDB(대소문자 무시)면 제외
/// - engine이 비어있거나 NULL 표기('', 'NULL')이면 뷰/특수 테이블로 보고 제외
/// - 시스템 스키마 행 제외
pub fn classify_non_transactional(
    rows: impl IntoIterator<Item = (String, String, String)>,
) -> Vec<NonTransactionalTable> {
    rows.into_iter()
        .filter(|(schema, _table, engine)| {
            let engine_norm = engine.trim();
            !engine_norm.is_empty()
                && !engine_norm.eq_ignore_ascii_case("NULL")
                && !engine_norm.eq_ignore_ascii_case("InnoDB")
                && !SYSTEM_SCHEMAS.contains(&schema.as_str())
        })
        .map(|(schema, table, engine)| NonTransactionalTable { schema, table, engine })
        .collect()
}

/// information_schema.tables 스캔 쿼리(대상 DB만, 시스템 스키마 이중 제외). meta.rs가 실행한다.
pub const SCAN_TABLES_SQL: &str = "SELECT table_schema, table_name, engine \
     FROM information_schema.tables \
     WHERE table_schema = ? \
       AND table_type = 'BASE TABLE' \
       AND engine IS NOT NULL \
       AND engine NOT IN ('InnoDB') \
       AND table_schema NOT IN ('mysql','information_schema','performance_schema','sys')";

/// §2.4 mojibake 추적용 charset/collation 조회 SQL. meta.rs가 실행한다.
pub const FETCH_CHARSET_SQL: &str = "SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME \
     FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?";

#[cfg(test)]
mod tests {
    use super::*;

    fn row(schema: &str, table: &str, engine: &str) -> (String, String, String) {
        (schema.to_string(), table.to_string(), engine.to_string())
    }

    #[test]
    fn t1_myisam_table_is_flagged() {
        let out = classify_non_transactional(vec![
            row("app", "users", "InnoDB"),
            row("app", "legacy_log", "MyISAM"),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].display(), "app.legacy_log(MyISAM)");
    }

    #[test]
    fn t2_all_innodb_yields_empty() {
        let out = classify_non_transactional(vec![
            row("app", "users", "InnoDB"),
            row("app", "orders", "innodb"), // 대소문자 무시
        ]);
        assert!(out.is_empty());
    }

    #[test]
    fn system_schemas_excluded() {
        let out = classify_non_transactional(vec![
            row("mysql", "user", "MyISAM"), // 시스템 스키마 → 제외
            row("app", "cache", "MEMORY"),  // 실제 대상 → 포함
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].schema, "app");
        assert_eq!(out[0].engine, "MEMORY");
    }

    #[test]
    fn null_or_empty_engine_excluded() {
        let out = classify_non_transactional(vec![
            row("app", "a_view", ""),
            row("app", "b_view", "NULL"),
            row("app", "real", "Aria"),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].engine, "Aria");
    }

    #[test]
    fn t3_db_name_system_schema_rejected() {
        assert!(matches!(
            ensure_not_system_schema("mysql"),
            Err(MySqlSourceError::SystemSchemaRejected(_))
        ));
        assert!(ensure_not_system_schema("app").is_ok());
    }
}
