mod support;
use restoreplane_domain::workbench::TypedCell;
use restoreplane_domain::workbench_metadata::{
    CatalogDetail, CatalogKind, Comparison, Completeness, Consistency, Count, DescribePart,
    Direction, Filter, Limits, NullOrder, ObjectRef, Order, PageInput,
};
use restoreplane_domain::workbench_stream::Ack;
use restoreplane_workbench::browse::{BrowseInput, BrowseService};
use restoreplane_workbench::metadata::MetadataCatalog;
use restoreplane_workbench::result_stream::{ResultStreams, NATIVE_RESULT_BYTES};
use restoreplane_workbench::WorkbenchError;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn services(
    fixture: &support::Fixture,
) -> (Arc<ResultStreams>, Arc<MetadataCatalog>, BrowseService) {
    let streams = Arc::new(ResultStreams::new_pull(fixture.runtime.clone()).unwrap());
    let metadata = Arc::new(MetadataCatalog::new(
        fixture.runtime.clone(),
        streams.budget(),
    ));
    let browse = BrowseService::new(metadata.clone(), streams.clone());
    (streams, metadata, browse)
}
fn limits(rows: usize) -> Limits {
    Limits {
        max_rows: rows,
        max_bytes: 512 * 1024,
        statement_timeout_ms: 30_000,
        lock_wait_ms: 2000,
    }
}
async fn relation_object(
    metadata: &MetadataCatalog,
    fixture: &support::Fixture,
    name: &str,
) -> ObjectRef {
    let page = metadata
        .page(
            &fixture.context,
            fixture.session,
            None,
            CatalogKind::Relations,
            name.into(),
            PageInput {
                cursor: None,
                limit: 100,
            },
        )
        .await
        .unwrap();
    page.items
        .into_iter()
        .find(|item| item.name == name)
        .unwrap()
        .object
}

#[tokio::test]
async fn wide_two_hundred_row_requests_page_all_six_hundred_rows_without_wrapper_overflow_or_gaps()
{
    let columns = (1..100)
        .map(|index| format!("c{index} TEXT"))
        .collect::<Vec<_>>()
        .join(",");
    let values = (1..100)
        .map(|_| "CAST(x AS TEXT)")
        .collect::<Vec<_>>()
        .join(",");
    let setup=format!("CREATE TABLE wide(id INTEGER PRIMARY KEY,{columns});WITH RECURSIVE n(x) AS(VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<600) INSERT INTO wide SELECT x,{values} FROM n;");
    let fixture = support::Fixture::sqlite(&setup).await;
    let (streams, metadata, browse) = services(&fixture);
    let object = relation_object(&metadata, &fixture, "wide").await;
    let mut page = browse
        .open(
            &fixture.context,
            fixture.session,
            BrowseInput {
                object,
                filter: Filter::default(),
                order: Vec::new(),
                snapshot_id: None,
                limits: limits(200),
            },
            None,
        )
        .await
        .unwrap()
        .result;
    let mut keys = Vec::new();
    let mut pages = 0;
    loop {
        pages += 1;
        assert_eq!(page.columns.len(), 100);
        assert!(!page.page.items.is_empty());
        assert!(page.page.items.len() <= 200);
        assert!(serde_json::to_vec(&page).unwrap().len() <= 512 * 1024);
        keys.extend(page.page.items.iter().map(|row| match &row.cells[0] {
            TypedCell::I64 { value } => value.parse::<i64>().unwrap(),
            _ => panic!("exact key required"),
        }));
        match page.page.next_cursor.take() {
            Some(cursor) => {
                assert_eq!(page.page.completeness, Completeness::Limited);
                page = browse
                    .page(&fixture.context, fixture.session, cursor, 200)
                    .await
                    .unwrap()
                    .result()
                    .clone();
            }
            None => {
                assert_eq!(page.page.completeness, Completeness::Complete);
                break;
            }
        }
    }
    assert_eq!(keys, (1..=600).collect::<Vec<_>>());
    assert!(
        pages > 3,
        "normal 200-row requests must be split by bytes rather than rejected"
    );
    assert!(streams.budget().retained_bytes() <= NATIVE_RESULT_BYTES);
}

