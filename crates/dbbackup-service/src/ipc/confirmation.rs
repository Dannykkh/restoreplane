use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dbbackup_application::{canonical_payload_digest, ConfirmationAction};
use dbbackup_catalog::{ConfirmationSpec, IpcRepository, RepositoryError};
use dbbackup_domain::ConfirmationTokenId;
use rand::rngs::OsRng;
use rand::RngCore;
use serde_json::Value;
use subtle::ConstantTimeEq;
use thiserror::Error;
use zeroize::Zeroizing;

use super::{AuthenticatedPrincipal, PrincipalKind};

const TOKEN_BYTES: usize = 32;
pub(super) const MAX_CONFIRMATION_TTL_MS: i64 = 2 * 60 * 1_000;

#[derive(Debug, Error)]
pub enum ConfirmationServiceError {
    #[error("IPC_UNAUTHORIZED")]
    Unauthorized,
    #[error("CONFIRMATION_INPUT_INVALID")]
    InputInvalid,
    #[error("CONFIRMATION_TOKEN_INVALID_OR_REPLAYED")]
    InvalidOrReplayed,
    #[error("CONFIRMATION_TOKEN_GENERATION_FAILED")]
    TokenGenerationFailed,
    #[error("{0}")]
    Repository(RepositoryError),
}

impl ConfirmationServiceError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized => "IPC_UNAUTHORIZED",
            Self::InputInvalid => "CONFIRMATION_INPUT_INVALID",
            Self::InvalidOrReplayed => "CONFIRMATION_TOKEN_INVALID_OR_REPLAYED",
            Self::TokenGenerationFailed => "CONFIRMATION_TOKEN_GENERATION_FAILED",
            Self::Repository(error) => error.code(),
        }
    }
}

pub struct RawConfirmationToken(Zeroizing<Vec<u8>>);

impl RawConfirmationToken {
    pub(super) fn generate() -> Result<Self, ConfirmationServiceError> {
        let mut token_bytes = Zeroizing::new(vec![0_u8; TOKEN_BYTES]);
        OsRng
            .try_fill_bytes(&mut token_bytes)
            .map_err(|_| ConfirmationServiceError::TokenGenerationFailed)?;
        Ok(Self(token_bytes))
    }

    pub fn from_encoded(value: &str) -> Result<Self, ConfirmationServiceError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| ConfirmationServiceError::InvalidOrReplayed)?;
        if bytes.len() != TOKEN_BYTES {
            return Err(ConfirmationServiceError::InvalidOrReplayed);
        }
        Ok(Self(Zeroizing::new(bytes)))
    }

    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    pub fn encode(&self) -> Zeroizing<String> {
        Zeroizing::new(URL_SAFE_NO_PAD.encode(&self.0))
    }

    pub(super) fn digest_blake3(&self) -> String {
        blake3::hash(self.expose()).to_hex().to_string()
    }
}

pub struct IssuedConfirmation {
    id: ConfirmationTokenId,
    action: ConfirmationAction,
    expires_at_ms: i64,
    token: RawConfirmationToken,
}

impl IssuedConfirmation {
    pub const fn id(&self) -> &ConfirmationTokenId {
        &self.id
    }

