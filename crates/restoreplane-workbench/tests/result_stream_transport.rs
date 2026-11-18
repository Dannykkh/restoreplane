//! Complements result_stream.rs using the shared real SQLite driver seam.
//! This is not a native Windows standard-token or WebView transport claim.
mod support;
use restoreplane_domain::workbench::{Column, TypedCell};
use restoreplane_domain::workbench_metadata::{Consistency, Row};
use restoreplane_domain::workbench_stream::{Ack, ResultEnvelope, ResultEvent, StreamIdentity};
use restoreplane_workbench::result_stream::{
    ResultStreams, StreamSpec, MAX_FRAME_BYTES, NATIVE_RESULT_BYTES,
};
use restoreplane_workbench::{IntegrityLevel, InvocationContext, NativePrincipal, WorkbenchError};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

fn column() -> Column {
    Column {
        column_id: Uuid::new_v4(),
        name: "value".into(),
        type_name: "text".into(),
        nullable: true,
        editable: false,
        readonly_reason: Some("test_readonly".into()),
    }
}
fn create(streams: &ResultStreams, fixture: &support::Fixture, column: &Column) -> StreamIdentity {
    streams
        .create(
            &fixture.context,
            fixture.session,
            StreamSpec {
                query_run_id: Uuid::new_v4(),
                result_set_id: Uuid::new_v4(),
                schema_version: "c".repeat(64),
                columns: vec![column.clone()],
                consistency: Consistency::LivePerPage,
            },
        )
        .unwrap()
}
fn schema(column: &Column) -> ResultEvent {
    ResultEvent::Schema {
        columns: vec![column.clone()],
        consistency: Consistency::LivePerPage,
    }
}
fn row(start: u64, bytes: usize) -> ResultEvent {
    ResultEvent::Rows {
        start_row: start.into(),
        rows: vec![Row {
            row_handle: Uuid::new_v4(),
            cells: vec![TypedCell::Text {
                value: "x".repeat(bytes),
            }],
        }],
    }
}
fn publish(
    streams: &ResultStreams,
    fixture: &support::Fixture,
    identity: &StreamIdentity,
    event: ResultEvent,
) {
    streams
        .try_publish(&fixture.context, identity, event, &mut |_| {
            panic!("full-frame Channel in pull mode")
        })
        .unwrap();
}
fn ack(
    streams: &ResultStreams,
    fixture: &support::Fixture,
    identity: &StreamIdentity,
    sequence: u64,
) {
    streams
        .ack(
            &fixture.context,
            &Ack {
                identity: identity.clone(),
                highest_contiguous_sequence: sequence.into(),
            },
        )
        .unwrap();
}
fn pull(
    streams: &Arc<ResultStreams>,
    fixture: &support::Fixture,
    identity: &StreamIdentity,
    after: u64,
) -> Vec<u8> {
    let pull = streams
        .frames(&fixture.context, identity, Some(after))
        .unwrap();
    let encoded = serde_json::to_vec(&pull).unwrap();
    assert!(encoded.len() + 128 <= MAX_FRAME_BYTES);
    let value: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    let envelope: ResultEnvelope = serde_json::from_value(value["frames"][0].clone()).unwrap();
    assert_eq!(
        serde_json::to_vec(&envelope).unwrap().len(),
        envelope.encoded_bytes
    );
    pull.mark_sent().unwrap();
    encoded
}

