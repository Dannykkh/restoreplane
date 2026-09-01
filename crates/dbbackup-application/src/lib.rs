// Connected Backup Platform application boundary.

mod authority;
mod commands;
mod error;
mod g1_evidence;
mod payload;
mod results;
mod use_cases;

pub use authority::{enforce_authority, AuthoritySnapshot, CutoverMode, ExecutionAuthority};
pub use commands::{
    AccessClass, ApplicationCommand, CommandDescriptor, ConfirmationAction, MutationClass,
};
pub use error::ApplicationError;
pub use g1_evidence::{BuildIdentityV1, G1EvidenceError, VerifiedG1EvidenceV1};
pub use payload::{canonical_payload_bytes, canonical_payload_digest, MAX_CANONICAL_PAYLOAD_BYTES};
pub use results::{ApplicationFailure, ApplicationResponse, ResourceRef};
pub use use_cases::{ApplicationRequest, ApplicationUseCases};
