//! Additive metadata carried by existing backup Scope commands. No SQL or
//! credential may be placed in this contract.
use crate::workbench::{DecimalU64, Engine};
use crate::{ConnectionProfileId, RestoreRunId, WorkItemId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_SELECTED_TABLES: usize = 100;
pub const MAX_SELECTION_BYTES: usize = 64 * 1024;

/// Returned only from an authenticated completed restore's native evidence.
/// A database name or the source fingerprint alone is never this identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkbenchRestoreProof {
    pub restore_work_item_id: WorkItemId,
    pub restore_run_id: RestoreRunId,
    pub connection_profile_id: ConnectionProfileId,
    pub database_name: String,
    pub engine: Engine,
    pub server_identity: Option<String>,
    pub actual_database_identity: String,
    pub completed_at_ms: DecimalU64,
}
impl WorkbenchRestoreProof {
    pub fn is_well_formed(&self) -> bool {
        if !valid_text(&self.database_name, 2048)
            || !valid_text(&self.actual_database_identity, 512)
            || self.completed_at_ms.value() == 0
        {
            return false;
        }
        match self.engine {
            Engine::Sqlite => {
                self.server_identity.is_none()
                    && self
                        .actual_database_identity
                        .strip_prefix("sqlite-file-object-id:")
                        .is_some_and(|id| valid_text(id, 256))
            }
            Engine::Postgresql => {
                self.server_identity
                    .as_deref()
                    .and_then(|id| id.strip_prefix("postgresql-system-id:"))
                    .is_some_and(canonical_positive_u64)
                    && self
                        .actual_database_identity
                        .strip_prefix("postgresql-database-oid:")
                        .is_some_and(canonical_positive_u64)
            }
            _ => false,
        }
    }
}
fn canonical_positive_u64(value: &str) -> bool {
    value
        .parse::<u64>()
        .is_ok_and(|n| n > 0 && n.to_string() == value)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkbenchSelectionItem {
    pub engine_object_id: String,
    pub schema_name: String,
    pub object_name: String,
    pub structure_signature: String,
}
impl WorkbenchSelectionItem {
    pub fn is_well_formed(&self) -> bool {
        valid_text(&self.engine_object_id, 2048)
            && valid_text(&self.schema_name, 256)
            && valid_text(&self.object_name, 256)
            && is_digest(&self.structure_signature)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkbenchSelection {
    pub signature_version: u32,
    pub expected_descriptor_revision: String,
    pub items: Vec<WorkbenchSelectionItem>,
}
impl WorkbenchSelection {
    /// The receiving application also checks the actual encoded 64KiB cap.
    pub fn is_well_formed(&self) -> bool {
        self.signature_version == 1
            && is_digest(&self.expected_descriptor_revision)
            && valid_items(&self.items)
    }
    pub fn canonical_items(&self) -> Vec<WorkbenchSelectionItem> {
        let mut items = self.items.clone();
        items.sort();
        items
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkbenchSelectionProof {
    pub selection_digest: String,
    pub descriptor_revision: String,
    pub items: Vec<WorkbenchSelectionItem>,
    pub verified_at_ms: DecimalU64,
}
impl WorkbenchSelectionProof {
    pub fn is_well_formed(&self) -> bool {
        is_digest(&self.selection_digest)
            && is_digest(&self.descriptor_revision)
            && valid_items(&self.items)
    }
}

fn valid_items(items: &[WorkbenchSelectionItem]) -> bool {
    !items.is_empty()
        && items.len() <= MAX_SELECTED_TABLES
        && items.iter().all(WorkbenchSelectionItem::is_well_formed)
        && items
            .iter()
            .map(|item| item.engine_object_id.as_str())
            .collect::<BTreeSet<_>>()
            .len()
            == items.len()
        && items
            .iter()
            .map(|item| (&item.schema_name, &item.object_name))
            .collect::<BTreeSet<_>>()
            .len()
            == items.len()
}
fn valid_text(value: &str, maximum: usize) -> bool {
    !value.trim().is_empty() && value.len() <= maximum && !value.chars().any(char::is_control)
}
fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
