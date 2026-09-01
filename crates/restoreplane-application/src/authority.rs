use serde::{Deserialize, Serialize};

use crate::{ApplicationCommand, ApplicationError};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAuthority {
    LegacyDirect,
    Service,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CutoverMode {
    RehearsalOnly,
    Production,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthoritySnapshot {
    execution_authority: ExecutionAuthority,
    cutover_mode: CutoverMode,
    g1_proven: bool,
    cutover_marker_present: bool,
}

impl AuthoritySnapshot {
    pub const fn new(
        execution_authority: ExecutionAuthority,
        cutover_mode: CutoverMode,
        g1_proven: bool,
        cutover_marker_present: bool,
    ) -> Self {
        Self {
            execution_authority,
            cutover_mode,
            g1_proven,
            cutover_marker_present,
        }
    }

    pub const fn execution_authority(self) -> ExecutionAuthority {
        self.execution_authority
    }

    pub const fn cutover_mode(self) -> CutoverMode {
        self.cutover_mode
    }

    pub const fn g1_proven(self) -> bool {
        self.g1_proven
    }

    pub const fn cutover_marker_present(self) -> bool {
        self.cutover_marker_present
    }
}

pub fn enforce_authority(
    authority: &AuthoritySnapshot,
    command: ApplicationCommand,
) -> Result<(), ApplicationError> {
    if !command.descriptor().requires_g1() {
        return Ok(());
    }

    if !authority.g1_proven {
        return Err(ApplicationError::RehearsalOnly);
    }

    if command == ApplicationCommand::ConfigCutover {
        return Ok(());
    }

    if authority.execution_authority != ExecutionAuthority::Service
        || authority.cutover_mode != CutoverMode::Production
        || !authority.cutover_marker_present
    {
        return Err(ApplicationError::RehearsalOnly);
    }

    Ok(())
}
