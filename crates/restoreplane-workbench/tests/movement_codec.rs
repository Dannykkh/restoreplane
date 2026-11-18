use restoreplane_domain::workbench::{Column, TypedCell};
use restoreplane_workbench::movement_codec::{
    decode_delimited_value, CodecLimits, DelimitedReader, ExportFormat, ExportWriter, FormulaPolicy,
};
use restoreplane_workbench::WorkbenchError;
use std::io::{Cursor, Read, Write};
use uuid::Uuid;

struct Tiny<R> {
    reader: R,
    chunk: usize,
}
impl<R: Read> Read for Tiny<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let size = output.len().min(self.chunk);
        self.reader.read(&mut output[..size])
    }
}
fn columns(names: &[&str]) -> Vec<Column> {
    names
        .iter()
        .map(|name| Column {
            column_id: Uuid::new_v4(),
            name: (*name).into(),
            type_name: "fixture".into(),
            nullable: true,
            editable: false,
            readonly_reason: None,
        })
        .collect()
}
fn text(value: &str) -> TypedCell {
    TypedCell::Text {
        value: value.into(),
    }
}

#[test]
fn quoted_multiline_escaped_quotes_crlf_bom_and_split_utf8_keep_exact_byte_ranges() {
    let bytes = "\u{feff}같은,같은\r\n\"첫줄\r\n다음 \"\"인용\"\" é\",값\r\n\n끝,\"\"".as_bytes();
    for chunk in [1, 2, 3, 7, 16_384] {
        let mut reader = DelimitedReader::new(
            Tiny {
                reader: Cursor::new(bytes),
                chunk,
            },
            b',',
            CodecLimits {
                read_chunk_bytes: chunk,
                ..CodecLimits::default()
            },
        )
        .unwrap();
        let header = reader.next_record().unwrap().unwrap();
        assert_eq!(header.fields, ["같은", "같은"]);
        assert_eq!(header.byte_start, 0);
        assert_eq!(header.row_index, 0);
        assert_eq!(
            &bytes[header.byte_start as usize..header.byte_end as usize],
            "\u{feff}같은,같은\r\n".as_bytes()
        );
        let row = reader.next_record().unwrap().unwrap();
        assert_eq!(row.fields, ["첫줄\r\n다음 \"인용\" é", "값"]);
        assert_eq!(row.byte_start, header.byte_end);
        assert_eq!(row.row_index, 1);
        let blank = reader.next_record().unwrap().unwrap();
        assert_eq!(blank.fields, [""]);
        assert_eq!(blank.byte_start, row.byte_end);
        assert_eq!(blank.byte_end - blank.byte_start, 1);
        let end = reader.next_record().unwrap().unwrap();
        assert_eq!(end.fields, ["끝", ""]);
        assert_eq!(end.byte_start, blank.byte_end);
        assert_eq!(end.byte_end as usize, bytes.len());
        assert_eq!(end.row_index, 3);
        assert!(reader.next_record().unwrap().is_none());
        assert!(reader.next_record().unwrap().is_none());
    }
}
#[test]
fn strict_quote_and_utf8_errors_are_not_silently_repaired() {
    for source in [
        b"a,\"unterminated".as_slice(),
        b"a,\"closed\"junk\n",
        b"bare\"quote,b\n",
    ] {
        let mut reader = DelimitedReader::new(
            Tiny {
                reader: Cursor::new(source),
                chunk: 1,
            },
            b',',
            CodecLimits::default(),
        )
        .unwrap();
        assert_eq!(
            reader.next_record().unwrap_err(),
            WorkbenchError::InvalidInput
        );
        assert!(reader.next_record().is_err());
    }
    let mut reader =
        DelimitedReader::new(Cursor::new(b"name\n\xe2\x82"), b',', CodecLimits::default()).unwrap();
    reader.next_record().unwrap();
    assert_eq!(
        reader.next_record().unwrap_err(),
        WorkbenchError::Unsupported
    );
}
#[test]
fn exact_field_column_and_raw_record_bounds_refuse_before_an_unbounded_row() {
    let limits = CodecLimits {
        max_field_bytes: 3,
        max_record_bytes: 8,
        max_fields: 2,
        read_chunk_bytes: 1,
    };
    let mut reader = DelimitedReader::new(Cursor::new(b"abc,x\r\n"), b',', limits).unwrap();
    assert_eq!(reader.next_record().unwrap().unwrap().fields, ["abc", "x"]);
    let mut exact = DelimitedReader::new(
        Cursor::new(b"ab,cd\r\n"),
        b',',
        CodecLimits {
            max_record_bytes: 7,
            ..limits
        },
    )
    .unwrap();
    assert_eq!(exact.next_record().unwrap().unwrap().byte_end, 7);
    let mut over = DelimitedReader::new(
        Cursor::new(b"ab,cd\r\n"),
        b',',
        CodecLimits {
            max_record_bytes: 6,
            ..limits
        },
    )
    .unwrap();
    assert_eq!(over.next_record().unwrap_err(), WorkbenchError::Limit);
    for source in [
        b"abcd,x\n".as_slice(),
        b"a,b,c\n",
        b"\"\"\"\"\"\"\"\"\"\"\"\"\n",
    ] {
        let mut reader = DelimitedReader::new(Cursor::new(source), b',', limits).unwrap();
        assert_eq!(reader.next_record().unwrap_err(), WorkbenchError::Limit);
    }
    assert!(DelimitedReader::new(Cursor::new(b""), b';', limits).is_err());
    assert!(DelimitedReader::new(
        Cursor::new(b""),
        b',',
        CodecLimits {
            max_fields: 513,
            ..limits
        }
    )
    .is_err());
}
#[test]
fn null_empty_and_literal_backslash_are_distinct_and_headers_stay_raw() {
    let bytes = b"\\N,\\\\N,\"\"\n\\N,\\\\N,\"\"\n";
    let mut reader =
        DelimitedReader::new(Cursor::new(bytes), b',', CodecLimits::default()).unwrap();
    assert_eq!(
        reader.next_record().unwrap().unwrap().fields,
        [r"\N", r"\\N", ""]
    );
    let fields = reader.next_record().unwrap().unwrap().fields;
    assert_eq!(
        fields
            .iter()
            .map(|value| decode_delimited_value(value))
            .collect::<Vec<_>>(),
        [None, Some(r"\N"), Some("")]
    );
}
#[test]
fn csv_and_tsv_export_preserve_scalar_lexemes_and_quote_syntax() {
    let cells = vec![
        TypedCell::Null,
        text(""),
        text(r"\N"),
        TypedCell::U64 {
            value: "18446744073709551615".into(),
        },
        TypedCell::Decimal {
            value: "12345678901234567890.000000001".into(),
            scale: Some(9),
        },
        text("é한\t,\r\n\"quoted\""),
    ];
    for (format, delimiter) in [(ExportFormat::Csv, b','), (ExportFormat::Tsv, b'\t')] {
        let mut writer = ExportWriter::new(
            Vec::new(),
            format,
            &columns(&["n", "empty", "literal", "uint", "decimal", "text"]),
            FormulaPolicy::RawConfirmed,
        )
        .unwrap();
        writer.write_row(&cells).unwrap();
        let (output, stats) = writer.finish_with_stats().unwrap();
        assert_eq!(stats.rows, 1);
        assert_eq!(stats.bytes as usize, output.len());
        let mut reader = DelimitedReader::new(
            Tiny {
                reader: Cursor::new(&output),
                chunk: 1,
            },
            delimiter,
            CodecLimits::default(),
        )
        .unwrap();
        reader.next_record().unwrap();
        let row = reader.next_record().unwrap().unwrap();
        let values: Vec<_> = row
            .fields
            .iter()
            .map(|value| decode_delimited_value(value))
            .collect();
        assert_eq!(
            values,
            [
                None,
                Some(""),
                Some(r"\N"),
                Some("18446744073709551615"),
                Some("12345678901234567890.000000001"),
                Some("é한\t,\r\n\"quoted\"")
            ]
        );
    }
}
#[test]
fn spreadsheet_escape_applies_to_text_headers_and_controls_but_not_numeric_lexemes() {
    for policy in [FormulaPolicy::Escape, FormulaPolicy::RawConfirmed] {
        let mut writer = ExportWriter::new(
            Vec::new(),
            ExportFormat::Csv,
            &columns(&["=header", "text", "number"]),
            policy,
        )
        .unwrap();
        writer
            .write_row(&[
                text(" =SUM(1)"),
                text("\t@cmd"),
                TypedCell::I64 { value: "-7".into() },
            ])
            .unwrap();
        let output = writer.finish().unwrap();
        let mut reader =
            DelimitedReader::new(Cursor::new(output), b',', CodecLimits::default()).unwrap();
        let header = reader.next_record().unwrap().unwrap();
        let row = reader.next_record().unwrap().unwrap();
        assert_eq!(
            header.fields[0],
            if policy == FormulaPolicy::Escape {
                "'=header"
            } else {
                "=header"
            }
        );
        assert_eq!(
            row.fields[0],
            if policy == FormulaPolicy::Escape {
                "' =SUM(1)"
            } else {
                " =SUM(1)"
            }
        );
        assert_eq!(
            row.fields[1],
            if policy == FormulaPolicy::Escape {
                "'\t@cmd"
            } else {
                "\t@cmd"
            }
        );
        assert_eq!(row.fields[2], "-7");
    }
}
#[test]
fn typed_json_preserves_numeric_string_tags_bytes_null_and_temporal_metadata() {
    let row = vec![
        TypedCell::I64 {
            value: "-9223372036854775808".into(),
        },
        TypedCell::U64 {
            value: "18446744073709551615".into(),
        },
        TypedCell::Decimal {
            value: "12345678901234567890.000000001".into(),
            scale: Some(9),
        },
        TypedCell::Bytes {
            base64: "AP8=".into(),
        },
        TypedCell::Temporal {
            value: "2026-11-17 12:34:56.123456789".into(),
            type_name: "timestamp".into(),
            offset: Some("+09:00".into()),
            precision: 9,
        },
        TypedCell::Null,
        text(""),
        TypedCell::Float {
            value: "NaN".into(),
        },
    ];
    let mut writer = ExportWriter::new(
        Vec::new(),
        ExportFormat::TypedJson,
        &columns(&["i", "u", "d", "b", "t", "n", "e", "f"]),
        FormulaPolicy::Escape,
    )
    .unwrap();
    writer.write_row(&row).unwrap();
    let (bytes, stats) = writer.finish_with_stats().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let restored: Vec<TypedCell> = serde_json::from_value(value["rows"][0].clone()).unwrap();
    assert_eq!(restored, row);
    assert_eq!(value["complete"], true);
    assert!(value["rows"][0][1]["value"].is_string());
    assert_eq!(stats.bytes as usize, bytes.len());
}
#[test]
fn large_unsupported_oversized_and_io_failures_cannot_finish_as_complete() {
    let refused = [
        TypedCell::Unsupported {
            type_name: "opaque".into(),
            reason: "not_proven".into(),
        },
        TypedCell::Large {
            value_handle: Uuid::new_v4(),
            type_name: "blob".into(),
            byte_length: 100000u64.into(),
            preview: "not the original".into(),
        },
    ];
    for cell in refused {
        let mut writer = ExportWriter::new(
            Vec::new(),
            ExportFormat::TypedJson,
            &columns(&["one"]),
            FormulaPolicy::Escape,
        )
        .unwrap();
        assert_eq!(
            writer.write_row(&[cell]).unwrap_err(),
            WorkbenchError::Unsupported
        );
        assert!(writer.finish().is_err());
    }
    let mut writer = ExportWriter::new(
        Vec::new(),
        ExportFormat::Csv,
        &columns(&["one"]),
        FormulaPolicy::Escape,
    )
    .unwrap();
    assert_eq!(
        writer.write_row(&[text(&"x".repeat(65537))]).unwrap_err(),
        WorkbenchError::Limit
    );
    assert!(writer.finish().is_err());
    struct Broken;
    impl Write for Broken {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("owned write fault"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    assert!(matches!(
        ExportWriter::new(
            Broken,
            ExportFormat::TypedJson,
            &columns(&["one"]),
            FormulaPolicy::Escape
        ),
        Err(WorkbenchError::Io)
    ));
}
#[test]
fn lazy_large_input_is_read_in_fixed_chunks_without_collecting_the_source() {
    struct Lazy {
        remaining: usize,
    }
    impl Read for Lazy {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            let n = output.len().min(self.remaining);
            for (index, byte) in output[..n].iter_mut().enumerate() {
                *byte = b"x,y\n"[index % 4];
            }
            self.remaining -= n;
            Ok(n)
        }
    }
    let mut reader = DelimitedReader::new(
        Lazy {
            remaining: 1024 * 1024 * 1024,
        },
        b',',
        CodecLimits::default(),
    )
    .unwrap();
    let capacity = reader.buffer_capacity_bytes();
    for ordinal in 0..1000 {
        let row = reader.next_record().unwrap().unwrap();
        assert_eq!(row.fields, ["x", "y"]);
        assert_eq!(row.row_index, ordinal);
        assert_eq!(row.byte_start, ordinal * 4);
        assert_eq!(reader.buffer_capacity_bytes(), capacity);
    }
    assert_eq!(reader.byte_position(), 4000);
    assert!(capacity < 96 * 1024);
}
