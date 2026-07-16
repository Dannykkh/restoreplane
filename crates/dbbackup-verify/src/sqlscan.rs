// S09 9.5 — Restore-as-Exec 사전 스캐너(3차 방어선). 서명 게이트(1차)+샌드박스 격리(2차)에
// 더해, "정상 백업엔 절대 없을" 패턴을 시끄럽게 거부해 조기 경보를 준다(C3). 거짓음성 가능성을
// 전제로 한 보조 방어이며, 이것이 실패해도 곧바로 RCE로 이어지지 않도록 설계한다(심층 방어).

/// plain SQL/덤프 텍스트에서 위험 패턴을 탐지한다. 발견 시 복원 거부(Err).
/// custom 포맷은 pg_restore가 백슬래시 메타를 해석하지 않으므로 주 대상은 plain/MySQL SQL.
pub fn scan_dangerous_sql(bytes: &[u8]) -> anyhow::Result<()> {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "from program", // PG: COPY ... FROM PROGRAM
        "language plpythonu",
        "language plpython3u",
        "language plperlu",
        "language plr",
        "\n\\!", // psql backslash-bang(줄 시작)
        "\\! ",  // psql backslash-bang
        "load data local",
        "into outfile",
        "into dumpfile",
        "sys_exec",
        "sys_eval",
        "lo_import",
        "lo_export",
    ];
    for p in PATTERNS {
        if text.contains(p) {
            anyhow::bail!("위험 SQL 패턴 감지('{p}') — Restore-as-Exec 의심, 복원 거부(C3)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_copy_from_program() {
        assert!(scan_dangerous_sql(b"COPY x FROM PROGRAM 'touch /tmp/pwned'").is_err());
    }

    #[test]
    fn rejects_load_data_local() {
        assert!(scan_dangerous_sql(b"LOAD DATA LOCAL INFILE '/etc/passwd'").is_err());
    }

    #[test]
    fn rejects_backslash_bang() {
        assert!(scan_dangerous_sql(b"\\! rm -rf /").is_err());
    }

    #[test]
    fn accepts_clean_sql() {
        assert!(scan_dangerous_sql(b"CREATE TABLE t (id int); INSERT INTO t VALUES (1);").is_ok());
    }
}
