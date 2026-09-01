use std::str::FromStr;

use restoreplane_application::{
    canonical_payload_bytes, ApplicationCommand, ApplicationError, MutationClass,
    MAX_CANONICAL_PAYLOAD_BYTES,
};
use restoreplane_domain::IpcRequestId;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;
use zeroize::Zeroizing;

pub const PROTOCOL_VERSION: u16 = 1;
pub const FIXED_HEADER_BYTES: usize = 48;
pub const MAX_COMMAND_BYTES: usize = 128;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const MAX_FRAME_BODY_BYTES: usize = FIXED_HEADER_BYTES
    + MAX_COMMAND_BYTES
    + MAX_IDEMPOTENCY_KEY_BYTES
    + MAX_CANONICAL_PAYLOAD_BYTES;
pub const MAX_REQUEST_TTL_MS: i64 = 5 * 60 * 1_000;
pub const MAX_CLOCK_SKEW_MS: i64 = 30 * 1_000;

const LENGTH_PREFIX_BYTES: usize = 4;
const MAGIC: [u8; 4] = *b"DBBK";

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("IPC_FRAME_TRUNCATED")]
    FrameTruncated,
    #[error("IPC_FRAME_TOO_LARGE")]
    FrameTooLarge,
    #[error("IPC_FRAME_LENGTH_INVALID")]
    FrameLengthInvalid,
    #[error("IPC_FRAME_MAGIC_INVALID")]
    FrameMagicInvalid,
    #[error("IPC_PROTOCOL_UNSUPPORTED")]
    ProtocolUnsupported,
    #[error("IPC_FLAGS_UNSUPPORTED")]
    FlagsUnsupported,
    #[error("IPC_REQUEST_ID_INVALID")]
    RequestIdInvalid,
    #[error("IPC_REQUEST_TIME_INVALID")]
    RequestTimeInvalid,
    #[error("IPC_REQUEST_EXPIRED")]
    RequestExpired,
    #[error("IPC_BODY_LENGTH_INVALID")]
    BodyLengthInvalid,
    #[error("IPC_COMMAND_INVALID")]
    CommandInvalid,
    #[error("IPC_IDEMPOTENCY_KEY_INVALID")]
    IdempotencyKeyInvalid,
    #[error("IPC_IDEMPOTENCY_REQUIRED")]
    IdempotencyRequired,
    #[error("IPC_PAYLOAD_INVALID")]
    PayloadInvalid,
    #[error("IPC_PAYLOAD_TOO_LARGE")]
    PayloadTooLarge,
    #[error("IPC_STREAM_CHUNK_TOO_LARGE")]
    StreamChunkTooLarge,
    #[error("IPC_RESPONSE_INVALID")]
    ResponseInvalid,
    #[error("IPC_IDENTITY_INVALID")]
    IdentityInvalid,
    #[error("IPC_UNAUTHORIZED")]
    Unauthorized,
}

