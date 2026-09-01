use dbbackup_application::{
    ApplicationCommand, ApplicationError, ApplicationFailure, ApplicationResponse,
};
use dbbackup_domain::IpcRequestId;
use dbbackup_service::ipc::{
    decode_request, decode_response, encode_request, encode_response, RequestEnvelope,
    RequestStreamDecoder, ResponseEnvelope, FIXED_HEADER_BYTES, MAX_FRAME_BODY_BYTES,
    PROTOCOL_VERSION,
};
use serde_json::json;

#[test]
fn request_roundtrip_preserves_protocol_contract() {
    let request_id = IpcRequestId::new();
    let request = RequestEnvelope::new(
        request_id.clone(),
        ApplicationCommand::BackupEnqueue,
        Some("backup-2026-07-29".to_owned()),
        1_000,
        2_000,
        json!({"planId": "plan-1", "origin": "manual"}),
    )
    .expect("valid request");

    let encoded = encode_request(&request).expect("encode request");
    let decoded = decode_request(&encoded, 1_500).expect("decode request");

    assert_eq!(decoded.protocol_version(), PROTOCOL_VERSION);
    assert_eq!(decoded.request_id(), &request_id);
    assert_eq!(decoded.command(), ApplicationCommand::BackupEnqueue);
    assert_eq!(decoded.idempotency_key(), Some("backup-2026-07-29"));
    assert_eq!(decoded.issued_at_ms(), 1_000);
    assert_eq!(decoded.expires_at_ms(), 2_000);
    assert_eq!(
        decoded.payload(),
        &json!({"origin": "manual", "planId": "plan-1"})
    );
}

#[test]
fn version_size_time_and_body_length_fail_before_payload_decode() {
    let read = RequestEnvelope::new(
        IpcRequestId::new(),
        ApplicationCommand::ServiceHealth,
        None,
        1_000,
        2_000,
        json!({}),
    )
    .expect("valid read request");
    let encoded = encode_request(&read).expect("encode read request");

    let mut unknown_version = encoded.clone();
    unknown_version[8..10].copy_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
    unknown_version.truncate(FIXED_HEADER_BYTES + 4);
    assert_code(
        decode_request(&unknown_version, 1_500),
        "IPC_PROTOCOL_UNSUPPORTED",
    );

    let mut oversized = vec![0_u8; 4];
    oversized.copy_from_slice(&((MAX_FRAME_BODY_BYTES as u32) + 1).to_le_bytes());
    assert_code(decode_request(&oversized, 1_500), "IPC_FRAME_TOO_LARGE");

    let mut expired_invalid_json = encoded.clone();
    let payload_index = encoded.len() - 2;
    expired_invalid_json[payload_index..].copy_from_slice(b"??");
    assert_code(
        decode_request(&expired_invalid_json, 2_000),
        "IPC_REQUEST_EXPIRED",
    );

    let mut invalid_body_length = encoded.clone();
    let declared_payload = u32::from_le_bytes(
        invalid_body_length[48..52]
            .try_into()
            .expect("payload length bytes"),
    );
    invalid_body_length[48..52].copy_from_slice(&(declared_payload + 1).to_le_bytes());
    assert_code(
        decode_request(&invalid_body_length, 1_500),
        "IPC_BODY_LENGTH_INVALID",
    );
}

#[test]
fn mutation_requires_idempotency_key_but_read_does_not() {
    let read = RequestEnvelope::new(
        IpcRequestId::new(),
        ApplicationCommand::ServiceHealth,
        None,
        10,
        20,
        json!({}),
    )
    .expect("read request may omit key");
    decode_request(&encode_request(&read).expect("encode read"), 15).expect("decode read");

    let mutation = RequestEnvelope::new(
        IpcRequestId::new(),
        ApplicationCommand::ConnectionSave,
        None,
        10,
        20,
        json!({"displayName": "primary"}),
    )
    .expect_err("mutation requires idempotency");
    assert_eq!(mutation.code(), "IPC_IDEMPOTENCY_REQUIRED");
}

