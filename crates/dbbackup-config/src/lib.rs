// crates/dbbackup-config/src/lib.rs
pub mod migrate;
pub mod prune;
pub mod schema;
pub mod shadow;
pub mod validate;

use std::path::PathBuf;

pub use schema::Config;
pub use shadow::{
    load_v1_shadow_model, CanonicalConnection, CanonicalDestination, CanonicalPlan,
    CanonicalSecretBinding, CanonicalTarget, CanonicalV1ShadowModel, ImportedSecretRef,
    LegacySecretSink, ShadowImportError,
};
pub use validate::{validate, ValidationError};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("cannot parse TOML: {0}")]
    Parse(#[source] toml::de::Error),
    #[error("cannot deserialize config: {0}")]
    Deserialize(#[source] toml::de::Error),
    #[error("cannot serialize config: {0}")]
    Serialize(#[source] toml::ser::Error),
    #[error("config is missing [meta] schema_version")]
    MissingSchemaVersion,
    #[error("config schema_version {found} is newer than this binary supports (max {max_supported}); upgrade dbbackup")]
    UnsupportedSchemaVersion { found: u32, max_supported: u32 },
}
