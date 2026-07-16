// crates/dbbackup-config/src/migrate.rs
use crate::{schema::{Config, CURRENT_SCHEMA_VERSION}, ConfigError};

/// v_n -> v_(n+1) 변환. 항상 raw toml::Value 레벨에서 동작해 Config 구조체가
/// 최신 스키마만 알면 되게 한다(deny_unknown_fields와 공존 가능).
pub type MigrationFn = fn(toml::Value) -> Result<toml::Value, ConfigError>;

/// (target_version, migration) 오름차순. v1은 최초 스키마라 마이그레이션이 없다 —
/// 이 배열이 비어 있어도 인프라(레지스트리 자체)는 항상 존재해야 한다는 것이 P2-1의 요지.
pub static MIGRATIONS: &[(u32, MigrationFn)] = &[
    // (2, migrate_v1_to_v2),  // 예시: v2가 생기면 여기 추가.
];

pub fn load_and_migrate(raw: &str) -> Result<Config, ConfigError> {
    let mut value: toml::Value = toml::from_str(raw).map_err(ConfigError::Parse)?;
    let mut version = read_schema_version(&value)?;

    if version > CURRENT_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchemaVersion {
            found: version,
            max_supported: CURRENT_SCHEMA_VERSION,
        });
    }

    for (target_version, migration) in MIGRATIONS {
        if version < *target_version {
            value = migration(value)?;
            version = *target_version;
        }
    }

    value.try_into::<Config>().map_err(ConfigError::Deserialize)
}

fn read_schema_version(value: &toml::Value) -> Result<u32, ConfigError> {
    value
        .get("meta")
        .and_then(|m| m.get("schema_version"))
        .and_then(|v| v.as_integer())
        .map(|v| v as u32)
        .ok_or(ConfigError::MissingSchemaVersion)
}