#[test]
fn malformed_corpus_never_panics_or_allocates_from_untrusted_lengths() {
    for length in 0..512 {
        let mut bytes = Vec::with_capacity(length);
        for index in 0..length {
            bytes.push(((length * 31 + index * 17) & 0xff) as u8);
        }
        let result = decode_request(&bytes, 50_000);
        if let Err(error) = result {
            assert!(
                error.code().starts_with("IPC_"),
                "all malformed input has a stable IPC code"
            );
        }
    }
}

#[test]
fn stream_decoder_handles_chunk_boundaries_and_multiple_frames() {
    let first = encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::ServiceHealth,
            None,
            1_000,
            2_000,
            json!({}),
        )
        .expect("first request"),
    )
    .expect("encode first");
    let second = encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::SchemaStatus,
            None,
            1_000,
            2_000,
            json!({}),
        )
        .expect("second request"),
    )
    .expect("encode second");
    let mut combined = first;
    combined.extend_from_slice(&second);

    let mut decoder = RequestStreamDecoder::new();
    let mut decoded = Vec::new();
    for chunk in combined.chunks(7) {
        decoded.extend(decoder.push(chunk, 1_500).expect("decode stream chunk"));
    }
    decoder.finish().expect("complete stream");

    assert_eq!(decoded.len(), 2);
    assert_eq!(decoded[0].command(), ApplicationCommand::ServiceHealth);
    assert_eq!(decoded[1].command(), ApplicationCommand::SchemaStatus);
    assert_eq!(decoder.buffered_bytes(), 0);
}

#[test]
fn partial_secret_frame_is_wiped_on_disconnect() {
    let canary = "ipc-secret-canary-7bb1";
    let encoded = encode_request(
        &RequestEnvelope::new(
            IpcRequestId::new(),
            ApplicationCommand::SecretCreate,
            Some("secret-create-1".to_owned()),
            1_000,
            2_000,
            json!({"secret": canary}),
        )
        .expect("secret request"),
    )
    .expect("encode secret request");
    let partial_length = encoded.len() - 1;

    let mut decoder = RequestStreamDecoder::new();
    let decoded = decoder
        .push(&encoded[..partial_length], 1_500)
        .expect("partial request is buffered");
    assert!(decoded.is_empty());
    assert_eq!(decoder.buffered_bytes(), partial_length);

    assert_code(decoder.finish(), "IPC_FRAME_TRUNCATED");
    assert_eq!(decoder.buffered_bytes(), 0);
    assert_eq!(decoder.wiped_bytes(), partial_length);
}

#[test]
fn response_roundtrip_exposes_only_stable_redacted_failure() {
    let request_id = IpcRequestId::new();
    let success = ResponseEnvelope::success(
        request_id.clone(),
        ApplicationResponse {
            deduplicated: true,
            resource: None,
            body: json!({"service": "healthy"}),
        },
    )
    .expect("success response");
    let success =
        decode_response(&encode_response(&success).expect("encode success")).expect("decode");
    assert!(success.is_success());
    assert_eq!(success.code(), "OK");
    assert!(success.deduplicated());
    assert_eq!(success.request_id(), &request_id);

    let failure = ApplicationFailure::from(&ApplicationError::Internal);
    let response =
        ResponseEnvelope::failure(request_id, failure).expect("bounded failure response");
    let encoded = encode_response(&response).expect("encode failure");
    let encoded_text = String::from_utf8_lossy(&encoded);
    assert!(!encoded_text.contains("sqlite"));
    assert!(!encoded_text.contains("Debug"));

    let decoded = decode_response(&encoded).expect("decode failure");
    assert!(!decoded.is_success());
    assert_eq!(decoded.code(), "APPLICATION_INTERNAL");
    assert_eq!(
        decoded.body()["message"],
        "The service could not complete the request."
    );
}

fn assert_code<T: std::fmt::Debug>(
    result: Result<T, dbbackup_service::ipc::IpcError>,
    expected: &str,
) {
    let error = result.expect_err("request must be rejected");
    assert_eq!(error.code(), expected);
}
