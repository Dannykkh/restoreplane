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
}
