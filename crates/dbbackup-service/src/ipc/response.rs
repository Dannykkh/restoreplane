use std::str::FromStr;

use dbbackup_application::{ApplicationFailure, ApplicationResponse, MAX_CANONICAL_PAYLOAD_BYTES};
use dbbackup_domain::IpcRequestId;
use serde_json::{json, Value};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{IpcError, PROTOCOL_VERSION};

const LENGTH_PREFIX_BYTES: usize = 4;
const MAGIC: [u8; 4] = *b"DBBR";
const RESPONSE_FIXED_HEADER_BYTES: usize = 32;
const MAX_RESPONSE_CODE_BYTES: usize = 64;
const FLAG_SUCCESS: u16 = 0b0001;
const FLAG_DEDUPLICATED: u16 = 0b0010;
const KNOWN_FLAGS: u16 = FLAG_SUCCESS | FLAG_DEDUPLICATED;
pub const MAX_RESPONSE_BODY_BYTES: usize =
    RESPONSE_FIXED_HEADER_BYTES + MAX_RESPONSE_CODE_BYTES + MAX_CANONICAL_PAYLOAD_BYTES;

#[derive(Clone, Debug, PartialEq)]
pub struct ResponseEnvelope {
    request_id: IpcRequestId,
    success: bool,
    deduplicated: bool,
    code: String,
    body: Value,
}

impl ResponseEnvelope {
    pub fn success(
        request_id: IpcRequestId,
        response: ApplicationResponse,
    ) -> Result<Self, IpcError> {
        let body = json!({
            "resource": response.resource,
            "body": response.body,
        });
        validate_response_body(&body)?;
        Ok(Self {
            request_id,
            success: true,
            deduplicated: response.deduplicated,
            code: "OK".to_owned(),
            body,
        })
    }

    pub fn failure(
        request_id: IpcRequestId,
        failure: ApplicationFailure,
    ) -> Result<Self, IpcError> {
        Self::failure_with_deduplication(request_id, failure, false)
    }

    pub fn failure_with_deduplication(
        request_id: IpcRequestId,
        failure: ApplicationFailure,
        deduplicated: bool,
    ) -> Result<Self, IpcError> {
        validate_failure_code(failure.code())?;
        let body = json!({
            "message": failure.message(),
            "retryable": failure.retryable(),
        });
        validate_response_body(&body)?;
        Ok(Self {
            request_id,
            success: false,
            deduplicated,
            code: failure.code().to_owned(),
            body,
        })
    }

    pub fn redacted_failure(
        request_id: IpcRequestId,
        code: &str,
        retryable: bool,
    ) -> Result<Self, IpcError> {
        validate_failure_code(code)?;
        let body = json!({
            "message": "The service could not complete the request.",
            "retryable": retryable,
        });
        validate_response_body(&body)?;
        Ok(Self {
            request_id,
            success: false,
            deduplicated: false,
            code: code.to_owned(),
            body,
        })
    }

    pub const fn request_id(&self) -> &IpcRequestId {
        &self.request_id
    }

    pub const fn is_success(&self) -> bool {
        self.success
    }

    pub const fn deduplicated(&self) -> bool {
        self.deduplicated
    }

    pub fn code(&self) -> &str {
        &self.code
    }

    pub const fn body(&self) -> &Value {
        &self.body
    }
}

