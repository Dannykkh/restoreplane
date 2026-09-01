use zeroize::{Zeroize, Zeroizing};

use super::{decode_request, IpcError, RequestEnvelope, FIXED_HEADER_BYTES, MAX_FRAME_BODY_BYTES};

const LENGTH_PREFIX_BYTES: usize = 4;
pub const MAX_STREAM_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug)]
pub struct RequestStreamDecoder {
    buffer: Zeroizing<Vec<u8>>,
    wiped_bytes: usize,
}

impl Default for RequestStreamDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestStreamDecoder {
    pub fn new() -> Self {
        Self {
            buffer: Zeroizing::new(Vec::with_capacity(
                LENGTH_PREFIX_BYTES + MAX_FRAME_BODY_BYTES,
            )),
            wiped_bytes: 0,
        }
    }

    pub fn push(&mut self, input: &[u8], now_ms: i64) -> Result<Vec<RequestEnvelope>, IpcError> {
        if input.len() > MAX_STREAM_CHUNK_BYTES {
            self.wipe();
            return Err(IpcError::StreamChunkTooLarge);
        }

        let mut decoded = Vec::new();
        let mut cursor = 0;
        while cursor < input.len() {
            if self.buffer.len() < LENGTH_PREFIX_BYTES {
                let needed = LENGTH_PREFIX_BYTES - self.buffer.len();
                let take = needed.min(input.len() - cursor);
                self.buffer.extend_from_slice(&input[cursor..cursor + take]);
                cursor += take;
                if self.buffer.len() < LENGTH_PREFIX_BYTES {
                    continue;
                }
            }

            let frame_length = match self.expected_frame_length() {
                Ok(length) => length,
                Err(error) => {
                    self.wipe();
                    return Err(error);
                }
            };
            let needed = frame_length - self.buffer.len();
            let take = needed.min(input.len() - cursor);
            self.buffer.extend_from_slice(&input[cursor..cursor + take]);
            cursor += take;

            if self.buffer.len() == frame_length {
                let request = match decode_request(&self.buffer, now_ms) {
                    Ok(request) => request,
                    Err(error) => {
                        self.wipe();
                        return Err(error);
                    }
                };
                self.wipe();
                decoded.push(request);
            }
        }
        Ok(decoded)
    }

    pub fn finish(&mut self) -> Result<(), IpcError> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        self.wipe();
        Err(IpcError::FrameTruncated)
    }

    pub fn abort(&mut self) -> usize {
        let buffered = self.buffer.len();
        self.wipe();
        buffered
    }

    pub fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    pub const fn wiped_bytes(&self) -> usize {
        self.wiped_bytes
    }

    fn expected_frame_length(&self) -> Result<usize, IpcError> {
        let prefix: [u8; 4] = self.buffer[..4]
            .try_into()
            .map_err(|_| IpcError::FrameTruncated)?;
        let body_length = u32::from_le_bytes(prefix) as usize;
        if body_length > MAX_FRAME_BODY_BYTES {
            return Err(IpcError::FrameTooLarge);
        }
        if body_length < FIXED_HEADER_BYTES {
            return Err(IpcError::BodyLengthInvalid);
        }
        Ok(LENGTH_PREFIX_BYTES + body_length)
    }

    fn wipe(&mut self) {
        self.wiped_bytes = self.wiped_bytes.saturating_add(self.buffer.len());
        self.buffer.zeroize();
    }
}
