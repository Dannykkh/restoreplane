// 스키마-예시 드리프트 방지: config/*.example.toml이 항상 최신 스키마로 로드되는지 고정.
use std::path::PathBuf;

use dbbackup_config::{prune::PruneProfile, validate, Config};

fn example_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("config")
        .join(name)
}

#[test]
fn example_config_loads_and_validates() {
    let config = Config::load(&example_path("dbbackup.example.toml"))
        .expect("dbbackup.example.toml must deserialize into Config");
    validate(&config).expect("dbbackup.example.toml must pass validate()");

    assert_eq!(config.databases.len(), 2);
    assert_eq!(config.tunnels.len(), 1);
    assert_eq!(config.destinations.len(), 3);
    assert_eq!(config.jobs.len(), 2);
    assert!(config.notify.is_some());
}

#[test]
fn example_prune_profile_loads() {
    let profile = PruneProfile::load(&example_path("prune.example.toml"))
        .expect("prune.example.toml must deserialize into PruneProfile");
    assert_eq!(profile.destination, "s3-primary");
}

#[test]
fn config_serialization_roundtrip_is_stable() {
    let config = Config::load(&example_path("dbbackup.example.toml")).unwrap();

    let first = toml::to_string_pretty(&config).expect("Config must serialize to TOML");
    let reparsed: Config = toml::from_str(&first).expect("serialized Config must reparse");
    let second = toml::to_string_pretty(&reparsed).expect("reparsed Config must serialize again");

    assert_eq!(
        first, second,
        "serialize -> parse -> serialize must be a fixed point"
    );
}

#[test]
fn empty_config_roundtrips() {
    let config = Config::empty();
    let raw = toml::to_string_pretty(&config).unwrap();
    let back: Config = toml::from_str(&raw).unwrap();
    assert_eq!(back.meta.schema_version, config.meta.schema_version);
    assert!(back.databases.is_empty());
    assert!(back.jobs.is_empty());
}

#[test]
fn load_strips_utf8_bom() {
    // 메모장 등으로 저장돼 UTF-8 BOM이 붙은 config 파일도 정상 파싱돼야 한다.
    let dir = std::env::temp_dir().join("dbbackup-config-bom-test");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bom.toml");
    let body = "\u{FEFF}[meta]\nschema_version = 1\n\n[policy]\nspool_dir = \"spool\"\n";
    std::fs::write(&path, body).unwrap();

    let config = Config::load(&path).expect("BOM-prefixed config must load");
    assert_eq!(config.meta.schema_version, 1);
}
