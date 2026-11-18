use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApplicationError {
    #[error("APPLICATION_COMMAND_UNKNOWN")]
    UnknownCommand,
    #[error("APPLICATION_PAYLOAD_INVALID")]
    InvalidPayload,
    #[error("APPLICATION_PAYLOAD_TOO_LARGE")]
    PayloadTooLarge,
    #[error("APPLICATION_FORBIDDEN")]
    Forbidden,
    #[error("APPLICATION_REHEARSAL_ONLY")]
    RehearsalOnly,
    #[error("WORK_ITEM_FENCE_STALE")]
    WorkItemFenceStale,
    #[error("APPLICATION_OUTCOME_UNKNOWN")]
    OutcomeUnknown,
    #[error("PLAN_REVISION_TARGET_NOT_READY")]
    PlanRevisionTargetNotReady,
    #[error("PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE")]
    PlanRevisionEngineCapabilityUnavailable,
    #[error("PLAN_REVISION_DESTINATION_NOT_READY")]
    PlanRevisionDestinationNotReady,
    #[error("PLAN_REVISION_VERIFICATION_UNSUPPORTED")]
    PlanRevisionVerificationUnsupported,
    #[error("APPLICATION_INTERNAL")]
    Internal,
    #[error("WB_BINDING_UNPROVEN")]
    WorkbenchBindingUnproven,
    #[error("WB_BINDING_STALE")]
    WorkbenchBindingStale,
    #[error("WB_TARGET_BUSY")]
    WorkbenchTargetBusy,
}

impl ApplicationError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownCommand => "APPLICATION_COMMAND_UNKNOWN",
            Self::InvalidPayload => "APPLICATION_PAYLOAD_INVALID",
            Self::PayloadTooLarge => "APPLICATION_PAYLOAD_TOO_LARGE",
            Self::Forbidden => "APPLICATION_FORBIDDEN",
            Self::RehearsalOnly => "APPLICATION_REHEARSAL_ONLY",
            Self::WorkItemFenceStale => "WORK_ITEM_FENCE_STALE",
            Self::OutcomeUnknown => "APPLICATION_OUTCOME_UNKNOWN",
            Self::PlanRevisionTargetNotReady => "PLAN_REVISION_TARGET_NOT_READY",
            Self::PlanRevisionEngineCapabilityUnavailable => {
                "PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE"
            }
            Self::PlanRevisionDestinationNotReady => "PLAN_REVISION_DESTINATION_NOT_READY",
            Self::PlanRevisionVerificationUnsupported => "PLAN_REVISION_VERIFICATION_UNSUPPORTED",
            Self::Internal => "APPLICATION_INTERNAL",
            Self::WorkbenchBindingUnproven => "WB_BINDING_UNPROVEN",
            Self::WorkbenchBindingStale => "WB_BINDING_STALE",
            Self::WorkbenchTargetBusy => "WB_TARGET_BUSY",
        }
    }
}
