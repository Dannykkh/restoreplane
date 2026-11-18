//! Ephemeral version hints. The SQLite journal remains the durable authority.
use crate::WorkbenchError;
use restoreplane_domain::workbench::DecimalU64;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationHint {
    pub operation_id: Uuid,
    pub version: DecimalU64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HintAction {
    IgnoreOld,
    ApplyNext,
    PullRequired,
}
pub struct VersionTracker {
    versions: HashMap<Uuid, u64>,
    capacity: usize,
}
impl VersionTracker {
    pub fn new(capacity: usize) -> Result<Self, WorkbenchError> {
        if capacity == 0 || capacity > 100_000 {
            return Err(WorkbenchError::Limit);
        }
        Ok(Self {
            versions: HashMap::new(),
            capacity,
        })
    }
    pub fn observe(&mut self, hint: &OperationHint) -> Result<HintAction, WorkbenchError> {
        if hint.operation_id.is_nil() || hint.version.value() == 0 {
            return Err(WorkbenchError::InvalidInput);
        }
        let prior = self.versions.get(&hint.operation_id).copied().unwrap_or(0);
        if hint.version.value() <= prior {
            return Ok(HintAction::IgnoreOld);
        }
        // A hint contains no projection to apply. Even an adjacent hint must
        // pull its authoritative snapshot before the stored version advances.
        if hint.version.value() != prior.saturating_add(1) {
            return Ok(HintAction::PullRequired);
        }
        Ok(HintAction::ApplyNext)
    }
    pub fn applied_snapshot(
        &mut self,
        operation_id: Uuid,
        version: u64,
    ) -> Result<(), WorkbenchError> {
        if operation_id.is_nil() || version == 0 {
            return Err(WorkbenchError::InvalidInput);
        }
        if !self.versions.contains_key(&operation_id) && self.versions.len() >= self.capacity {
            return Err(WorkbenchError::Limit);
        }
        let existing = self.versions.entry(operation_id).or_insert(0);
        *existing = (*existing).max(version);
        Ok(())
    }
    /// The caller pulls operation.list/get after reconnect, never replays SQL.
    pub fn reconnect(&mut self) {
        self.versions.clear();
    }
}