#[tokio::test]
async fn byte_full_browse_never_keeps_a_smaller_later_row_past_an_unreturned_key() {
    let fixture=support::Fixture::sqlite("CREATE TABLE varied(id INTEGER PRIMARY KEY,payload TEXT);INSERT INTO varied VALUES(1,printf('%060000d',1)),(2,printf('%060000d',2)),(3,'small');").await;
    let (_, metadata, browse) = services(&fixture);
    let object = relation_object(&metadata, &fixture, "varied").await;
    let mut quota = limits(200);
    quota.max_bytes = 100 * 1024;
    let first = browse
        .open(
            &fixture.context,
            fixture.session,
            BrowseInput {
                object,
                filter: Filter::default(),
                order: Vec::new(),
                snapshot_id: None,
                limits: quota,
            },
            None,
        )
        .await
        .unwrap()
        .result;
    assert_eq!(first.page.items.len(), 1);
    assert_eq!(
        first.page.items[0].cells[0],
        TypedCell::I64 { value: "1".into() }
    );
    let next = browse
        .page(
            &fixture.context,
            fixture.session,
            first.page.next_cursor.unwrap(),
            200,
        )
        .await
        .unwrap()
        .result()
        .clone();
    assert_eq!(
        next.page
            .items
            .iter()
            .map(|row| row.cells[0].clone())
            .collect::<Vec<_>>(),
        vec![
            TypedCell::I64 { value: "2".into() },
            TypedCell::I64 { value: "3".into() }
        ]
    );
    assert!(next.page.next_cursor.is_none());
    assert_eq!(next.page.completeness, Completeness::Complete);
}
async fn drain(
    streams: &Arc<ResultStreams>,
    fixture: &support::Fixture,
    identity: &restoreplane_domain::workbench_stream::StreamIdentity,
    after: &mut Option<u64>,
) {
    loop {
        let pull = streams.frames(&fixture.context, identity, *after).unwrap();
        let response: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&pull).unwrap()).unwrap();
        let Some(frame) = response["frames"].as_array().unwrap().first() else {
            break;
        };
        let sequence = frame["sequence"].as_str().unwrap().parse::<u64>().unwrap();
        let more = response["hasMore"].as_bool().unwrap();
        pull.mark_sent().unwrap();
        drop(pull);
        streams
            .ack(
                &fixture.context,
                &Ack {
                    identity: identity.clone(),
                    highest_contiguous_sequence: sequence.into(),
                },
            )
            .unwrap();
        *after = Some(sequence);
        if !more {
            break;
        }
    }
}

