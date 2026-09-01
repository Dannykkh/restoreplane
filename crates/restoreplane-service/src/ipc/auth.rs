use std::fmt;
use std::sync::{Arc, Mutex};

use restoreplane_application::{AccessClass, ApplicationCommand};

use super::{decode_preflighted_request, preflight_request, IpcError, RequestEnvelope};

const MAX_SID_STRING_BYTES: usize = 184;
const MAX_SID_SUB_AUTHORITIES: usize = 15;

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WindowsSid(String);

impl WindowsSid {
    pub fn parse(value: &str) -> Result<Self, IpcError> {
        if value.len() > MAX_SID_STRING_BYTES || !is_canonical_sid(value) {
            return Err(IpcError::IdentityInvalid);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WindowsSid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WindowsSid([redacted])")
    }
}

#[derive(Clone, Debug)]
pub struct TokenIdentity {
    user_sid: WindowsSid,
    logon_sid: WindowsSid,
    session_id: u32,
    elevated_admin: bool,
    local: bool,
}

impl TokenIdentity {
    pub const fn new(
        user_sid: WindowsSid,
        logon_sid: WindowsSid,
        session_id: u32,
        elevated_admin: bool,
        local: bool,
    ) -> Self {
        Self {
            user_sid,
            logon_sid,
            session_id,
            elevated_admin,
            local,
        }
    }

    pub const fn user_sid(&self) -> &WindowsSid {
        &self.user_sid
    }

    pub const fn logon_sid(&self) -> &WindowsSid {
        &self.logon_sid
    }

    pub const fn session_id(&self) -> u32 {
        self.session_id
    }

    pub const fn elevated_admin(&self) -> bool {
        self.elevated_admin
    }