pub fn encode_response(response: &ResponseEnvelope) -> Result<Vec<u8>, IpcError> {
    validate_code(&response.code, response.success)?;
    let payload =
        Zeroizing::new(serde_json::to_vec(&response.body).map_err(|_| IpcError::ResponseInvalid)?);
    if payload.len() > MAX_CANONICAL_PAYLOAD_BYTES {
        return Err(IpcError::PayloadTooLarge);
    }
    let code = response.code.as_bytes();
    let body_length = RESPONSE_FIXED_HEADER_BYTES + code.len() + payload.len();
    if body_length > MAX_RESPONSE_BODY_BYTES {
        return Err(IpcError::FrameTooLarge);
    }

    let mut flags = 0_u16;
    if response.success {
        flags |= FLAG_SUCCESS;
    }
    if response.deduplicated {
        flags |= FLAG_DEDUPLICATED;
    }

    let mut frame = Vec::with_capacity(LENGTH_PREFIX_BYTES + body_length);
    frame.extend_from_slice(&(body_length as u32).to_le_bytes());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    frame.extend_from_slice(&flags.to_le_bytes());
    frame.extend_from_slice(response.request_id.as_uuid().as_bytes());
    frame.extend_from_slice(&(code.len() as u16).to_le_bytes());
    frame.extend_from_slice(&0_u16.to_le_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    frame.extend_from_slice(code);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_response(frame: &[u8]) -> Result<ResponseEnvelope, IpcError> {
    if frame.len() < LENGTH_PREFIX_BYTES {
        return Err(IpcError::FrameTruncated);
    }
    let body_length = read_u32(frame, 0)? as usize;
    if body_length > MAX_RESPONSE_BODY_BYTES {
        return Err(IpcError::FrameTooLarge);
    }
    if body_length < RESPONSE_FIXED_HEADER_BYTES {
        return Err(IpcError::BodyLengthInvalid);
    }
    let expected_length = LENGTH_PREFIX_BYTES + body_length;
    if frame.len() < expected_length {
        return Err(IpcError::FrameTruncated);
    }
    if frame.len() != expected_length {
        return Err(IpcError::FrameLengthInvalid);
    }

    let body = &frame[LENGTH_PREFIX_BYTES..];
    if body[0..4] != MAGIC {
        return Err(IpcError::FrameMagicInvalid);
    }
    if read_u16(body, 4)? != PROTOCOL_VERSION {
        return Err(IpcError::ProtocolUnsupported);
    }
    let flags = read_u16(body, 6)?;
    if flags & !KNOWN_FLAGS != 0 {
        return Err(IpcError::FlagsUnsupported);
    }
    let success = flags & FLAG_SUCCESS != 0;
    let deduplicated = flags & FLAG_DEDUPLICATED != 0;
    let request_id = request_id_from_bytes(&body[8..24])?;
    let code_length = read_u16(body, 24)? as usize;
    if read_u16(body, 26)? != 0 {
        return Err(IpcError::FlagsUnsupported);
    }
    let payload_length = read_u32(body, 28)? as usize;
    if code_length == 0
        || code_length > MAX_RESPONSE_CODE_BYTES
        || payload_length > MAX_CANONICAL_PAYLOAD_BYTES
        || RESPONSE_FIXED_HEADER_BYTES + code_length + payload_length != body_length
    {
        return Err(IpcError::BodyLengthInvalid);
    }

    let code_start = RESPONSE_FIXED_HEADER_BYTES;
    let payload_start = code_start + code_length;
    let code = std::str::from_utf8(&body[code_start..payload_start])
        .map_err(|_| IpcError::ResponseInvalid)?
        .to_owned();
    validate_code(&code, success)?;
    let payload = Zeroizing::new(body[payload_start..payload_start + payload_length].to_vec());
    let payload =
        serde_json::from_slice::<Value>(&payload).map_err(|_| IpcError::ResponseInvalid)?;
    validate_response_body(&payload)?;

    Ok(ResponseEnvelope {
        request_id,
        success,
        deduplicated,
        code,
        body: payload,
    })
}

fn validate_response_body(body: &Value) -> Result<(), IpcError> {
    if !body.is_object() {
        return Err(IpcError::ResponseInvalid);
    }
    let length = serde_json::to_vec(body)
        .map_err(|_| IpcError::ResponseInvalid)?
        .len();
    if length > MAX_CANONICAL_PAYLOAD_BYTES {
        return Err(IpcError::PayloadTooLarge);
    }
    Ok(())
}

fn validate_code(code: &str, success: bool) -> Result<(), IpcError> {
    if success {
        if code != "OK" {
            return Err(IpcError::ResponseInvalid);
        }
    } else {
        validate_failure_code(code)?;
    }
    Ok(())
}

fn validate_failure_code(code: &str) -> Result<(), IpcError> {
    if code.is_empty()
        || code == "OK"
        || code.len() > MAX_RESPONSE_CODE_BYTES
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(IpcError::ResponseInvalid);
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
