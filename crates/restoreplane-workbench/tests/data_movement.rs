use restoreplane_domain::workbench::{Column, TypedCell};
use restoreplane_workbench::data_movement::{JsonRecordReader, SourceScalar};
use restoreplane_workbench::movement_codec::{ExportFormat, ExportWriter, FormulaPolicy};
use restoreplane_workbench::WorkbenchError;
use std::io::Read;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Chunks<R> {
    inner: R,
    bytes: Arc<AtomicUsize>,
    chunk: usize,
}
#[test]
fn legal_escaped_typed_json_record_roundtrips_without_relaxing_decoded_cell_bound() {
    let column = Column {
        column_id: uuid::Uuid::new_v4(),
        name: "escaped".into(),
        type_name: "text".into(),
        nullable: false,
        editable: false,
        readonly_reason: None,
    };
    let cell = TypedCell::Text {
        value: "\0".repeat(40 * 1024),
    };
    let mut writer = ExportWriter::new(
        Vec::new(),
        ExportFormat::TypedJson,
        std::slice::from_ref(&column),
        FormulaPolicy::Escape,
    )
    .unwrap();
    writer.write_row(std::slice::from_ref(&cell)).unwrap();
    let bytes = writer.finish().unwrap();
    assert!(bytes.len() > 128 * 1024 && bytes.len() < 256 * 1024);
    let mut reader = JsonRecordReader::new(bytes.as_slice()).unwrap();
    let record = reader.next_record().unwrap().unwrap();
    assert_eq!(record.fields, vec![SourceScalar::Typed(cell)]);
    assert!(reader.next_record().unwrap().is_none());
    assert!(reader.complete());
    // The raw record is legal, but the decoded value still exceeds 64KiB.
    let over = serde_json::json!({"kind":"text","value":"x".repeat(64*1024+1)}).to_string();
    let header = serde_json::to_string(&vec![column]).unwrap();
    let invalid = format!("{{\"columns\":{header},\"rows\":[[{over}]],\"complete\":true}}");
    let mut reader = JsonRecordReader::new(invalid.as_bytes()).unwrap();
    assert!(matches!(reader.next_record(), Err(WorkbenchError::Limit)));
    assert!(!reader.complete());
}
impl<R: Read> Read for Chunks<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let size = buf.len().min(self.chunk);
        let count = self.inner.read(&mut buf[..size])?;
        self.bytes.fetch_add(count, Ordering::SeqCst);
        Ok(count)
    }
}

#[test]
fn json_preserves_number_lexemes_and_reorders_object_keys_without_materializing_the_file() {
    let input =
        br#"[{"a":90071992547409931234567,"b":1.2300e-20},{"b":-0.0000,"a":18446744073709551615}]"#;
    let count = Arc::new(AtomicUsize::new(0));
    let mut reader = JsonRecordReader::new(Chunks {
        inner: input.as_slice(),
        bytes: count.clone(),
        chunk: 1,
    })
    .unwrap();
    let first = reader.next_record().unwrap().unwrap();
    assert_eq!(first.names, Some(vec!["a".into(), "b".into()]));
    assert_eq!(
        first.fields,
        vec![
            SourceScalar::Number("90071992547409931234567".into()),
            SourceScalar::Number("1.2300e-20".into())
        ]
    );
    assert_eq!(
        &input[first.byte_start as usize..first.byte_end as usize],
        br#"{"a":90071992547409931234567,"b":1.2300e-20}"#
    );
    let second = reader.next_record().unwrap().unwrap();
    assert_eq!(second.row_index, 1);
    assert_eq!(
        second.fields,
        vec![
            SourceScalar::Number("18446744073709551615".into()),
            SourceScalar::Number("-0.0000".into())
        ]
    );
    assert!(reader.next_record().unwrap().is_none());
    assert!(reader.complete());
    assert_eq!(count.load(Ordering::SeqCst), input.len());
}

