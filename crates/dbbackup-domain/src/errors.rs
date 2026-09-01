use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DomainError {
    #[error("DOMAIN_ID_INVALID")]
    InvalidId { type_name: &'static str },
    #[error("DOMAIN_ID_NONCANONICAL")]
    NonCanonicalId { type_name: &'static str },
    #[error("DOMAIN_ENUM_INVALID")]
    InvalidEnum { enum_name: &'static str },
    #[error("DOMAIN_STATE_TRANSITION_INVALID")]
    InvalidTransition {
        machine: &'static str,
        current: &'static str,
        command: &'static str,
    },
    #[error("PLAN_REVISION_TARGET_REQUIRED")]
    PlanRevisionTargetRequired,
    #[error("PLAN_REVISION_DESTINATION_REQUIRED")]
    PlanRevisionDestinationRequired,
    #[error("PLAN_REVISION_REQUIRED_DESTINATION_REQUIRED")]
    PlanRevisionRequiredDestinationRequired,
    #[error("PLAN_REVISION_TARGET_DUPLICATE")]
    PlanRevisionTargetDuplicate,
    #[error("PLAN_REVISION_DESTINATION_DUPLICATE")]
    PlanRevisionDestinationDuplicate,
    #[error("PLAN_REVISION_PUBLISHED_AT_INVALID")]
    PlanRevisionPublishedAtInvalid,
    #[error("PLAN_REVISION_VERIFICATION_UNSUPPORTED")]
    PlanRevisionVerificationUnsupported,
    #[error("PLAN_REVISION_TARGET_NOT_READY")]
    PlanRevisionTargetNotReady,
    #[error("PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE")]
    PlanRevisionEngineCapabilityUnavailable,
    #[error("PLAN_REVISION_DESTINATION_NOT_READY")]
    PlanRevisionDestinationNotReady,
    #[error("PLAN_REVISION_IMMUTABLE")]
    PlanRevisionImmutable,
    #[error("OCCURRENCE_ORIGIN_INVALID")]
    OccurrenceOriginInvalid,
    #[error("OCCURRENCE_SCHEDULED_TIME_INVALID")]
    OccurrenceScheduledTimeInvalid,
    #[error("OCCURRENCE_IDEMPOTENCY_KEY_INVALID")]
    OccurrenceIdempotencyKeyInvalid,
}

impl DomainError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => "DOMAIN_ID_INVALID",
            Self::NonCanonicalId { .. } => "DOMAIN_ID_NONCANONICAL",
            Self::InvalidEnum { .. } => "DOMAIN_ENUM_INVALID",
            Self::InvalidTransition { .. } => "DOMAIN_STATE_TRANSITION_INVALID",
            Self::PlanRevisionTargetRequired => "PLAN_REVISION_TARGET_REQUIRED",
            Self::PlanRevisionDestinationRequired => "PLAN_REVISION_DESTINATION_REQUIRED",
            Self::PlanRevisionRequiredDestinationRequired => {
                "PLAN_REVISION_REQUIRED_DESTINATION_REQUIRED"
            }
            Self::PlanRevisionTargetDuplicate => "PLAN_REVISION_TARGET_DUPLICATE",
            Self::PlanRevisionDestinationDuplicate => "PLAN_REVISION_DESTINATION_DUPLICATE",
            Self::PlanRevisionPublishedAtInvalid => "PLAN_REVISION_PUBLISHED_AT_INVALID",
            Self::PlanRevisionVerificationUnsupported => "PLAN_REVISION_VERIFICATION_UNSUPPORTED",
            Self::PlanRevisionTargetNotReady => "PLAN_REVISION_TARGET_NOT_READY",
            Self::PlanRevisionEngineCapabilityUnavailable => {
                "PLAN_REVISION_ENGINE_CAPABILITY_UNAVAILABLE"
            }
            Self::PlanRevisionDestinationNotReady => "PLAN_REVISION_DESTINATION_NOT_READY",
            Self::PlanRevisionImmutable => "PLAN_REVISION_IMMUTABLE",
            Self::OccurrenceOriginInvalid => "OCCURRENCE_ORIGIN_INVALID",
            Self::OccurrenceScheduledTimeInvalid => "OCCURRENCE_SCHEDULED_TIME_INVALID",
            Self::OccurrenceIdempotencyKeyInvalid => "OCCURRENCE_IDEMPOTENCY_KEY_INVALID",
        }
    }
}