#[tokio::test]
async fn cross_owner_window_cleanup_and_generation_are_exact() {
    let fixture = support::Fixture::sqlite("CREATE TABLE sample(id INTEGER PRIMARY KEY)").await;
    let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
    let column = column();
    let identity = create(&streams, &fixture, &column);
    publish(&streams, &fixture, &identity, schema(&column));
    publish(&streams, &fixture, &identity, row(0, 200));
    let other_window = InvocationContext::new(fixture.context.principal().clone(), Uuid::new_v4());
    let other_owner = InvocationContext::new(
        NativePrincipal::new("d".repeat(64), 7, IntegrityLevel::Standard).unwrap(),
        fixture.context.window_instance_id(),
    );
    for context in [&other_window, &other_owner] {
        assert!(matches!(
            streams.frames(context, &identity, None),
            Err(WorkbenchError::Forbidden)
        ));
    }
    assert_eq!(
        streams.retire_window(&other_owner).unwrap_err(),
        WorkbenchError::Forbidden
    );
    streams.retire_window(&other_window).unwrap();
    assert!(streams.is_retained(&fixture.context, &identity).unwrap());
    let mut stale = identity.clone();
    stale.stream_generation = 2.into();
    assert!(matches!(
        streams.frames(&fixture.context, &stale, None),
        Err(WorkbenchError::Forbidden)
    ));
    streams
        .retire_session(&fixture.context, fixture.session)
        .unwrap();
    assert!(matches!(
        streams.frames(&fixture.context, &identity, None),
        Err(WorkbenchError::ResultExpired)
    ));
    assert_eq!(streams.budget().outstanding_bytes(), 0);
    let next = create(&streams, &fixture, &column);
    streams.retire_window(&fixture.context).unwrap();
    assert!(matches!(
        streams.frames(&fixture.context, &next, None),
        Err(WorkbenchError::ResultExpired)
    ));
    fixture
        .runtime
        .close_window(&fixture.context)
        .await
        .unwrap();
}

#[tokio::test]
async fn eight_concurrent_large_pulls_and_repeat_replay_are_memory_bounded() {
    let fixture = support::Fixture::sqlite("CREATE TABLE sample(id INTEGER PRIMARY KEY)").await;
    let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
    let column = column();
    let identities = (0..9)
        .map(|_| create(&streams, &fixture, &column))
        .collect::<Vec<_>>();
    for identity in &identities {
        publish(&streams, &fixture, identity, schema(&column));
        publish(
            &streams,
            &fixture,
            identity,
            ResultEvent::Rows {
                start_row: 0.into(),
                rows: (0..4)
                    .map(|_| Row {
                        row_handle: Uuid::new_v4(),
                        cells: vec![TypedCell::Text {
                            value: "x".repeat(60_000),
                        }],
                    })
                    .collect(),
            },
        );
        pull(&streams, &fixture, identity, 0);
        ack(&streams, &fixture, identity, 1);
    }
    let baseline = streams.budget().retained_bytes();
    let guards = identities[..8]
        .iter()
        .map(|identity| streams.frames(&fixture.context, identity, Some(1)).unwrap())
        .collect::<Vec<_>>();
    assert!(streams.budget().pull_bytes() > 3_000_000);
    assert!(streams.budget().pull_bytes() <= 4 * 1024 * 1024);
    assert!(matches!(
        streams.frames(&fixture.context, &identities[8], Some(1)),
        Err(WorkbenchError::Limit)
    ));
    for guard in &guards {
        assert!(serde_json::to_vec(guard).unwrap().len() + 128 <= MAX_FRAME_BYTES);
        guard.mark_sent().unwrap();
    }
    drop(guards);
    assert_eq!(streams.budget().retained_bytes(), baseline);
    assert_eq!(streams.budget().pull_bytes(), 0);
    let encoded = pull(&streams, &fixture, &identities[0], 1);
    let credit = streams.budget().outstanding_bytes();
    for _ in 0..256 {
        streams
            .attach_pull(
                &fixture.context,
                &identities[0],
                Some(1),
                Arc::new(|notice| {
                    assert!(serde_json::to_vec(notice).unwrap().len() < 1024);
                    Ok(())
                }),
            )
            .unwrap();
        assert_eq!(pull(&streams, &fixture, &identities[0], 1), encoded);
        assert_eq!(streams.budget().outstanding_bytes(), credit);
        assert_eq!(streams.budget().retained_bytes(), baseline);
    }
    streams.retire_window(&fixture.context).unwrap();
    assert_eq!(streams.budget().outstanding_bytes(), 0);
    assert!(streams.budget().peak_bytes() <= NATIVE_RESULT_BYTES);
    println!("M3 pull_transport concurrent_pulls=8 replay_count=256 boundary_violations=0");
}