#[test]
fn malformed_or_changed_json_rows_poison_the_reader_instead_of_skipping() {
    for input in [
        r#"[{"a":1,"a":2},{"a":3}]"#,
        r#"[[1],[2,3],[4]]"#,
        r#"[{"a":1},{"b":2},{"a":3}]"#,
        r#"[[1],]"#,
    ] {
        let mut reader = JsonRecordReader::new(input.as_bytes()).unwrap();
        let result = loop {
            match reader.next_record() {
                Ok(Some(_)) => {}
                value => break value,
            }
        };
        assert!(result.is_err());
        assert!(reader.next_record().is_err());
        assert!(!reader.complete());
    }
}

#[test]
fn typed_json_export_and_import_roundtrip_exact_cells_including_large_base64_wire() {
    let cells = vec![
        TypedCell::Null,
        TypedCell::U64 {
            value: u64::MAX.to_string(),
        },
        TypedCell::Decimal {
            value: "12345678901234567890.00000001".into(),
            scale: Some(8),
        },
        TypedCell::Text {
            value: "=한글\n\\N".into(),
        },
        TypedCell::Bytes {
            base64: base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                vec![0xabu8; 64 * 1024],
            ),
        },
    ];
    let columns = cells
        .iter()
        .enumerate()
        .map(|(index, _)| Column {
            column_id: uuid::Uuid::new_v4(),
            name: format!("column{index}"),
            type_name: "test".into(),
            nullable: true,
            editable: false,
            readonly_reason: Some("test".into()),
        })
        .collect::<Vec<_>>();
    let mut writer = ExportWriter::new(
        Vec::new(),
        ExportFormat::TypedJson,
        &columns,
        FormulaPolicy::Escape,
    )
    .unwrap();
    writer.write_row(&cells).unwrap();
    let encoded = writer.finish().unwrap();
    let mut reader = JsonRecordReader::new(encoded.as_slice()).unwrap();
    assert_eq!(reader.columns().unwrap().len(), cells.len());
    let parsed = reader.next_record().unwrap().unwrap();
    assert_eq!(
        parsed.fields,
        cells
            .into_iter()
            .map(SourceScalar::Typed)
            .collect::<Vec<_>>()
    );
    assert!(reader.next_record().unwrap().is_none());
    assert!(reader.complete());
}

#[test]
fn incomplete_source_or_missing_complete_marker_never_yields_complete() {
    let column = Column {
        column_id: uuid::Uuid::new_v4(),
        name: "a".into(),
        type_name: "bigint".into(),
        nullable: true,
        editable: false,
        readonly_reason: None,
    };
    let mut writer = ExportWriter::new(
        Vec::new(),
        ExportFormat::TypedJson,
        &[column],
        FormulaPolicy::Escape,
    )
    .unwrap();
    writer
        .write_row(&[TypedCell::I64 { value: "1".into() }])
        .unwrap();
    let encoded = String::from_utf8(writer.finish().unwrap()).unwrap();
    let complete = format!("{}false}}", encoded.strip_suffix("true}").unwrap());
    let mut reader = JsonRecordReader::new(complete.as_bytes()).unwrap();
    assert!(reader.next_record().unwrap().is_some());
    assert!(reader.next_record().is_err());
    assert!(!reader.complete());
    for input in [b"[[1]".as_slice(), b"[[1]] true".as_slice()] {
        let mut reader = JsonRecordReader::new(input).unwrap();
        assert!(reader.next_record().unwrap().is_some());
        assert!(reader.next_record().is_err());
        assert!(!reader.complete());
    }
}

#[test]
fn oversized_and_deep_json_stops_at_a_bounded_record_before_reading_the_whole_source() {
    let count = Arc::new(AtomicUsize::new(0));
    let input = format!("[[\"{}\"]]", "x".repeat(4 * 1024 * 1024));
    let mut reader = JsonRecordReader::new(Chunks {
        inner: input.as_bytes(),
        bytes: count.clone(),
        chunk: 4096,
    })
    .unwrap();
    assert!(matches!(reader.next_record(), Err(WorkbenchError::Limit)));
    assert!(count.load(Ordering::SeqCst) <= 272 * 1024);
    let deep = format!("[{}0{}]", "[".repeat(40), "]".repeat(40));
    let mut reader = JsonRecordReader::new(deep.as_bytes()).unwrap();
    assert!(matches!(reader.next_record(), Err(WorkbenchError::Limit)));
}
