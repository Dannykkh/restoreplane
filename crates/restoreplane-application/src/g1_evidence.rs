use std::collections::BTreeSet;

use chrono::DateTime;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_EVIDENCE_BYTES: usize = 1024 * 1024;
const MAX_REQUIREMENTS_BYTES: usize = 64 * 1024;
const MAX_MATRIX_BYTES: usize = 256 * 1024;
const REQUIRED_G1_EVIDENCE_COUNT: usize = 9;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildIdentityV1 {
    source_revision: String,
    source_tree_digest: String,
    build_id: String,
}

impl BuildIdentityV1 {
    pub fn new(
        source_revision: impl Into<String>,
        source_tree_digest: impl Into<String>,
        build_id: impl Into<String>,
    ) -> Result<Self, G1EvidenceError> {
        let identity = Self {
            source_revision: source_revision.into(),
            source_tree_digest: source_tree_digest.into(),
            build_id: build_id.into(),
        };
        if !is_source_revision(&identity.source_revision)
            || !is_digest(&identity.source_tree_digest)
            || !is_digest(&identity.build_id)
        {
            return Err(G1EvidenceError::Invalid);
        }
        Ok(identity)
    }

    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    pub fn source_tree_digest(&self) -> &str {
        &self.source_tree_digest
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedG1EvidenceV1 {
    source_revision: String,
    source_tree_digest: String,
    build_id: String,
    evidence_digest_blake3: String,
    runner_os: String,
    required_evidence_count: usize,
}

impl VerifiedG1EvidenceV1 {
    pub fn verify(
        evidence_json: &str,
        requirements_json: &str,
        matrix_json: &str,
        expected_build: &BuildIdentityV1,
    ) -> Result<Self, G1EvidenceError> {
        if evidence_json.is_empty()
            || evidence_json.len() > MAX_EVIDENCE_BYTES
            || requirements_json.is_empty()
            || requirements_json.len() > MAX_REQUIREMENTS_BYTES
            || matrix_json.is_empty()
            || matrix_json.len() > MAX_MATRIX_BYTES
        {
            return Err(G1EvidenceError::Invalid);
        }
        let requirements: G1RequirementsV1 =
            serde_json::from_str(requirements_json).map_err(|_| G1EvidenceError::Invalid)?;
        let matrix: G1MatrixV1 =
            serde_json::from_str(matrix_json).map_err(|_| G1EvidenceError::Invalid)?;
        let evidence: G1EvidenceDocumentV1 =
            serde_json::from_str(evidence_json).map_err(|_| G1EvidenceError::Invalid)?;

        validate_requirements(&requirements)?;
        validate_matrix(&matrix, &requirements)?;
        validate_evidence(&evidence, &requirements, &matrix, expected_build)?;

        let canonical = serde_json::to_vec(&evidence).map_err(|_| G1EvidenceError::Invalid)?;
        Ok(Self {
            source_revision: evidence.source_revision,
            source_tree_digest: evidence.source_tree_digest,
            build_id: evidence.build_id,
            evidence_digest_blake3: blake3::hash(&canonical).to_hex().to_string(),
            runner_os: evidence.runner.os,
            required_evidence_count: requirements.required_evidence_ids.len(),
        })
    }

    pub fn source_revision(&self) -> &str {
        &self.source_revision
    }

    pub fn source_tree_digest(&self) -> &str {
        &self.source_tree_digest
    }

    pub fn build_id(&self) -> &str {
        &self.build_id
    }

    pub fn evidence_digest_blake3(&self) -> &str {
        &self.evidence_digest_blake3
    }

    pub fn runner_os(&self) -> &str {
        &self.runner_os
    }

    pub const fn required_evidence_count(&self) -> usize {
        self.required_evidence_count
    }
}

#[derive(Debug, Error)]
pub enum G1EvidenceError {
    #[error("G1_EVIDENCE_INVALID")]
    Invalid,
}

impl G1EvidenceError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid => "G1_EVIDENCE_INVALID",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct G1RequirementsV1 {
    schema_version: u32,
    gate_id: String,
    same_source_revision: bool,
    same_build_id: bool,
    all_required: bool,
    required_evidence_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct G1MatrixV1 {
    schema_version: u32,
    gate_id: String,
    commands: Vec<G1MatrixCommandV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct G1MatrixCommandV1 {
    id: String,
    display: String,
    executable: String,
    arguments: Vec<String>,
    platform: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct G1EvidenceDocumentV1 {
    schema_version: u32,
    gate_id: String,
    source_revision: String,
    source_tree_digest: String,
    build_id: String,
    started_at: String,
    finished_at: String,
    runner: G1RunnerV1,
    evidence: Vec<G1EvidenceRowV1>,
    failure_count: u32,
    status: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct G1RunnerV1 {
    os: String,
    architecture: String,
    powershell: String,
    rustc: String,
    cargo: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct G1EvidenceRowV1 {
    id: String,
    command: String,
    status: String,
    exit_code: i32,
    started_at: String,
    finished_at: String,
    duration_ms: f64,
    source_revision: String,
    source_tree_digest: String,
    build_id: String,
    runner_os: String,
}

fn validate_requirements(requirements: &G1RequirementsV1) -> Result<(), G1EvidenceError> {
    let unique = requirements
        .required_evidence_ids
        .iter()
        .collect::<BTreeSet<_>>();
    if requirements.schema_version != 1
        || requirements.gate_id != "G1"
        || !requirements.same_source_revision
        || !requirements.same_build_id
        || !requirements.all_required
        || requirements.required_evidence_ids.len() != REQUIRED_G1_EVIDENCE_COUNT
        || unique.len() != requirements.required_evidence_ids.len()
        || requirements
            .required_evidence_ids
            .iter()
            .any(|id| id.trim().is_empty())
    {
        return Err(G1EvidenceError::Invalid);
    }
    Ok(())
}

fn validate_matrix(
    matrix: &G1MatrixV1,
    requirements: &G1RequirementsV1,
) -> Result<(), G1EvidenceError> {
    if matrix.schema_version != 1
        || matrix.gate_id != "G1"
        || matrix.commands.len() != requirements.required_evidence_ids.len()
    {
        return Err(G1EvidenceError::Invalid);
    }
    for (command, expected_id) in matrix
        .commands
        .iter()
        .zip(&requirements.required_evidence_ids)
    {
        if &command.id != expected_id
            || command.display.trim().is_empty()
            || command.executable != "cargo"
            || command.arguments.is_empty()
            || command.arguments.iter().any(|argument| argument.is_empty())
            || command.platform != "windows"
        {
            return Err(G1EvidenceError::Invalid);
        }
    }
    Ok(())
}

fn validate_evidence(
    evidence: &G1EvidenceDocumentV1,
    requirements: &G1RequirementsV1,
    matrix: &G1MatrixV1,
    expected_build: &BuildIdentityV1,
) -> Result<(), G1EvidenceError> {
    if evidence.schema_version != 1
        || evidence.gate_id != "G1"
        || evidence.status != "pass"
        || evidence.failure_count != 0
        || evidence.source_revision != expected_build.source_revision
        || evidence.source_tree_digest != expected_build.source_tree_digest
        || evidence.build_id != expected_build.build_id
        || evidence.evidence.len() != requirements.required_evidence_ids.len()
        || !valid_time_range(&evidence.started_at, &evidence.finished_at)
        || [
            evidence.runner.os.as_str(),
            evidence.runner.architecture.as_str(),
            evidence.runner.powershell.as_str(),
            evidence.runner.rustc.as_str(),
            evidence.runner.cargo.as_str(),
        ]
        .iter()
        .any(|value| value.trim().is_empty())
    {
        return Err(G1EvidenceError::Invalid);
    }

    let mut seen = BTreeSet::new();
    for ((row, expected_id), command) in evidence
        .evidence
        .iter()
        .zip(&requirements.required_evidence_ids)
        .zip(&matrix.commands)
    {
        if &row.id != expected_id
            || row.id != command.id
            || row.command != command.display
            || row.status != "pass"
            || row.exit_code != 0
            || !row.duration_ms.is_finite()
            || row.duration_ms < 0.0
            || row.source_revision != evidence.source_revision
            || row.source_tree_digest != evidence.source_tree_digest
            || row.build_id != evidence.build_id
            || row.runner_os != evidence.runner.os
            || !valid_time_range(&row.started_at, &row.finished_at)
            || !seen.insert(row.id.as_str())
        {
            return Err(G1EvidenceError::Invalid);
        }
    }
    Ok(())
}

fn valid_time_range(started_at: &str, finished_at: &str) -> bool {
    let Ok(started_at) = DateTime::parse_from_rfc3339(started_at) else {
        return false;
    };
    let Ok(finished_at) = DateTime::parse_from_rfc3339(finished_at) else {
        return false;
    };
    finished_at >= started_at
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_source_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
