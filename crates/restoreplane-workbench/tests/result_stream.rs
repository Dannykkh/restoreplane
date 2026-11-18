mod support;
use restoreplane_domain::workbench::{Column, TypedCell};
use restoreplane_domain::workbench_metadata::{Consistency, Row};
use restoreplane_domain::workbench_stream::{Ack, Completion, ResultEvent, StreamIdentity};
use restoreplane_workbench::result_stream::{
    ResultStreams, StreamSpec, GLOBAL_CREDIT_BYTES, NATIVE_RESULT_BYTES,
};
use restoreplane_workbench::WorkbenchError;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
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
fn schema(columns: &[Column]) -> ResultEvent {
    ResultEvent::Schema {
        columns: columns.to_vec(),
        consistency: Consistency::LivePerPage,
    }
}
fn row(start: u64, value: TypedCell) -> ResultEvent {
    ResultEvent::Rows {
        start_row: start.into(),
        rows: vec![Row {
            row_handle: Uuid::new_v4(),
            cells: vec![value],
        }],
    }
}
fn ack(identity: &StreamIdentity, sequence: u64) -> Ack {
    Ack {
        identity: identity.clone(),
        highest_contiguous_sequence: sequence.into(),
    }
}
fn create(
    streams: &ResultStreams,
    fixture: &support::Fixture,
    columns: Vec<Column>,
) -> StreamIdentity {
    streams
        .create(
            &fixture.context,
            fixture.session,
            StreamSpec {
                query_run_id: Uuid::new_v4(),
                result_set_id: Uuid::new_v4(),
                schema_version: "d".repeat(64),
                columns,
                consistency: Consistency::LivePerPage,
            },
        )
        .unwrap()
}