impl IpcError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::FrameTruncated => "IPC_FRAME_TRUNCATED",
            Self::FrameTooLarge => "IPC_FRAME_TOO_LARGE",
            Self::FrameLengthInvalid => "IPC_FRAME_LENGTH_INVALID",
            Self::FrameMagicInvalid => "IPC_FRAME_MAGIC_INVALID",
            Self::ProtocolUnsupported => "IPC_PROTOCOL_UNSUPPORTED",
            Self::FlagsUnsupported => "IPC_FLAGS_UNSUPPORTED",
            Self::RequestIdInvalid => "IPC_REQUEST_ID_INVALID",
            Self::RequestTimeInvalid => "IPC_REQUEST_TIME_INVALID",
            Self::RequestExpired => "IPC_REQUEST_EXPIRED",
            Self::BodyLengthInvalid => "IPC_BODY_LENGTH_INVALID",
            Self::CommandInvalid => "IPC_COMMAND_INVALID",
            Self::IdempotencyKeyInvalid => "IPC_IDEMPOTENCY_KEY_INVALID",
            Self::IdempotencyRequired => "IPC_IDEMPOTENCY_REQUIRED",
            Self::PayloadInvalid => "IPC_PAYLOAD_INVALID",
            Self::PayloadTooLarge => "IPC_PAYLOAD_TOO_LARGE",
            Self::StreamChunkTooLarge => "IPC_STREAM_CHUNK_TOO_LARGE",
            Self::ResponseInvalid => "IPC_RESPONSE_INVALID",
            Self::IdentityInvalid => "IPC_IDENTITY_INVALID",
            Self::Unauthorized => "IPC_UNAUTHORIZED",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RequestEnvelope {
    protocol_version: u16,
    request_id: IpcRequestId,
    command: ApplicationCommand,
    idempotency_key: Option<String>,
    issued_at_ms: i64,
    expires_at_ms: i64,
    payload: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct RequestPreflight {
    protocol_version: u16,
    request_id: IpcRequestId,
    command: ApplicationCommand,
    idempotency_key: Option<String>,
    issued_at_ms: i64,
    expires_at_ms: i64,
    payload_start: usize,
    payload_length: usize,
    frame_length: usize,
}

impl RequestPreflight {
    pub(crate) const fn command(&self) -> ApplicationCommand {
        self.command
    }
}

impl RequestEnvelope {
    pub fn new(
        request_id: IpcRequestId,
        command: ApplicationCommand,
        idempotency_key: Option<String>,
        issued_at_ms: i64,
        expires_at_ms: i64,
        payload: Value,
    ) -> Result<Self, IpcError> {
        validate_times(issued_at_ms, expires_at_ms)?;
        validate_idempotency(command, idempotency_key.as_deref())?;
        canonical_payload_bytes(&payload).map_err(map_payload_error)?;
        Ok(Self {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            command,
            idempotency_key,
            issued_at_ms,
            expires_at_ms,
            payload,
        })
    }

    pub const fn protocol_version(&self) -> u16 {
        self.protocol_version
    }

    pub const fn request_id(&self) -> &IpcRequestId {
        &self.request_id
    }

    pub const fn command(&self) -> ApplicationCommand {
        self.command
    }

    pub fn idempotency_key(&self) -> Option<&str> {
        self.idempotency_key.as_deref()
    }

    pub const fn issued_at_ms(&self) -> i64 {
        self.issued_at_ms
    }

    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    pub const fn payload(&self) -> &Value {
        &self.payload
    }
}

pub fn encode_request(request: &RequestEnvelope) -> Result<Vec<u8>, IpcError> {
    if request.protocol_version != PROTOCOL_VERSION {
        return Err(IpcError::ProtocolUnsupported);
    }
    validate_times(request.issued_at_ms, request.expires_at_ms)?;
    validate_idempotency(request.command, request.idempotency_key.as_deref())?;

    let command = request.command.descriptor().name().as_bytes();
    if command.is_empty() || command.len() > MAX_COMMAND_BYTES {
        return Err(IpcError::CommandInvalid);
    }
    let idempotency_key = request.idempotency_key.as_deref().unwrap_or("").as_bytes();
    let payload =
        Zeroizing::new(canonical_payload_bytes(&request.payload).map_err(map_payload_error)?);
    let body_length = FIXED_HEADER_BYTES
        .checked_add(command.len())
        .and_then(|length| length.checked_add(idempotency_key.len()))
        .and_then(|length| length.checked_add(payload.len()))
        .ok_or(IpcError::FrameTooLarge)?;
    if body_length > MAX_FRAME_BODY_BYTES {
        return Err(IpcError::FrameTooLarge);
    }

    let mut frame = Vec::with_capacity(LENGTH_PREFIX_BYTES + body_length);
    frame.extend_from_slice(&(body_length as u32).to_le_bytes());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    frame.extend_from_slice(&0_u16.to_le_bytes());
    frame.extend_from_slice(request.request_id.as_uuid().as_bytes());
    frame.extend_from_slice(&request.issued_at_ms.to_le_bytes());
    frame.extend_from_slice(&request.expires_at_ms.to_le_bytes());
    frame.extend_from_slice(&(command.len() as u16).to_le_bytes());
    frame.extend_from_slice(&(idempotency_key.len() as u16).to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(command);
    frame.extend_from_slice(idempotency_key);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_request(frame: &[u8], now_ms: i64) -> Result<RequestEnvelope, IpcError> {
    let preflight = preflight_request(frame, now_ms)?;
    decode_preflighted_request(frame, preflight)
}

pub fn request_id_hint(frame: &[u8]) -> Result<IpcRequestId, IpcError> {
    if frame.len() < LENGTH_PREFIX_BYTES + 24 {
        return Err(IpcError::FrameTruncated);
    }
    if frame[LENGTH_PREFIX_BYTES..LENGTH_PREFIX_BYTES + 4] != MAGIC {
        return Err(IpcError::FrameMagicInvalid);
    }
    request_id_from_bytes(&frame[LENGTH_PREFIX_BYTES + 8..LENGTH_PREFIX_BYTES + 24])
}

pub(crate) fn preflight_request(frame: &[u8], now_ms: i64) -> Result<RequestPreflight, IpcError> {
    if frame.len() < LENGTH_PREFIX_BYTES {
        return Err(IpcError::FrameTruncated);
    }

    let declared_body_length = read_u32(frame, 0)? as usize;
    if declared_body_length > MAX_FRAME_BODY_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    if declared_body_length < FIXED_HEADER_BYTES {
        return Err(IpcError::BodyLengthInvalid);
    }
    if frame.len() < LENGTH_PREFIX_BYTES + FIXED_HEADER_BYTES {
        return Err(IpcError::FrameTruncated);
    }

    let body = &frame[LENGTH_PREFIX_BYTES..];
    if body[0..4] != MAGIC {
        return Err(IpcError::FrameMagicInvalid);
    }
    let protocol_version = read_u16(body, 4)?;
    if protocol_version != PROTOCOL_VERSION {
        return Err(IpcError::ProtocolUnsupported);
    }
    if read_u16(body, 6)? != 0 {
        return Err(IpcError::FlagsUnsupported);
    }

    let issued_at_ms = read_i64(body, 24)?;
    let expires_at_ms = read_i64(body, 32)?;
    validate_times(issued_at_ms, expires_at_ms)?;
    if issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
        return Err(IpcError::RequestTimeInvalid);
    }
    if now_ms >= expires_at_ms {
        return Err(IpcError::RequestExpired);
    }

    let expected_frame_length = LENGTH_PREFIX_BYTES
        .checked_add(declared_body_length)
        .ok_or(IpcError::FrameTooLarge)?;
    if frame.len() < expected_frame_length {
        return Err(IpcError::FrameTruncated);
    }
    if frame.len() != expected_frame_length {
        return Err(IpcError::FrameLengthInvalid);
    }

    let command_length = read_u16(body, 40)? as usize;
    let key_length = read_u16(body, 42)? as usize;
    let payload_length = read_u32(body, 44)? as usize;
    if command_length == 0
        || command_length > MAX_COMMAND_BYTES
        || key_length > MAX_IDEMPOTENCY_KEY_BYTES
        || payload_length > MAX_CANONICAL_PAYLOAD_BYTES
    {
        return Err(IpcError::BodyLengthInvalid);
    }
    let variable_length = command_length
        .checked_add(key_length)
        .and_then(|length| length.checked_add(payload_length))
        .ok_or(IpcError::BodyLengthInvalid)?;
    if FIXED_HEADER_BYTES + variable_length != declared_body_length {
        return Err(IpcError::BodyLengthInvalid);
    }

    let request_id = request_id_from_bytes(&body[8..24])?;
    let command_start = FIXED_HEADER_BYTES;
    let key_start = command_start + command_length;
    let payload_start = key_start + key_length;
    let command_name = std::str::from_utf8(&body[command_start..key_start])
        .map_err(|_| IpcError::CommandInvalid)?;
    let command =
        ApplicationCommand::try_from(command_name).map_err(|_| IpcError::CommandInvalid)?;

    let idempotency_key = if key_length == 0 {
        None
    } else {
        Some(
            std::str::from_utf8(&body[key_start..payload_start])
                .map_err(|_| IpcError::IdempotencyKeyInvalid)?
                .to_owned(),
        )
    };
    validate_idempotency(command, idempotency_key.as_deref())?;

    Ok(RequestPreflight {
        protocol_version,
        request_id,
        command,
        idempotency_key,
        issued_at_ms,
        expires_at_ms,
        payload_start: LENGTH_PREFIX_BYTES + payload_start,
        payload_length,
        frame_length: expected_frame_length,
    })
}

pub(crate) fn decode_preflighted_request(
    frame: &[u8],
    preflight: RequestPreflight,
) -> Result<RequestEnvelope, IpcError> {
    if frame.len() != preflight.frame_length {
        return Err(IpcError::FrameLengthInvalid);
    }
    let payload_end = preflight
        .payload_start
        .checked_add(preflight.payload_length)
        .ok_or(IpcError::BodyLengthInvalid)?;
    let payload_bytes = Zeroizing::new(frame[preflight.payload_start..payload_end].to_vec());
    let payload =
        serde_json::from_slice::<Value>(&payload_bytes).map_err(|_| IpcError::PayloadInvalid)?;
    canonical_payload_bytes(&payload).map_err(map_payload_error)?;

    Ok(RequestEnvelope {
        protocol_version: preflight.protocol_version,
        request_id: preflight.request_id,
        command: preflight.command,
        idempotency_key: preflight.idempotency_key,
        issued_at_ms: preflight.issued_at_ms,
        expires_at_ms: preflight.expires_at_ms,
        payload,
    })
}

fn validate_times(issued_at_ms: i64, expires_at_ms: i64) -> Result<(), IpcError> {
    if issued_at_ms < 0
        || expires_at_ms <= issued_at_ms
        || expires_at_ms.saturating_sub(issued_at_ms) > MAX_REQUEST_TTL_MS
    {
        return Err(IpcError::RequestTimeInvalid);
    }
    Ok(())
}

fn validate_idempotency(
    command: ApplicationCommand,
    idempotency_key: Option<&str>,
) -> Result<(), IpcError> {
    if command.descriptor().mutation() != MutationClass::ReadOnly && idempotency_key.is_none() {
        return Err(IpcError::IdempotencyRequired);
    }
    if let Some(key) = idempotency_key {
        if key.is_empty()
            || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
            || !key.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
        {
            return Err(IpcError::IdempotencyKeyInvalid);
        }
    }
    Ok(())
}

fn request_id_from_bytes(bytes: &[u8]) -> Result<IpcRequestId, IpcError> {
    let bytes: [u8; 16] = bytes.try_into().map_err(|_| IpcError::RequestIdInvalid)?;
    let uuid = Uuid::from_bytes(bytes);
    if uuid.is_nil() {
        return Err(IpcError::RequestIdInvalid);
    }
    IpcRequestId::from_str(&uuid.to_string()).map_err(|_| IpcError::RequestIdInvalid)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, IpcError> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or(IpcError::FrameTruncated)?
        .try_into()
        .map_err(|_| IpcError::FrameTruncated)?;
    Ok(u16::from_le_bytes(value))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, IpcError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(IpcError::FrameTruncated)?
        .try_into()
        .map_err(|_| IpcError::FrameTruncated)?;
    Ok(u32::from_le_bytes(value))
}

fn read_i64(bytes: &[u8], offset: usize) -> Result<i64, IpcError> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or(IpcError::FrameTruncated)?
        .try_into()
        .map_err(|_| IpcError::FrameTruncated)?;
    Ok(i64::from_le_bytes(value))
}

fn map_payload_error(error: ApplicationError) -> IpcError {
    match error {
        ApplicationError::PayloadTooLarge => IpcError::PayloadTooLarge,
        _ => IpcError::PayloadInvalid,
    }
}