    pub const fn local(&self) -> bool {
        self.local
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrincipalKind {
    Service,
    Owner,
    ApprovedAdministrator,
}

#[derive(Clone, Debug)]
pub struct AuthenticatedPrincipal {
    kind: PrincipalKind,
    sid_hash: String,
    session_id: u32,
    max_access: AccessClass,
}

impl AuthenticatedPrincipal {
    pub const fn kind(&self) -> PrincipalKind {
        self.kind
    }

    pub fn sid_hash(&self) -> &str {
        &self.sid_hash
    }

    pub const fn session_id(&self) -> u32 {
        self.session_id
    }
}

#[derive(Clone, Debug)]
pub struct AuthorizationPolicy {
    service_sid: WindowsSid,
    owner_sid: WindowsSid,
    owner_session: OwnerSessionBinding,
    approved_administrator_sids: Vec<WindowsSid>,
}

#[derive(Clone, Debug)]
enum OwnerSessionBinding {
    Fixed {
        logon_sid: WindowsSid,
        session_id: u32,
    },
    FirstLocal(Arc<Mutex<Option<(WindowsSid, u32)>>>),
}

impl AuthorizationPolicy {
    pub fn new(
        service_sid: WindowsSid,
        owner_sid: WindowsSid,
        owner_logon_sid: WindowsSid,
        owner_session_id: u32,
        approved_administrator_sids: Vec<WindowsSid>,
    ) -> Result<Self, IpcError> {
        Self::build(
            service_sid,
            owner_sid,
            OwnerSessionBinding::Fixed {
                logon_sid: owner_logon_sid,
                session_id: owner_session_id,
            },
            approved_administrator_sids,
        )
    }

    pub fn new_unbound_owner(
        service_sid: WindowsSid,
        owner_sid: WindowsSid,
        approved_administrator_sids: Vec<WindowsSid>,
    ) -> Result<Self, IpcError> {
        Self::build(
            service_sid,
            owner_sid,
            OwnerSessionBinding::FirstLocal(Arc::new(Mutex::new(None))),
            approved_administrator_sids,
        )
    }

    fn build(
        service_sid: WindowsSid,
        owner_sid: WindowsSid,
        owner_session: OwnerSessionBinding,
        mut approved_administrator_sids: Vec<WindowsSid>,
    ) -> Result<Self, IpcError> {
        if service_sid == owner_sid
            || approved_administrator_sids
                .iter()
                .any(|sid| sid == &service_sid || sid == &owner_sid)
        {
            return Err(IpcError::IdentityInvalid);
        }
        approved_administrator_sids.sort();
        approved_administrator_sids.dedup();
        Ok(Self {
            service_sid,
            owner_sid,
            owner_session,
            approved_administrator_sids,
        })
    }

    pub fn authenticate(
        &self,
        identity: &TokenIdentity,
    ) -> Result<AuthenticatedPrincipal, IpcError> {
        if !identity.local {
            return Err(IpcError::Unauthorized);
        }

        let kind = if identity.user_sid == self.service_sid {
            PrincipalKind::Service
        } else if identity.user_sid == self.owner_sid && self.owner_session_matches(identity)? {
            PrincipalKind::Owner
        } else if identity.elevated_admin
            && self
                .approved_administrator_sids
                .binary_search(&identity.user_sid)
                .is_ok()
        {
            PrincipalKind::ApprovedAdministrator
        } else {
            return Err(IpcError::Unauthorized);
        };

        Ok(AuthenticatedPrincipal {
            kind,
            sid_hash: blake3::hash(identity.user_sid.as_str().as_bytes())
                .to_hex()
                .to_string(),
            session_id: identity.session_id,
            max_access: AccessClass::Admin,
        })
    }

    fn owner_session_matches(&self, identity: &TokenIdentity) -> Result<bool, IpcError> {
        match &self.owner_session {
            OwnerSessionBinding::Fixed {
                logon_sid,
                session_id,
            } => Ok(identity.logon_sid == *logon_sid && identity.session_id == *session_id),
            OwnerSessionBinding::FirstLocal(binding) => {
                let mut binding = binding.lock().map_err(|_| IpcError::Unauthorized)?;
                match binding.as_ref() {
                    Some((logon_sid, session_id)) => {
                        Ok(identity.logon_sid == *logon_sid && identity.session_id == *session_id)
                    }
                    None => {
                        *binding = Some((identity.logon_sid.clone(), identity.session_id));
                        Ok(true)
                    }
                }
            }
        }
    }

    pub fn authorize_command(
        &self,
        principal: &AuthenticatedPrincipal,
        command: ApplicationCommand,
    ) -> Result<(), IpcError> {
        if access_rank(command.descriptor().access()) > access_rank(principal.max_access) {
            return Err(IpcError::Unauthorized);
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct AuthorizedRequest {
    principal: AuthenticatedPrincipal,
    request: RequestEnvelope,
    accepted_at_ms: i64,
}

impl AuthorizedRequest {
    pub const fn principal(&self) -> &AuthenticatedPrincipal {
        &self.principal
    }

    pub const fn request(&self) -> &RequestEnvelope {
        &self.request
    }

    pub const fn accepted_at_ms(&self) -> i64 {
        self.accepted_at_ms
    }
}

pub fn authorize_and_decode(
    policy: &AuthorizationPolicy,
    identity: &TokenIdentity,
    frame: &[u8],
    now_ms: i64,
) -> Result<AuthorizedRequest, IpcError> {
    let principal = policy.authenticate(identity)?;
    let preflight = preflight_request(frame, now_ms)?;
    policy.authorize_command(&principal, preflight.command())?;
    let request = decode_preflighted_request(frame, preflight)?;
    Ok(AuthorizedRequest {
        principal,
        request,
        accepted_at_ms: now_ms,
    })
}

const fn access_rank(access: AccessClass) -> u8 {
    match access {
        AccessClass::Read => 0,
        AccessClass::Operate => 1,
        AccessClass::Admin => 2,
    }
}

fn is_canonical_sid(value: &str) -> bool {
    let mut parts = value.split('-');
    if parts.next() != Some("S") || parts.next() != Some("1") {
        return false;
    }
    let Some(identifier_authority) = parts.next() else {
        return false;
    };
    if !is_canonical_decimal(identifier_authority)
        || identifier_authority.parse::<u64>().ok().is_none()
    {
        return false;
    }

    let sub_authorities: Vec<_> = parts.collect();
    if sub_authorities.is_empty() || sub_authorities.len() > MAX_SID_SUB_AUTHORITIES {
        return false;
    }
    sub_authorities
        .iter()
        .all(|part| is_canonical_decimal(part) && part.parse::<u32>().ok().is_some())
}

fn is_canonical_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}