#[tokio::test]
async fn pull_notice_sent_ledger_ack_once_replay_and_owner_are_enforced() {
    let fixture = support::Fixture::sqlite("CREATE TABLE sample(id INTEGER PRIMARY KEY)").await;
    let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
    let columns = vec![column()];
    let identity = create(&streams, &fixture, columns.clone());
    let notices = Arc::new(AtomicUsize::new(0));
    let seen = notices.clone();
    streams
        .set_notice_emitter(
            &fixture.context,
            &identity,
            Arc::new(move |notice| {
                assert!(serde_json::to_vec(notice).unwrap().len() < 1024);
                seen.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
        )
        .unwrap();
    for event in [
        schema(&columns),
        row(
            0,
            TypedCell::Text {
                value: "literal </script> 한국어".into(),
            },
        ),
        ResultEvent::Terminal {
            completeness: Completion::Complete,
            rows_seen: 1.into(),
            operation_version: 1.into(),
            error_code: None,
        },
    ] {
        streams
            .publish_registered(&fixture.context, &identity, event)
            .await
            .unwrap();
    }
    assert_eq!(notices.load(Ordering::SeqCst), 3);
    assert!(matches!(
        streams.ack(&fixture.context, &ack(&identity, 1)),
        Err(WorkbenchError::AckInvalid)
    ));
    let first = streams.frames(&fixture.context, &identity, None).unwrap();
    assert!(serde_json::to_vec(&first).unwrap().len() <= 256 * 1024);
    assert!(matches!(
        streams.frames(&fixture.context, &identity, None),
        Err(WorkbenchError::Limit)
    ));
    drop(first);
    assert!(matches!(
        streams.ack(&fixture.context, &ack(&identity, 1)),
        Err(WorkbenchError::AckInvalid)
    ));
    let first = streams.frames(&fixture.context, &identity, None).unwrap();
    first.mark_sent().unwrap();
    drop(first);
    assert_eq!(
        streams
            .ack(&fixture.context, &ack(&identity, 1))
            .unwrap()
            .released_bytes
            .value(),
        0
    );
    let second = streams
        .frames(&fixture.context, &identity, Some(1))
        .unwrap();
    let encoded = serde_json::to_vec(&second).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(response["frames"][0]["sequence"], "2");
    second.mark_sent().unwrap();
    drop(second);
    let released = streams
        .ack(&fixture.context, &ack(&identity, 2))
        .unwrap()
        .released_bytes
        .value();
    assert!(released > 0);
    assert_eq!(
        streams
            .ack(&fixture.context, &ack(&identity, 2))
            .unwrap()
            .released_bytes
            .value(),
        0
    );
    let replay = streams
        .frames(&fixture.context, &identity, Some(1))
        .unwrap();
    replay.mark_sent().unwrap();
    drop(replay);
    assert_eq!(
        streams
            .ack(&fixture.context, &ack(&identity, 2))
            .unwrap()
            .released_bytes
            .value(),
        0
    );
    let third = streams
        .frames(&fixture.context, &identity, Some(2))
        .unwrap();
    third.mark_sent().unwrap();
    drop(third);
    let mut wrong = identity.clone();
    wrong.connection_generation = (identity.connection_generation.value() + 1).into();
    assert!(streams.frames(&fixture.context, &wrong, None).is_err());
    assert!(matches!(
        streams.frames(&fixture.context, &identity, Some(u64::MAX)),
        Err(WorkbenchError::AckInvalid)
    ));
    streams.retire_window(&fixture.context).unwrap();
    assert!(matches!(
        streams.frames(&fixture.context, &identity, None),
        Err(WorkbenchError::ResultExpired)
    ));
    assert_eq!(streams.budget().outstanding_bytes(), 0);
    assert!(streams.budget().peak_bytes() <= NATIVE_RESULT_BYTES);
}

#[tokio::test]
async fn global_credit_and_zero_pull_expiry_bound_all_results_together() {
    let fixture = support::Fixture::sqlite("CREATE TABLE sample(id INTEGER PRIMARY KEY)").await;
    let streams = Arc::new(
        ResultStreams::new_pull_with_timeout(fixture.runtime.clone(), Duration::from_millis(10))
            .unwrap(),
    );
    let columns = vec![column()];
    let identities = [
        create(&streams, &fixture, columns.clone()),
        create(&streams, &fixture, columns.clone()),
    ];
    for id in &identities {
        streams
            .try_publish(&fixture.context, id, schema(&columns), &mut |_| {
                panic!("pull mode cannot emit a full channel frame")
            })
            .unwrap();
    }
    let mut count = [0u64; 2];
    let mut bounded = false;
    for index in 0..256 {
        let slot = index % 2;
        match streams.try_publish(
            &fixture.context,
            &identities[slot],
            row(
                count[slot],
                TypedCell::Text {
                    value: "x".repeat(64 * 1024),
                },
            ),
            &mut |_| panic!("full channel frame"),
        ) {
            Ok(_) => count[slot] += 1,
            Err(WorkbenchError::Limit) => {
                bounded = true;
                break;
            }
            Err(error) => panic!("unexpected {}", error.code()),
        }
    }
    assert!(bounded);
    assert!(streams.budget().outstanding_bytes() <= GLOBAL_CREDIT_BYTES);
    assert!(count[0] > 0 && count[1] > 0);
    for (slot, id) in identities.iter().enumerate() {
        streams
            .try_publish(
                &fixture.context,
                id,
                ResultEvent::Terminal {
                    completeness: Completion::Capped,
                    rows_seen: count[slot].into(),
                    operation_version: 1.into(),
                    error_code: None,
                },
                &mut |_| panic!("full channel frame"),
            )
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(streams.expire_unacknowledged().await.unwrap(), 2);
    assert_eq!(streams.budget().outstanding_bytes(), 0);
    assert!(streams
        .frames(&fixture.context, &identities[0], None)
        .is_err());
    let terminal = streams
        .attach_pull(
            &fixture.context,
            &identities[0],
            None,
            Arc::new(|_| panic!("expired stream must not send a notice")),
        )
        .unwrap()
        .unwrap();
    assert!(!streams
        .is_retained(&fixture.context, &identities[0])
        .unwrap());
    assert!(matches!(
        terminal.envelope().event,
        ResultEvent::Terminal {
            completeness: Completion::Capped,
            ..
        }
    ));
    assert!(matches!(
        streams.ack(
            &fixture.context,
            &Ack {
                identity: identities[0].clone(),
                highest_contiguous_sequence: terminal.envelope().sequence
            }
        ),
        Err(WorkbenchError::AckInvalid)
    ));
    assert!(streams.budget().peak_bytes() <= NATIVE_RESULT_BYTES);
}

#[tokio::test]
async fn saturated_data_budget_preserves_pull_and_control_headroom() {
    let fixture = support::Fixture::sqlite("CREATE TABLE sample(id INTEGER PRIMARY KEY)").await;
    let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
    let columns = vec![column()];
    let identity = create(&streams, &fixture, columns.clone());
    streams
        .try_publish(&fixture.context, &identity, schema(&columns), &mut |_| {
            panic!("full channel frame")
        })
        .unwrap();
    let budget = streams.budget();
    let blocker = budget
        .reserve(59 * 1024 * 1024 - budget.data_bytes())
        .unwrap();
    assert!(budget.reserve(1).is_err());
    let pull = streams.frames(&fixture.context, &identity, None).unwrap();
    assert!(budget.pull_bytes() > 0);
    serde_json::to_vec(&pull).unwrap();
    pull.mark_sent().unwrap();
    drop(pull);
    assert_eq!(budget.pull_bytes(), 0);
    drop(blocker);
    assert!(budget.peak_bytes() <= NATIVE_RESULT_BYTES);
}
