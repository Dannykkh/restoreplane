// tests/mysql_fatal_patterns.rs — R9/T15: MySqlSource::fatal_patterns()를 S05 PipelineCfg에 병합하면
//! exit=0이어도 DDL-race stderr가 FAIL로 강등되는지 확인한다. DB 불필요(순수 계약 검증).

use dbbackup_pipeline::{is_fatal_stderr, PipelineCfg};
use dbbackup_source_mysql::MySqlSource;

#[test]
fn ddl_race_pattern_merged_into_cfg_flags_fail() {
    let mut cfg = PipelineCfg::default();
    // 병합 전에는 MySQL DDL-race 문구가 치명 패턴으로 인식되지 않는다.
    let stderr = "mysqldump: Error 2013: Lost connection to MySQL server during query when dumping table `orders` at row: 512";
    assert!(!is_fatal_stderr(stderr, &cfg.fatal_patterns));

    // 잡 조립 시점의 병합(run 배선과 동일 지점).
    cfg.fatal_patterns.extend(MySqlSource::fatal_patterns());

    // 병합 후에는 exit code와 무관하게 FAIL로 강등된다(R9).
    assert!(is_fatal_stderr(stderr, &cfg.fatal_patterns));
}

#[test]
fn fatal_patterns_are_nonempty_and_cover_known_ddl_race_messages() {
    let p = MySqlSource::fatal_patterns();
    assert!(!p.is_empty());
    assert!(p.iter().any(|s| s.contains("Lost connection to MySQL server during query")));
    assert!(p.iter().any(|s| s.contains("SHOW CREATE TABLE")));
}
