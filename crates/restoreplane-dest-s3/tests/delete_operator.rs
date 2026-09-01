use opendal::{services::Memory, Operator};
use restoreplane_dest_s3::{S3DeleteCfg, S3DeleteOperatorV2};
use restoreplane_pipeline::{DeleteErrorV2, DestinationDeleteV2};

#[tokio::test]
async fn exact_version_is_required_before_an_s3_delete_is_attempted() {
    let op = Operator::new(Memory::default()).expect("memory operator");
    op.write("archive/recovery/payload.bin", "ciphertext")
        .await
        .expect("seed object");
    let delete = S3DeleteOperatorV2::from_operator(op.clone(), "archive");

    let error = delete
        .delete_object("recovery/payload.bin", None)
        .await
        .expect_err("versionless delete must not create a delete marker");
    assert!(matches!(
        error,
        DeleteErrorV2::Backend {
            retryable: false,
            ..
        }
    ));
    assert!(
        op.exists("archive/recovery/payload.bin")
            .await
            .expect("object existence"),
        "a rejected versionless delete must not touch the object"
    );

    let error = delete
        .delete_object("recovery/payload.bin", Some("version-17"))
        .await
        .expect_err("memory backend exposes that the version argument was propagated");
    assert!(matches!(
        error,
        DeleteErrorV2::Backend {
            retryable: false,
            ref message,
            ..
        } if message.contains("version")
    ));
    assert!(
        op.exists("archive/recovery/payload.bin")
            .await
            .expect("object remains"),
        "a backend that cannot bind a version must not fall back to versionless delete"
    );
}

#[tokio::test]
async fn unsafe_relative_keys_and_versions_are_rejected_without_backend_mutation() {
    let op = Operator::new(Memory::default()).expect("memory operator");
    op.write("archive/recovery/payload.bin", "ciphertext")
        .await
        .expect("seed object");
    let delete = S3DeleteOperatorV2::from_operator(op.clone(), "archive");

    for (key, version) in [
        ("../payload.bin", Some("version-17")),
        ("/payload.bin", Some("version-17")),
        ("recovery\\payload.bin", Some("version-17")),
        ("recovery/payload.bin", Some("")),
        ("recovery/payload.bin", Some("version\r\n17")),
    ] {
        let error = delete
            .delete_object(key, version)
            .await
            .expect_err("unsafe identity must fail closed");
        assert!(matches!(
            error,
            DeleteErrorV2::Backend {
                retryable: false,
                ..
            }
        ));
    }
    assert!(op
        .exists("archive/recovery/payload.bin")
        .await
        .expect("object existence"));
}

#[tokio::test]
async fn s3_request_carries_the_exact_version_and_missing_replay_is_idempotent() {
    let (endpoint, requests, server) =
        serve_mock_s3(vec![("204 No Content", ""), ("404 Not Found", "")]);
    opendal::install_default();
    let builder = opendal::services::S3::default()
        .endpoint(&endpoint)
        .region("us-east-1")
        .bucket("backup-bucket")
        .access_key_id("delete-role")
        .secret_access_key("test-secret");
    let op = Operator::new(builder).expect("S3 operator");
    let delete = S3DeleteOperatorV2::from_operator(op, "archive");

    delete
        .delete_object("recovery/payload.bin", Some("version-17"))
        .await
        .expect("first exact-version delete");
    delete
        .delete_object("recovery/payload.bin", Some("version-17"))
        .await
        .expect("missing replay is idempotent");

    let first = requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("first request");
    let second = requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("second request");
    server.join().expect("mock S3 server");
    for request in [first, second] {
        let request_line = request.lines().next().expect("request line");
        assert!(
            request_line.starts_with(
                "DELETE /backup-bucket/archive/recovery/payload.bin?versionId=version-17 "
            ),
            "unexpected request line: {request_line}"
        );
    }
}

#[tokio::test]
async fn active_object_retention_is_reported_as_preservation_without_delete() {
    let (endpoint, requests, server) = serve_mock_s3(vec![
        (
            "200 OK",
            "<LegalHold xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Status>OFF</Status></LegalHold>",
        ),
        (
            "200 OK",
            "<Retention xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Mode>COMPLIANCE</Mode><RetainUntilDate>2099-01-01T00:00:00Z</RetainUntilDate></Retention>",
        ),
    ]);
    let delete = S3DeleteOperatorV2::new(delete_cfg(endpoint)).expect("delete operator");

    let error = delete
        .delete_object("recovery/payload.bin", Some("version-17"))
        .await
        .expect_err("active retention must preserve the object");
    assert!(matches!(
        error,
        DeleteErrorV2::RetentionLocked {
            retention_until_ms: Some(value),
            ..
        } if value > 4_000_000_000_000
    ));

    let first = requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("legal hold probe");
    let second = requests
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("retention probe");
    server.join().expect("mock S3 server");
    assert!(first
        .lines()
        .next()
        .expect("legal hold request")
        .contains("?legal-hold=&versionId=version-17 "));
    assert!(second
        .lines()
        .next()
        .expect("retention request")
        .contains("?retention=&versionId=version-17 "));
    assert!(
        requests.try_recv().is_err(),
        "active retention must prevent a DELETE request"
    );
}

#[tokio::test]
async fn generic_permission_denied_is_not_mislabeled_as_object_lock() {
    let legal_hold_off =
        "<LegalHold xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Status>OFF</Status></LegalHold>";
    let (endpoint, requests, server) = serve_mock_s3(vec![
        ("200 OK", legal_hold_off),
        ("404 Not Found", ""),
        (
            "403 Forbidden",
            "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
        ),
        ("200 OK", legal_hold_off),
        ("404 Not Found", ""),
    ]);
    let delete = S3DeleteOperatorV2::new(delete_cfg(endpoint)).expect("delete operator");

    let error = delete
        .delete_object("recovery/payload.bin", Some("version-17"))
        .await
        .expect_err("unlocked permission denial must remain an authorization failure");
    assert!(matches!(
        error,
        DeleteErrorV2::Backend {
            retryable: false,
            ref message,
            ..
        } if message.contains("permission denied")
    ));

    let recorded = (0..5)
        .map(|_| {
            requests
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("S3 request")
        })
        .collect::<Vec<_>>();
    server.join().expect("mock S3 server");
    assert!(recorded[2]
        .lines()
        .next()
        .expect("delete request")
        .starts_with("DELETE "));
}

fn delete_cfg(endpoint: String) -> S3DeleteCfg {
    S3DeleteCfg {
        endpoint,
        region: "us-east-1".to_owned(),
        bucket: "backup-bucket".to_owned(),
        prefix: "archive".to_owned(),
        access_key_id: "delete-role".to_owned(),
        secret_access_key: "test-secret".to_owned(),
    }
}

fn serve_mock_s3(
    responses: Vec<(&'static str, &'static str)>,
) -> (
    String,
    std::sync::mpsc::Receiver<String>,
    std::thread::JoinHandle<()>,
) {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock S3");
    let endpoint = format!("http://{}", listener.local_addr().expect("mock address"));
    let (sender, receiver) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().expect("accept S3 request");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .expect("read timeout");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4_096];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).expect("read S3 request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            sender
                .send(String::from_utf8(request).expect("UTF-8 S3 request"))
                .expect("record S3 request");
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/xml\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write S3 response");
        }
    });
    (endpoint, receiver, server)
}