    pub const fn action(&self) -> ConfirmationAction {
        self.action
    }

    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    pub const fn token(&self) -> &RawConfirmationToken {
        &self.token
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedConfirmation {
    id: ConfirmationTokenId,
    token_digest_blake3: String,
    payload_digest_blake3: String,
}

impl ValidatedConfirmation {
    pub const fn id(&self) -> &ConfirmationTokenId {
        &self.id
    }

    pub fn token_digest_blake3(&self) -> &str {
        &self.token_digest_blake3
    }

    pub fn payload_digest_blake3(&self) -> &str {
        &self.payload_digest_blake3
    }
}

#[derive(Clone)]
pub struct ConfirmationService {
    repository: IpcRepository,
}

impl ConfirmationService {
    pub const fn new(repository: IpcRepository) -> Self {
        Self { repository }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &self,
        principal: &AuthenticatedPrincipal,
        action: ConfirmationAction,
        resource_type: &str,
        resource_id: &str,
        resource_fingerprint: &str,
        payload: &Value,
        now_ms: i64,
        ttl_ms: i64,
    ) -> Result<IssuedConfirmation, ConfirmationServiceError> {
        ensure_destructive_principal(principal)?;
        if action == ConfirmationAction::RetentionDelete
            || now_ms < 0
            || ttl_ms <= 0
            || ttl_ms > MAX_CONFIRMATION_TTL_MS
            || resource_type.is_empty()
            || resource_id.is_empty()
            || resource_fingerprint.is_empty()
        {
            return Err(ConfirmationServiceError::InputInvalid);
        }
        let expires_at_ms = now_ms
            .checked_add(ttl_ms)
            .ok_or(ConfirmationServiceError::InputInvalid)?;
        let payload_digest = canonical_payload_digest(payload)
            .map_err(|_| ConfirmationServiceError::InputInvalid)?;

        let token = RawConfirmationToken::generate()?;
        let token_digest = token.digest_blake3();
        let id = ConfirmationTokenId::new();
        self.repository
            .issue_confirmation(ConfirmationSpec::new(
                id.clone(),
                token_digest,
                action.as_str(),
                principal.sid_hash(),
                resource_type,
                resource_id,
                resource_fingerprint,
                payload_digest,
                now_ms,
                expires_at_ms,
            ))
            .map_err(map_repository)?;

        Ok(IssuedConfirmation {
            id,
            action,
            expires_at_ms,
            token,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn validate(
        &self,
        principal: &AuthenticatedPrincipal,
        confirmation_token_id: &ConfirmationTokenId,
        action: ConfirmationAction,
        resource_type: &str,
        resource_id: &str,
        resource_fingerprint: &str,
        payload: &Value,
        token: &RawConfirmationToken,
        now_ms: i64,
    ) -> Result<ValidatedConfirmation, ConfirmationServiceError> {
        ensure_destructive_principal(principal)?;
        let payload_digest = canonical_payload_digest(payload)
            .map_err(|_| ConfirmationServiceError::InvalidOrReplayed)?;
        let record = self
            .repository
            .confirmation(confirmation_token_id)
            .map_err(map_repository)?;
        let presented_digest = blake3::hash(token.expose()).to_hex().to_string();
        let token_matches = bool::from(
            presented_digest
                .as_bytes()
                .ct_eq(record.token_digest_blake3().as_bytes()),
        );

        if !token_matches
            || record.state() != "issued"
            || record.action() != action.as_str()
            || record.client_sid_hash() != principal.sid_hash()
            || record.resource_type() != resource_type
            || record.resource_id() != resource_id
            || record.resource_fingerprint() != resource_fingerprint
            || record.payload_digest_blake3() != payload_digest
            || now_ms < record.issued_at_ms()
            || now_ms >= record.expires_at_ms()
        {
            return Err(ConfirmationServiceError::InvalidOrReplayed);
        }

        Ok(ValidatedConfirmation {
            id: record.id().clone(),
            token_digest_blake3: presented_digest,
            payload_digest_blake3: payload_digest,
        })
    }

    pub fn revoke(
        &self,
        principal: &AuthenticatedPrincipal,
        confirmation_token_id: &ConfirmationTokenId,
        now_ms: i64,
    ) -> Result<(), ConfirmationServiceError> {
        ensure_destructive_principal(principal)?;
        self.repository
            .revoke_confirmation(confirmation_token_id, principal.sid_hash(), now_ms)
            .map_err(map_repository)
    }

    pub fn expire(&self, now_ms: i64) -> Result<usize, ConfirmationServiceError> {
        self.repository
            .expire_confirmations(now_ms)
            .map_err(map_repository)
    }
}

fn ensure_destructive_principal(
    principal: &AuthenticatedPrincipal,
) -> Result<(), ConfirmationServiceError> {
    if matches!(
        principal.kind(),
        PrincipalKind::Owner | PrincipalKind::ApprovedAdministrator
    ) {
        Ok(())
    } else {
        Err(ConfirmationServiceError::Unauthorized)
    }
}

fn map_repository(error: RepositoryError) -> ConfirmationServiceError {
    match error {
        RepositoryError::NotFound | RepositoryError::ConfirmationInvalidOrReplayed => {
            ConfirmationServiceError::InvalidOrReplayed
        }
        RepositoryError::SecurityInputInvalid => ConfirmationServiceError::InputInvalid,
        other => ConfirmationServiceError::Repository(other),
    }
}
