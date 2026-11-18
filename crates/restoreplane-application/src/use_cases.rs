use serde_json::Value;

use crate::{ApplicationCommand, ApplicationError, ApplicationResponse};

#[derive(Clone, Debug, PartialEq)]
pub struct ApplicationRequest {
    pub command: ApplicationCommand,
    pub payload: Value,
}

pub trait ApplicationUseCases: Send + Sync {
    fn execute(&self, request: ApplicationRequest)
        -> Result<ApplicationResponse, ApplicationError>;

    /// Only the authenticated dispatcher may supply the installation-owner hash.
    fn authorize_workbench_binding(
        &self,
        _payload: Value,
        _owner_sid_hash: &str,
        _accepted_at_ms: i64,
    ) -> Result<ApplicationResponse, ApplicationError> {
        Err(ApplicationError::Forbidden)
    }

    /// Existing Scope commands may consume a native authenticated owner for
    /// optional Workbench metadata. Direct callers cannot supply this via JSON.
    fn execute_workbench_protection(
        &self,
        _request: ApplicationRequest,
        _owner_sid_hash: &str,
        _accepted_at_ms: i64,
    ) -> Result<ApplicationResponse, ApplicationError> {
        Err(ApplicationError::Forbidden)
    }
}
