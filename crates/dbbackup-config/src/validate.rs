// crates/dbbackup-config/src/validate.rs
use std::collections::HashSet;

use dbbackup_core::DestKind;

use crate::schema::Config;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("duplicate database alias: {0}")]
    DuplicateDatabaseAlias(String),
    #[error("duplicate destination name: {0}")]
    DuplicateDestinationName(String),
    #[error("duplicate ssh tunnel name: {0}")]
    DuplicateTunnelName(String),
    #[error("duplicate job name: {0}")]
    DuplicateJobName(String),
    #[error("job '{job}' references unknown source database '{source_db}'")]
    UnknownSourceRef { job: String, source_db: String },
    #[error("job '{job}' references unknown destination '{dest}'")]
    UnknownDestinationRef { job: String, dest: String },
    #[error("database '{db}' references unknown ssh tunnel '{tunnel}'")]
    UnknownTunnelRef { db: String, tunnel: String },
    #[error("destination '{dest}' (ssh) references unknown ssh tunnel '{tunnel}'")]
    UnknownDestinationTunnelRef { dest: String, tunnel: String },
    #[error("destination '{0}' (s3) is missing required field '{1}'")]
    MissingS3Field(String, &'static str),
    #[error("destination '{0}' has object_lock set but no checksum_algo (S3 requires a checksum when Object Lock is enabled)")]
    S3ChecksumRequiredForObjectLock(String),
    #[error("destination '{0}' (ssh) is missing required field '{1}'")]
    MissingSshField(String, &'static str),
    #[error("destination '{0}' (local) is missing required field 'path'")]
    MissingLocalPath(String),
    #[error("job '{0}' verify.level must be 1, 2 or 3")]
    InvalidVerifyLevel(String),
}

/// 실패 즉시 반환하지 않고 발견한 모든 문제를 모아 반환한다 — `add-db`/`run` 양쪽에서
/// 재사용하며, 사용자가 한 번에 여러 오류를 고칠 수 있게 한다.
pub fn validate(config: &Config) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    let mut db_aliases = HashSet::new();
    for db in &config.databases {
        if !db_aliases.insert(db.alias.clone()) {
            errors.push(ValidationError::DuplicateDatabaseAlias(db.alias.clone()));
        }
    }

    let mut tunnel_names = HashSet::new();
    for t in &config.tunnels {
        if !tunnel_names.insert(t.name.clone()) {
            errors.push(ValidationError::DuplicateTunnelName(t.name.clone()));
        }
    }

    for db in &config.databases {
        if let Some(tunnel) = &db.tunnel {
            if !tunnel_names.contains(tunnel) {
                errors.push(ValidationError::UnknownTunnelRef {
                    db: db.alias.clone(),
                    tunnel: tunnel.clone(),
                });
            }
        }
    }

    let mut dest_names = HashSet::new();
    for dest in &config.destinations {
        if !dest_names.insert(dest.name.clone()) {
            errors.push(ValidationError::DuplicateDestinationName(dest.name.clone()));
        }

        match dest.kind {
            DestKind::S3 => {
                if dest.endpoint.is_none() {
                    errors.push(ValidationError::MissingS3Field(
                        dest.name.clone(),
                        "endpoint",
                    ));
                }
                if dest.bucket.is_none() {
                    errors.push(ValidationError::MissingS3Field(dest.name.clone(), "bucket"));
                }
                if dest.access_key_id.is_none() {
                    errors.push(ValidationError::MissingS3Field(
                        dest.name.clone(),
                        "access_key_id",
                    ));
                }
                if dest.secret_access_key.is_none() {
                    errors.push(ValidationError::MissingS3Field(
                        dest.name.clone(),
                        "secret_access_key",
                    ));
                }
                if dest.object_lock.is_some() && dest.checksum_algo.is_none() {
                    errors.push(ValidationError::S3ChecksumRequiredForObjectLock(
                        dest.name.clone(),
                    ));
                }
            }
            DestKind::Ssh => {
                if let Some(tunnel) = &dest.tunnel {
                    if !tunnel_names.contains(tunnel) {
                        errors.push(ValidationError::UnknownDestinationTunnelRef {
                            dest: dest.name.clone(),
                            tunnel: tunnel.clone(),
                        });
                    }
                }
                if dest.path.is_none() {
                    errors.push(ValidationError::MissingSshField(dest.name.clone(), "path"));
                }
                if dest.user.is_none() {
                    errors.push(ValidationError::MissingSshField(dest.name.clone(), "user"));
                }
                if dest.host_key_fingerprint.is_none() {
                    errors.push(ValidationError::MissingSshField(
                        dest.name.clone(),
                        "host_key_fingerprint",
                    ));
                }
            }
            DestKind::Local => {
                if dest.path.is_none() {
                    errors.push(ValidationError::MissingLocalPath(dest.name.clone()));
                }
            }
        }
    }

    let mut job_names = HashSet::new();
    for job in &config.jobs {
        if !job_names.insert(job.name.clone()) {
            errors.push(ValidationError::DuplicateJobName(job.name.clone()));
        }
        if !db_aliases.contains(&job.source) {
            errors.push(ValidationError::UnknownSourceRef {
                job: job.name.clone(),
                source_db: job.source.clone(),
            });
        }
        for dest in &job.destinations {
            if !dest_names.contains(dest) {
                errors.push(ValidationError::UnknownDestinationRef {
                    job: job.name.clone(),
                    dest: dest.clone(),
                });
            }
        }
        if !(1..=3).contains(&job.verify.level) {
            errors.push(ValidationError::InvalidVerifyLevel(job.name.clone()));
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}
