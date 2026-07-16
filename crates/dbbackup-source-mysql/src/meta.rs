// crates/dbbackup-source-mysql/src/meta.rs
//! S04 §Reference Libraries — 런타임 메타 조회 seam. section-S04.md는 sqlx MySqlPool을 제안하지만,
//! "번들 heavy dep 최소화" 제약에 맞춰 sqlx를 도입하지 않고 MySqlMetaConn 트레이트로 추상화한다
//! (섹션 Risk & Rollback가 명시한 교체 가능 seam). 구체 구현 MysqlCliMetaConn은 tokio::process로
//! `mysql` 클라이언트를 구동해 탭 구분 출력을 파싱한다 — 크레이트 전체가 "CLI를 shell한다"는 결과
//! 지향과 일관되고 새 컴파일 의존이 없다. 실제 서버가 필요한 경로라 유닛 테스트는 하지 않는다.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use zeroize::Zeroizing;

use crate::engine_scan::{FETCH_CHARSET_SQL, SCAN_TABLES_SQL};

/// 엔진 스캔/버전 감지/charset 조회에 필요한 최소 계약. sqlx ↔ mysql_async ↔ CLI 교체 지점.
#[async_trait]
pub trait MySqlMetaConn: Send + Sync {
    /// SELECT VERSION() 원문.
    async fn query_version(&self) -> anyhow::Result<String>;
    /// information_schema.tables 스캔 결과 (schema, table, engine) 행 목록.
    async fn scan_tables(&self, db_name: &str) -> anyhow::Result<Vec<(String, String, String)>>;
    /// (charset, collation).
    async fn fetch_charset(&self, db_name: &str) -> anyhow::Result<(String, String)>;
}

/// `mysql` CLI 기반 구현(런타임 전용). 비밀번호는 MYSQL_PWD env로만 전달한다(R10).
pub struct MysqlCliMetaConn {
    pub client: PathBuf, // `mysql` 실행 파일 경로
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Zeroizing<String>,
}

impl MysqlCliMetaConn {
    /// -N(헤더 없음) -B(탭 구분) -e "<sql>"로 조회하고 stdout을 반환한다.
    /// 파라미터 바인딩이 없으므로 db_name은 호출부에서 검증된 식별자만 사용한다(system schema 거부 등).
    async fn run_sql(&self, sql: &str) -> anyhow::Result<String> {
        let out = tokio::process::Command::new(&self.client)
            .arg(format!("--host={}", self.host))
            .arg(format!("--port={}", self.port))
            .arg(format!("--user={}", self.user))
            .args(["-N", "-B", "-e", sql])
            .env("MYSQL_PWD", self.password.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("mysql 클라이언트 실행 실패({}): {e}", self.client.display()))?;
        if !out.status.success() {
            anyhow::bail!(
                "mysql 메타 조회 실패(exit={:?}): {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// SQL의 `?` 플레이스홀더를 리터럴 식별자로 치환한다(CLI는 prepared 바인딩이 없음).
/// db_name은 호출부(plan)에서 system schema 거부를 이미 통과한 식별자다.
fn inline_db(sql: &str, db_name: &str) -> String {
    sql.replacen('?', &format!("'{}'", db_name.replace('\'', "''")), 1)
}

#[async_trait]
impl MySqlMetaConn for MysqlCliMetaConn {
    async fn query_version(&self) -> anyhow::Result<String> {
        Ok(self.run_sql("SELECT VERSION()").await?.trim().to_string())
    }

    async fn scan_tables(&self, db_name: &str) -> anyhow::Result<Vec<(String, String, String)>> {
        let out = self.run_sql(&inline_db(SCAN_TABLES_SQL, db_name)).await?;
        Ok(out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let mut it = l.split('\t');
                Some((it.next()?.to_string(), it.next()?.to_string(), it.next()?.to_string()))
            })
            .collect())
    }

    async fn fetch_charset(&self, db_name: &str) -> anyhow::Result<(String, String)> {
        let out = self.run_sql(&inline_db(FETCH_CHARSET_SQL, db_name)).await?;
        let line = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let mut it = line.split('\t');
        let charset = it.next().unwrap_or("").to_string();
        let collation = it.next().unwrap_or("").to_string();
        Ok((charset, collation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_db_substitutes_placeholder_and_escapes_quotes() {
        let sql = inline_db(SCAN_TABLES_SQL, "app");
        assert!(sql.contains("table_schema = 'app'"));
        assert!(!sql.contains("= ?"));
        // 작은따옴표 이스케이프.
        assert!(inline_db("WHERE x = ?", "o'brien").contains("'o''brien'"));
    }
}
