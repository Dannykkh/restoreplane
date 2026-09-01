use serde_json::Value;

use crate::ApplicationError;

pub const MAX_CANONICAL_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_PAYLOAD_DEPTH: usize = 64;

pub fn canonical_payload_digest(payload: &Value) -> Result<String, ApplicationError> {
    let bytes = canonical_payload_bytes(payload)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub fn canonical_payload_bytes(payload: &Value) -> Result<Vec<u8>, ApplicationError> {
    if !payload.is_object() {
        return Err(ApplicationError::InvalidPayload);
    }

    let mut output = Vec::new();
    write_canonical(payload, &mut output, 0)?;
    if output.len() > MAX_CANONICAL_PAYLOAD_BYTES {
        return Err(ApplicationError::PayloadTooLarge);
    }
    Ok(output)
}

fn write_canonical(
    value: &Value,
    output: &mut Vec<u8>,
    depth: usize,
) -> Result<(), ApplicationError> {
    if depth > MAX_PAYLOAD_DEPTH {
        return Err(ApplicationError::InvalidPayload);
    }

    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => {
            output.extend_from_slice(if *value { b"true" } else { b"false" });
        }
        Value::Number(value) => {
            if value.as_i64().is_none() && value.as_u64().is_none() {
                return Err(ApplicationError::InvalidPayload);
            }
            output.extend_from_slice(value.to_string().as_bytes());
        }
        Value::String(value) => {
            serde_json::to_writer(output, value).map_err(|_| ApplicationError::InvalidPayload)?;
        }
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical(value, output, depth + 1)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                serde_json::to_writer(&mut *output, key)
                    .map_err(|_| ApplicationError::InvalidPayload)?;
                output.push(b':');
                write_canonical(value, output, depth + 1)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}
