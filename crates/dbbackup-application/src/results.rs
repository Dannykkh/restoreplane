use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ApplicationError;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRef {
    pub resource_type: String,
    pub resource_id: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationResponse {
    pub deduplicated: bool,
    pub resource: Option<ResourceRef>,
    pub body: Value,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationFailure {
    code: String,
    message: String,
    retryable: bool,
}

impl ApplicationFailure {
    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn retryable(&self) -> bool {
        self.retryable
    }
}

impl From<&ApplicationError> for ApplicationFailure {
    fn from(error: &ApplicationError) -> Self {
        let retryable = matches!(error, ApplicationError::Internal);
        let message = match error {
            ApplicationError::UnknownCommand => "The command is not supported.",
            ApplicationError::InvalidPayload | ApplicationError::PayloadTooLarge => {
                "The request payload is invalid."
            }
            ApplicationError::Forbidden => "This command is not authorized.",
            ApplicationError::RehearsalOnly => {
                "Production execution is disabled until the durable gate passes."
            }
            ApplicationError::WorkItemFenceStale => {
                "The work item changed; refresh its current fencing token before retrying."
            }
            ApplicationError::OutcomeUnknown => {
                "The prior request may have completed. Inspect current state before trying again."
            }
            ApplicationError::PlanRevisionTargetNotReady => {
                "Every selected database must have a current successful connection probe."
            }
            ApplicationError::PlanRevisionEngineCapabilityUnavailable => {
                "A selected database engine lacks a current passed capability certification."
            }
            ApplicationError::PlanRevisionDestinationNotReady => {
                "Every destination needs current successful backup-writer and restore-reader probes."
            }
            ApplicationError::PlanRevisionVerificationUnsupported => {
                "The requested verification level is not supported for every selected engine."
            }
            ApplicationError::Internal => "The service could not complete the request.",
        };
        Self {
            code: error.code().to_owned(),
            message: message.to_owned(),
            retryable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_publish_preconditions_are_actionable_and_non_retryable() {
        let cases = [
            (
                ApplicationError::PlanRevisionTargetNotReady,
                "PLAN_REVISION_TARGET_NOT_READY",
            ),
            (
                ApplicationError::PlanRevisionEngineCapabilityUnavailable,
                "PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE",
            ),
            (
                ApplicationError::PlanRevisionDestinationNotReady,
                "PLAN_REVISION_DESTINATION_NOT_READY",
            ),
            (
                ApplicationError::PlanRevisionVerificationUnsupported,
                "PLAN_REVISION_VERIFICATION_UNSUPPORTED",
            ),
        ];
        for (error, code) in cases {
            let failure = ApplicationFailure::from(&error);
            assert_eq!(failure.code(), code);
            assert!(!failure.retryable());
            assert!(!failure.message().is_empty());
        }
    }
}
