// schema_version 마이그레이션 스캐폴드 검증 — 마이그레이션 0개 경로가 깨지지 않는지,
// meta 부재/미래 버전이 명확한 에러로 거부되는지.
use restoreplane_config::{migrate::load_and_migrate, ConfigError};

const MINIMAL_V1: &str = r#"
[meta]
schema_version = 1

[policy]
spool_dir = "spool"
"#;

#[test]
fn v1_passes_through_with_zero_migrations() {
    let config = load_and_migrate(MINIMAL_V1).expect("v1 config must load without migrations");
    assert_eq!(config.meta.schema_version, 1);
}

#[test]
fn missing_meta_is_rejected() {
    let raw = r#"
[policy]
spool_dir = "spool"
"#;
    let err = load_and_migrate(raw).unwrap_err();
    assert!(matches!(err, ConfigError::MissingSchemaVersion));
}

#[test]
fn future_schema_version_is_rejected() {
    let raw = r#"
[meta]
schema_version = 99

[policy]
spool_dir = "spool"
"#;
    let err = load_and_migrate(raw).unwrap_err();
    match err {
        ConfigError::UnsupportedSchemaVersion {
            found,
            max_supported,
        } => {
            assert_eq!(found, 99);
            assert_eq!(max_supported, 1);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }
}

#[test]
fn unknown_top_level_key_is_rejected() {
    // deny_unknown_fields — 오타/오배치 필드를 조용히 무시하지 않는다.
    let raw = r#"
[meta]
schema_version = 1

[policy]
spool_dir = "spool"

[totally_unknown_table]
x = 1
"#;
    assert!(load_and_migrate(raw).is_err());
}

#[test]
fn unknown_engine_value_is_rejected() {
    let raw = r#"
[meta]
schema_version = 1

[[database]]
alias = "a"
engine = "oracle"
host = "h"
port = 1521
dbname = "d"
user = "u"
password = "enc:1:aGVsbG8="

[policy]
spool_dir = "spool"
"#;
    assert!(load_and_migrate(raw).is_err());
}
