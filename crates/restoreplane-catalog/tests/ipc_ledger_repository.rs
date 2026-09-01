use restoreplane_catalog::{
    IpcCompletionSpec, IpcLedgerReservation, IpcRequestSpec, OperationalStore,
};
use restoreplane_domain::IpcRequestId;
use tempfile::TempDir;

#[test]
fn incomplete_request_is_never_reexecuted_and_completed_result_is_durable() {
    let temp = TempDir::new().expect("temp directory");
    let path = temp.path().join("operational.db");
    let original_id = IpcRequestId::new();
    let digest = "a".repeat(64);

    let store = OperationalStore::open(path.clone()).expect("open store");
    let repository = store.ipc();
    let first = repository
        .reserve_request(request(original_id.clone(), &digest))
        .expect("reserve new request");
    assert_eq!(first, IpcLedgerReservation::New);
    drop(repository);
    drop(store);

    let reopened = OperationalStore::open(path).expect("reopen store");
    let repository = reopened.ipc();
    let incomplete = repository
        .reserve_request(request(IpcRequestId::new(), &digest))
        .expect_err("incomplete request is not executed again");
    assert_eq!(incomplete.code(), "IPC_REQUEST_INCOMPLETE");

    let conflict = repository
        .reserve_request(request(IpcRequestId::new(), &"b".repeat(64)))
        .expect_err("same key with different payload conflicts");
    assert_eq!(conflict.code(), "IPC_IDEMPOTENCY_CONFLICT");

    repository
        .complete_request(
            &original_id,
            &digest,
            IpcCompletionSpec::succeeded(
                r#"{"kind":"success","value":{"deduplicated":false,"resource":null,"body":{"saved":true}}}"#,
                None,
                300,
            ),
        )
        .expect("complete reserved request");

    let replay = repository
        .reserve_request(request(IpcRequestId::new(), &digest))
        .expect("replay completed request");
    let IpcLedgerReservation::Completed(stored) = replay else {
        panic!("expected completed replay");
    };
    assert!(stored.succeeded());
    assert!(stored.response_json().contains(r#""saved":true"#));
}

fn request(request_id: IpcRequestId, digest: &str) -> IpcRequestSpec {
    IpcRequestSpec::new(
        request_id,
        "connection.save",
        "c".repeat(64),
        "connection-save-01",
        digest,
        100,
        1_000,
    )
}