#[tokio::test]
async fn acknowledged_replay_eviction_expires_old_cursor_without_sql_replay() {
    let fixture = support::Fixture::sqlite("CREATE TABLE sample(id INTEGER PRIMARY KEY)").await;
    let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
    let column = column();
    let identity = create(&streams, &fixture, &column);
    publish(&streams, &fixture, &identity, schema(&column));
    pull(&streams, &fixture, &identity, 0);
    ack(&streams, &fixture, &identity, 1);
    for index in 0..140u64 {
        publish(&streams, &fixture, &identity, row(index, 16));
        pull(&streams, &fixture, &identity, index + 1);
        ack(&streams, &fixture, &identity, index + 2);
    }
    assert!(matches!(
        streams.frames(&fixture.context, &identity, None),
        Err(WorkbenchError::ResultExpired)
    ));
    assert_eq!(streams.budget().outstanding_bytes(), 0);
    assert!(streams
        .frames(&fixture.context, &identity, Some(141))
        .is_ok());
}

#[tokio::test]
async fn oversized_body_and_failed_notice_never_authorize_unseen_ack() {
    let fixture = support::Fixture::sqlite("CREATE TABLE sample(id INTEGER PRIMARY KEY)").await;
    let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
    let column = column();
    let identity = create(&streams, &fixture, &column);
    publish(&streams, &fixture, &identity, schema(&column));
    let baseline = streams.budget().retained_bytes();
    assert!(matches!(
        streams.try_publish(
            &fixture.context,
            &identity,
            row(0, MAX_FRAME_BYTES),
            &mut |_| Ok(())
        ),
        Err(WorkbenchError::Limit)
    ));
    assert_eq!(streams.budget().retained_bytes(), baseline);
    assert_eq!(streams.budget().outstanding_bytes(), 0);
    streams
        .set_notice_emitter(
            &fixture.context,
            &identity,
            Arc::new(|_| Err(WorkbenchError::Io)),
        )
        .unwrap();
    assert!(matches!(
        streams.try_publish(&fixture.context, &identity, row(0, 100), &mut |_| panic!(
            "large channel"
        )),
        Err(WorkbenchError::Io)
    ));
    assert_eq!(
        streams
            .ack(
                &fixture.context,
                &Ack {
                    identity: identity.clone(),
                    highest_contiguous_sequence: 2.into()
                }
            )
            .unwrap_err(),
        WorkbenchError::AckInvalid
    );
    streams
        .attach_pull(&fixture.context, &identity, None, Arc::new(|_| Ok(())))
        .unwrap();
    pull(&streams, &fixture, &identity, 0);
    pull(&streams, &fixture, &identity, 1);
    ack(&streams, &fixture, &identity, 2);
}

#[tokio::test]
async fn expiry_between_pull_and_reply_finish_never_advances_sent() {
    let fixture = support::Fixture::sqlite("CREATE TABLE sample(id INTEGER PRIMARY KEY)").await;
    let streams = Arc::new(
        ResultStreams::new_pull_with_timeout(fixture.runtime.clone(), Duration::from_millis(10))
            .unwrap(),
    );
    let column = column();
    let identity = create(&streams, &fixture, &column);
    publish(&streams, &fixture, &identity, schema(&column));
    publish(&streams, &fixture, &identity, row(0, 200));
    let pending_reply = streams.frames(&fixture.context, &identity, None).unwrap();
    serde_json::to_vec(&pending_reply).unwrap();
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(streams.expire_unacknowledged().await.unwrap(), 1);
    assert_eq!(
        pending_reply.mark_sent().unwrap_err(),
        WorkbenchError::ResultExpired
    );
    assert_eq!(
        streams
            .ack(
                &fixture.context,
                &Ack {
                    identity,
                    highest_contiguous_sequence: 1.into()
                }
            )
            .unwrap_err(),
        WorkbenchError::AckInvalid
    );
    drop(pending_reply);
    assert_eq!(streams.budget().pull_bytes(), 0);
    assert_eq!(streams.budget().outstanding_bytes(), 0);
}
