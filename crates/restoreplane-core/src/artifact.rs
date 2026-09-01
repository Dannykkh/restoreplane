use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactPartRole {
    DatabaseDump,
    Globals,
    Metadata,
    Wal,
    Auxiliary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsistencyGrade {
    CrashConsistent,
    ApplicationConsistent,
    TransactionConsistent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PartSpec {
    part_id: Uuid,
    role: ArtifactPartRole,
    logical_name: String,
    required: bool,
    dump_format: String,
}

impl PartSpec {
    pub fn new(
        part_id: Uuid,
        role: ArtifactPartRole,
        logical_name: impl Into<String>,
        required: bool,
        dump_format: impl Into<String>,
    ) -> Result<Self, ArtifactContractError> {
        let value = Self {
            part_id,
            role,
            logical_name: logical_name.into(),
            required,
            dump_format: dump_format.into(),
        };
        value.validate()?;
        Ok(value)
    }

    pub const fn part_id(&self) -> Uuid {
        self.part_id
    }

    pub const fn role(&self) -> ArtifactPartRole {
        self.role
    }

    pub fn logical_name(&self) -> &str {
        &self.logical_name
    }

    pub const fn required(&self) -> bool {
        self.required
    }

    pub fn dump_format(&self) -> &str {
        &self.dump_format
    }

    fn validate(&self) -> Result<(), ArtifactContractError> {
        if self.part_id.is_nil()
            || self.logical_name.is_empty()
            || self.logical_name.len() > 512
            || self.logical_name.contains(['\0', '\r', '\n'])
            || self.dump_format.is_empty()
            || self.dump_format.len() > 64
            || !self
                .dump_format
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(ArtifactContractError::PartInvalid);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactSpec {
    consistency_grade: ConsistencyGrade,
    parts: Vec<PartSpec>,
}

impl ArtifactSpec {
    pub fn new(
        consistency_grade: ConsistencyGrade,
        parts: Vec<PartSpec>,
    ) -> Result<Self, ArtifactContractError> {
        if parts.is_empty() || parts.len() > 1_024 {
            return Err(ArtifactContractError::PartInvalid);
        }
        let mut ids = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut required_database = false;
        for part in &parts {
            part.validate()?;
            if !ids.insert(part.part_id) || !names.insert(part.logical_name.as_str()) {
                return Err(ArtifactContractError::PartIdentityDuplicate);
            }
            required_database |= part.role == ArtifactPartRole::DatabaseDump && part.required;
        }
        if !required_database {
            return Err(ArtifactContractError::DatabasePartRequired);
        }
        Ok(Self {
            consistency_grade,
            parts,
        })
    }

    pub const fn consistency_grade(&self) -> ConsistencyGrade {
        self.consistency_grade
    }

    pub fn parts(&self) -> &[PartSpec] {
        &self.parts
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArtifactContractError {
    #[error("ARTIFACT_PART_INVALID")]
    PartInvalid,
    #[error("ARTIFACT_PART_IDENTITY_DUPLICATE")]
    PartIdentityDuplicate,
    #[error("ARTIFACT_DATABASE_PART_REQUIRED")]
    DatabasePartRequired,
}

impl ArtifactContractError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PartInvalid => "ARTIFACT_PART_INVALID",
            Self::PartIdentityDuplicate => "ARTIFACT_PART_IDENTITY_DUPLICATE",
            Self::DatabasePartRequired => "ARTIFACT_DATABASE_PART_REQUIRED",
        }
    }
}
