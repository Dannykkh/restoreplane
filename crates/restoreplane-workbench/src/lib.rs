//! Current-user database sessions. This crate never resolves backup-service secrets.
pub mod browse;
pub mod credentials;
pub mod data_movement;
pub mod driver;
pub mod edits;
pub mod events;
pub mod generated_sql;
pub mod journal;
pub mod metadata;
pub mod movement_codec;
pub mod movement_export;
pub mod policy;
pub mod protection;
pub mod query;
pub mod recovery;
pub mod result_stream;
pub mod saved_queries;
pub mod session;
pub mod store;
pub mod types;

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WorkbenchError {
    #[error("WB_FORBIDDEN")]
    Forbidden,
    #[error("WB_ELEVATED_UNSUPPORTED")]
    Elevated,
    #[error("WB_BINDING_UNPROVEN")]
    BindingUnproven,
    #[error("WB_BINDING_STALE")]
    BindingStale,
    #[error("WB_AUTH_EXPIRED")]
    AuthorizationExpired,
    #[error("WB_SERVICE_UNAVAILABLE")]
    ServiceUnavailable,
    #[error("WB_TARGET_BUSY")]
    TargetBusy,
    #[error("WB_CAPABILITY_UNSUPPORTED")]
    Unsupported,
    #[error("WB_SESSION_STALE")]
    SessionStale,
    #[error("WB_PAYLOAD_INVALID")]
    InvalidInput,
    #[error("WB_LIMIT")]
    Limit,
    #[error("WB_JOURNAL_UNAVAILABLE")]
    StoreUnavailable,
    #[error("WB_IO")]
    Io,
    #[error("WB_CANCEL_UNSETTLED")]
    CancelUnsettled,
    #[error("WB_STATEMENT_REJECTED")]
    StatementRejected(crate::driver::DriverTransactionState),
    #[error("WB_CAPABILITY_UNSUPPORTED")]
    EditRefused(String),
    #[error("WB_ROW_CONFLICT")]
    EditConflict(uuid::Uuid),
    #[error("WB_ATTEMPT_EXPIRED")]
    AttemptExpired,
    #[error("WB_IDEMPOTENCY_CONFLICT")]
    IdempotencyConflict,
    #[error("WB_OUTCOME_UNKNOWN")]
    OutcomeUnknown,
    #[error("WB_CURSOR_INVALID")]
    CursorInvalid,
    #[error("WB_RESULT_EXPIRED")]
    ResultExpired,
    #[error("WB_ACK_INVALID")]
    AckInvalid,
}

impl WorkbenchError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Forbidden => "WB_FORBIDDEN",
            Self::Elevated => "WB_ELEVATED_UNSUPPORTED",
            Self::BindingUnproven => "WB_BINDING_UNPROVEN",
            Self::BindingStale => "WB_BINDING_STALE",
            Self::AuthorizationExpired => "WB_AUTH_EXPIRED",
            Self::ServiceUnavailable => "WB_SERVICE_UNAVAILABLE",
            Self::TargetBusy => "WB_TARGET_BUSY",
            Self::Unsupported => "WB_CAPABILITY_UNSUPPORTED",
            Self::SessionStale => "WB_SESSION_STALE",
            Self::InvalidInput => "WB_PAYLOAD_INVALID",
            Self::Limit => "WB_LIMIT",
            Self::StoreUnavailable => "WB_JOURNAL_UNAVAILABLE",
            Self::Io => "WB_IO",
            Self::CancelUnsettled => "WB_CANCEL_UNSETTLED",
            Self::StatementRejected(_) => "WB_STATEMENT_REJECTED",
            Self::EditRefused(_) => "WB_CAPABILITY_UNSUPPORTED",
            Self::EditConflict(_) => "WB_ROW_CONFLICT",
            Self::AttemptExpired => "WB_ATTEMPT_EXPIRED",
            Self::IdempotencyConflict => "WB_IDEMPOTENCY_CONFLICT",
            Self::OutcomeUnknown => "WB_OUTCOME_UNKNOWN",
            Self::CursorInvalid => "WB_CURSOR_INVALID",
            Self::ResultExpired => "WB_RESULT_EXPIRED",
            Self::AckInvalid => "WB_ACK_INVALID",
        }
    }
}

pub use session::{
    BindingAuthorizer, IntegrityLevel, InvocationContext, NativePrincipal, WorkbenchState,
};