#[tokio::test]
async fn real_sqlite_metadata_keyset_ties_nulls_provenance_and_pull_are_coherent() {
    let fixture=support::Fixture::sqlite("PRAGMA journal_mode=WAL; CREATE TABLE records(id INTEGER PRIMARY KEY,label TEXT,payload BLOB); INSERT INTO records VALUES(1,'same',X'00FF'),(2,'same',X''),(3,NULL,NULL),(4,'',NULL),(5,'한국어 '' OR 1=1 --',NULL),(9223372036854775807,'same',NULL); CREATE TABLE keyless(label TEXT); INSERT INTO keyless VALUES('a'),('a'),(NULL);").await;
    let (streams, metadata, browse) = services(&fixture);
    let object = relation_object(&metadata, &fixture, "records").await;
    let relation = metadata
        .resolve_relation(&fixture.context, fixture.session, &object)
        .await
        .unwrap();
    assert!(relation.stable_key().is_some());
    let label = relation
        .columns
        .iter()
        .find(|column| column.column.name == "label")
        .unwrap()
        .column
        .column_id;
    let notices = Arc::new(AtomicUsize::new(0));
    let count = notices.clone();
    let first = browse
        .open(
            &fixture.context,
            fixture.session,
            BrowseInput {
                object: object.clone(),
                filter: Filter::default(),
                order: vec![Order {
                    column_id: label,
                    direction: Direction::Asc,
                    nulls: NullOrder::Last,
                }],
                snapshot_id: None,
                limits: limits(2),
            },
            Some(Arc::new(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })),
        )
        .await
        .unwrap();
    assert!(matches!(first.result.consistency, Consistency::LivePerPage));
    assert!(matches!(first.result.page.count, Count::Unknown));
    let identity = first
        .stream
        .clone()
        .expect("production pull identity required");
    let mut after = None;
    drain(&streams, &fixture, &identity, &mut after).await;
    let mut ids = first
        .result
        .page
        .items
        .iter()
        .map(|row| match &row.cells[0] {
            TypedCell::I64 { value } => value.clone(),
            _ => panic!("lossless integer required"),
        })
        .collect::<Vec<_>>();
    let original = browse
        .original(
            &fixture.context,
            fixture.session,
            first.result.page.items[0].row_handle,
        )
        .unwrap();
    assert!(original.single_table && original.complete_values);
    assert_eq!(original.key_columns.len(), 1);
    let mut next = first.result.page.next_cursor.clone();
    while let Some(cursor) = next {
        let page = browse
            .page(&fixture.context, fixture.session, cursor, 2)
            .await
            .unwrap();
        for row in &page.result().page.items {
            if let TypedCell::I64 { value } = &row.cells[0] {
                ids.push(value.clone());
            }
        }
        next = page.result().page.next_cursor.clone();
        drain(&streams, &fixture, &identity, &mut after).await;
    }
    assert_eq!(ids, vec!["4", "1", "2", "9223372036854775807", "5", "3"]);
    assert!(notices.load(Ordering::SeqCst) >= 5);
    assert!(streams.budget().peak_bytes() <= NATIVE_RESULT_BYTES);
    let filtered = browse
        .open(
            &fixture.context,
            fixture.session,
            BrowseInput {
                object,
                filter: Filter::Compare {
                    column_id: label,
                    operator: Comparison::Eq,
                    value: Some(TypedCell::Text {
                        value: "한국어 ' OR 1=1 --".into(),
                    }),
                },
                order: Vec::new(),
                snapshot_id: None,
                limits: limits(100),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(filtered.result.page.items.len(), 1);
    let keyless = relation_object(&metadata, &fixture, "keyless").await;
    let preview = browse
        .open(
            &fixture.context,
            fixture.session,
            BrowseInput {
                object: keyless,
                filter: Filter::default(),
                order: Vec::new(),
                snapshot_id: None,
                limits: limits(2),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(preview.result.page.completeness, Completeness::Limited);
    assert!(preview.result.page.next_cursor.is_none());
    let provenance = browse
        .original(
            &fixture.context,
            fixture.session,
            preview.result.page.items[0].row_handle,
        )
        .unwrap();
    assert!(provenance.key_columns.is_empty());
}

#[tokio::test]
async fn schema_changes_invalidate_cursor_and_graph_reuses_real_catalog_ids() {
    let fixture=support::Fixture::sqlite("CREATE TABLE parent(id INTEGER PRIMARY KEY); CREATE TABLE child(id INTEGER PRIMARY KEY,parent_id INTEGER REFERENCES parent(id)); INSERT INTO child VALUES(1,NULL),(2,NULL),(3,NULL);").await;
    let (streams, metadata, browse) = services(&fixture);
    let schemas = metadata
        .page(
            &fixture.context,
            fixture.session,
            None,
            CatalogKind::Schemas,
            String::new(),
            PageInput {
                cursor: None,
                limit: 100,
            },
        )
        .await
        .unwrap();
    let schema = schemas.items[0].object.clone();
    let graph = metadata
        .graph(
            &fixture.context,
            fixture.session,
            schema,
            PageInput {
                cursor: None,
                limit: 100,
            },
        )
        .await
        .unwrap();
    let parent = graph
        .items
        .iter()
        .find(|node| node.name == "parent")
        .unwrap();
    let child = graph
        .items
        .iter()
        .find(|node| node.name == "child")
        .unwrap();
    assert!(child.edges.iter().any(|edge| edge.to == parent.node_id));
    let details = metadata
        .describe(
            &fixture.context,
            fixture.session,
            child.relation.clone(),
            DescribePart::ForeignKeys,
            PageInput {
                cursor: None,
                limit: 100,
            },
        )
        .await
        .unwrap();
    let CatalogDetail::ForeignKey {
        referenced_relation,
        ..
    } = &details.items[0]
    else {
        panic!("foreign key required")
    };
    assert_eq!(referenced_relation.catalog_id, parent.node_id);
    let first = browse
        .open(
            &fixture.context,
            fixture.session,
            BrowseInput {
                object: child.relation.clone(),
                filter: Filter::default(),
                order: Vec::new(),
                snapshot_id: None,
                limits: limits(1),
            },
            None,
        )
        .await
        .unwrap();
    let handle = first.result.page.items[0].row_handle;
    let cursor = first.result.page.next_cursor.clone().unwrap();
    rusqlite::Connection::open(&fixture.path)
        .unwrap()
        .execute_batch("ALTER TABLE child ADD COLUMN new_value TEXT")
        .unwrap();
    assert!(matches!(
        browse
            .page(&fixture.context, fixture.session, cursor, 1)
            .await,
        Err(WorkbenchError::CursorInvalid)
    ));
    assert!(browse
        .original(&fixture.context, fixture.session, handle)
        .is_err());
    browse.retire_window(&fixture.context).await.unwrap();
    metadata.retire_window(&fixture.context).unwrap();
    streams.retire_window(&fixture.context).unwrap();
    assert_eq!(metadata.retained_counts().unwrap(), (0, 0, 0));
}

#[tokio::test]
async fn ten_thousand_real_metadata_objects_page_without_accumulating_the_catalog() {
    let fixture = support::Fixture::sqlite("CREATE TABLE t00000(id INTEGER PRIMARY KEY)").await;
    let writer = rusqlite::Connection::open(&fixture.path).unwrap();
    writer.execute_batch("BEGIN").unwrap();
    for index in 1..10_000 {
        writer
            .execute_batch(&format!("CREATE TABLE t{index:05}(id INTEGER PRIMARY KEY)"))
            .unwrap();
    }
    writer.execute_batch("COMMIT").unwrap();
    drop(writer);
    let (streams, metadata, _) = services(&fixture);
    let mut cursor = None;
    let mut count = 0usize;
    let mut previous = String::new();
    let started = std::time::Instant::now();
    loop {
        let page = metadata
            .page(
                &fixture.context,
                fixture.session,
                None,
                CatalogKind::Relations,
                "t".into(),
                PageInput { cursor, limit: 100 },
            )
            .await
            .unwrap();
        assert!(page.items.len() <= 100);
        for item in page.items {
            assert!(item.name > previous);
            previous = item.name;
            count += 1;
        }
        assert!(metadata.retained_counts().unwrap().0 <= 2048);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(count, 10_000);
    let search = metadata
        .page(
            &fixture.context,
            fixture.session,
            None,
            CatalogKind::Relations,
            "t099".into(),
            PageInput {
                cursor: None,
                limit: 200,
            },
        )
        .await
        .unwrap();
    assert_eq!(search.items.len(), 100);
    assert!(streams.budget().peak_bytes() <= NATIVE_RESULT_BYTES);
    eprintln!("04 metadata actual_objects={count}; elapsed_ms={}; native_reserved_peak_bytes={}; no_dom_or_rss_claim=true",started.elapsed().as_millis(),streams.budget().peak_bytes());
}
