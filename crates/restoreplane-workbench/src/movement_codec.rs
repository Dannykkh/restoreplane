//! Bounded lexical codecs only. Jobs own mapping, authority, transactions and
//! source completion/flush/sync/finalization. No file is complete because this
//! module finished its JSON syntax. Delimited scalar text is not typed JSON.
use crate::WorkbenchError;
use csv_core::{ReadFieldResult, WriteResult};
use restoreplane_domain::workbench::{Column, TypedCell};
use std::borrow::Cow;
use std::io::{Read, Write};

const RECORD_LIMIT: usize = 256 * 1024;
const FIELD_LIMIT: usize = 64 * 1024;
const FIELD_COUNT: usize = 512;
const WRITE_CHUNK: usize = 16 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct CodecLimits {
    pub max_record_bytes: usize,
    pub max_field_bytes: usize,
    pub max_fields: usize,
    pub read_chunk_bytes: usize,
}
impl Default for CodecLimits {
    fn default() -> Self {
        Self {
            max_record_bytes: RECORD_LIMIT,
            max_field_bytes: FIELD_LIMIT,
            max_fields: FIELD_COUNT,
            read_chunk_bytes: WRITE_CHUNK,
        }
    }
}
impl CodecLimits {
    fn validate(self) -> Result<Self, WorkbenchError> {
        if self.max_record_bytes == 0
            || self.max_record_bytes > RECORD_LIMIT
            || self.max_field_bytes == 0
            || self.max_field_bytes > FIELD_LIMIT
            || self.max_fields == 0
            || self.max_fields > FIELD_COUNT
            || self.read_chunk_bytes == 0
            || self.read_chunk_bytes > RECORD_LIMIT
        {
            return Err(WorkbenchError::Limit);
        }
        Ok(self)
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct DelimitedRecord {
    /// Raw unquoted UTF-8 fields. In particular, header names are never NULL decoded.
    pub fields: Vec<String>,
    pub byte_start: u64,
    pub byte_end: u64,
    /// Zero-based physical record ordinal; a header is record zero.
    pub row_index: u64,
}
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Syntax {
    #[default]
    Start,
    Unquoted,
    Quoted,
    Closed,
}
impl Syntax {
    fn feed(&mut self, bytes: &[u8], delimiter: u8) -> Result<(), WorkbenchError> {
        for &byte in bytes {
            *self = match (*self, byte) {
                (Self::Start, b'"') => Self::Quoted,
                (Self::Start | Self::Unquoted | Self::Closed, value)
                    if value == delimiter || matches!(value, b'\r' | b'\n') =>
                {
                    Self::Start
                }
                (Self::Start, _) => Self::Unquoted,
                (Self::Unquoted, b'"') => return Err(WorkbenchError::InvalidInput),
                (Self::Unquoted, _) => Self::Unquoted,
                (Self::Quoted, b'"') => Self::Closed,
                (Self::Quoted, _) => Self::Quoted,
                (Self::Closed, b'"') => Self::Quoted,
                (Self::Closed, _) => return Err(WorkbenchError::InvalidInput),
            };
        }
        Ok(())
    }
}
pub struct DelimitedReader<R: Read> {
    reader: R,
    parser: csv_core::Reader,
    delimiter: u8,
    limits: CodecLimits,
    input: Vec<u8>,
    position: usize,
    available: usize,
    field: Vec<u8>,
    offset: u64,
    row: u64,
    eof: bool,
    poisoned: bool,
}
impl<R: Read> DelimitedReader<R> {
    pub fn new(reader: R, delimiter: u8, limits: CodecLimits) -> Result<Self, WorkbenchError> {
        if !matches!(delimiter, b',' | b'\t') {
            return Err(WorkbenchError::Unsupported);
        }
        let limits = limits.validate()?;
        Ok(Self {
            reader,
            parser: csv_core::ReaderBuilder::new().delimiter(delimiter).build(),
            delimiter,
            limits,
            input: vec![0; limits.read_chunk_bytes.max(4)],
            position: 0,
            available: 0,
            field: vec![0; limits.max_field_bytes + 1],
            offset: 0,
            row: 0,
            eof: false,
            poisoned: false,
        })
    }
    pub fn byte_position(&self) -> u64 {
        self.offset
    }
    pub fn record_index(&self) -> u64 {
        self.row
    }
    pub fn buffer_capacity_bytes(&self) -> usize {
        self.input.len() + self.field.len()
    }
    fn refill(&mut self) -> Result<(), WorkbenchError> {
        if self.position < self.available || self.eof {
            return Ok(());
        }
        self.position = 0;
        self.available = 0;
        // The first four bytes let csv-core see a BOM even with one-byte reads.
        let minimum = if self.offset == 0 { 4 } else { 1 };
        while self.available < minimum && !self.eof {
            let end = self
                .input
                .len()
                .min(self.available + self.limits.read_chunk_bytes);
            let read = match self.reader.read(&mut self.input[self.available..end]) {
                Ok(count) => count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(WorkbenchError::Io),
            };
            if read == 0 {
                self.eof = true;
            } else {
                self.available += read;
            }
        }
        Ok(())
    }
    fn advance(&mut self, count: usize) -> Result<(), WorkbenchError> {
        self.position += count;
        self.offset = self
            .offset
            .checked_add(count as u64)
            .ok_or(WorkbenchError::Limit)?;
        Ok(())
    }
    fn trailing_lf(&mut self, last: Option<u8>) -> Result<(), WorkbenchError> {
        if last != Some(b'\r') {
            return Ok(());
        }
        self.refill()?;
        if self
            .input
            .get(self.position)
            .copied()
            .filter(|_| self.position < self.available)
            == Some(b'\n')
        {
            let (_, used, written) = self.parser.read_field(b"\n", &mut self.field);
            if used != 1 || written != 0 {
                return Err(WorkbenchError::InvalidInput);
            }
            self.advance(1)?;
        }
        Ok(())
    }
    pub fn next_record(&mut self) -> Result<Option<DelimitedRecord>, WorkbenchError> {
        if self.poisoned {
            return Err(WorkbenchError::InvalidInput);
        }
        let result = self.read_record();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn read_record(&mut self) -> Result<Option<DelimitedRecord>, WorkbenchError> {
        self.refill()?;
        let start = self.offset;
        let bom = if self.offset == 0 && self.input[..self.available].starts_with(b"\xef\xbb\xbf") {
            3
        } else {
            0
        };
        // csv-core intentionally skips blank lines. Preserve them as one empty
        // field instead, so an input row is never silently dropped.
        if self
            .input
            .get(self.position + bom)
            .copied()
            .filter(|_| self.position + bom < self.available)
            .is_some_and(|byte| matches!(byte, b'\r' | b'\n'))
        {
            let used = bom + 1;
            let last = self.input[self.position + used - 1];
            let (_, consumed, written) = self.parser.read_field(
                &self.input[self.position..self.position + used],
                &mut self.field,
            );
            if consumed != used || written != 0 {
                return Err(WorkbenchError::InvalidInput);
            }
            self.advance(used)?;
            self.trailing_lf(Some(last))?;
            return self.record(start, vec![String::new()]).map(Some);
        }
        let mut fields = Vec::new();
        let mut output = 0usize;
        let mut syntax = Syntax::Start;
        loop {
            self.refill()?;
            if self.position == self.available && self.eof && syntax == Syntax::Quoted {
                return Err(WorkbenchError::InvalidInput);
            }
            let first = self.position;
            let (result, used, written) = self.parser.read_field(
                &self.input[first..self.available],
                &mut self.field[output..],
            );
            let skip_bom = if self.offset == 0
                && used >= 3
                && self.input[first..first + used].starts_with(b"\xef\xbb\xbf")
            {
                3
            } else {
                0
            };
            syntax.feed(&self.input[first + skip_bom..first + used], self.delimiter)?;
            let last = used.checked_sub(1).map(|index| self.input[first + index]);
            self.advance(used)?;
            output += written;
            if output > self.limits.max_field_bytes
                || self.offset - start > self.limits.max_record_bytes as u64
            {
                return Err(WorkbenchError::Limit);
            }
            match result {
                ReadFieldResult::Field { record_end } => {
                    if fields.len() >= self.limits.max_fields {
                        return Err(WorkbenchError::Limit);
                    }
                    fields.push(
                        std::str::from_utf8(&self.field[..output])
                            .map_err(|_| WorkbenchError::Unsupported)?
                            .to_owned(),
                    );
                    output = 0;
                    if record_end {
                        self.trailing_lf(last)?;
                        return self.record(start, fields).map(Some);
                    }
                }
                ReadFieldResult::InputEmpty => {
                    if used == 0 && self.eof {
                        return Err(WorkbenchError::InvalidInput);
                    }
                }
                ReadFieldResult::OutputFull => return Err(WorkbenchError::Limit),
                ReadFieldResult::End => {
                    if fields.is_empty() && output == 0 {
                        return Ok(None);
                    }
                    return Err(WorkbenchError::InvalidInput);
                }
            }
        }
    }
    fn record(
        &mut self,
        start: u64,
        fields: Vec<String>,
    ) -> Result<DelimitedRecord, WorkbenchError> {
        if self.offset - start > self.limits.max_record_bytes as u64 {
            return Err(WorkbenchError::Limit);
        }
        let record = DelimitedRecord {
            fields,
            byte_start: start,
            byte_end: self.offset,
            row_index: self.row,
        };
        self.row = self.row.checked_add(1).ok_or(WorkbenchError::Limit)?;
        Ok(record)
    }
}
/// Apply only to data fields, after raw header/ordinal handling. NULL detection
/// precedes reversal of a leading backslash escape.
pub fn decode_delimited_value(value: &str) -> Option<&str> {
    if value == r"\N" {
        None
    } else if value.starts_with(r"\\") {
        Some(&value[1..])
    } else {
        Some(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    Csv,
    Tsv,
    TypedJson,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormulaPolicy {
    Escape,
    RawConfirmed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodecStats {
    pub rows: u64,
    pub bytes: u64,
}
struct Counted<W> {
    inner: W,
    bytes: u64,
}
impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let count = self.inner.write(bytes)?;
        self.bytes = self
            .bytes
            .checked_add(count as u64)
            .ok_or_else(|| std::io::Error::other("codec byte limit"))?;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
struct RecordCount {
    bytes: usize,
}
impl Write for RecordCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("record limit"))?;
        if self.bytes > RECORD_LIMIT {
            return Err(std::io::Error::other("record limit"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn formula_like(value: &str) -> bool {
    value.chars().next().is_some_and(char::is_control)
        || value
            .trim_start_matches(|value: char| {
                value.is_whitespace() || value.is_control() || value == '\u{feff}'
            })
            .starts_with(['=', '+', '-', '@'])
}
fn text_field(value: &str, policy: FormulaPolicy, data: bool) -> Cow<'_, str> {
    if policy == FormulaPolicy::Escape && formula_like(value) {
        Cow::Owned(format!("'{value}"))
    } else if data && value.starts_with('\\') {
        Cow::Owned(format!("\\{value}"))
    } else {
        Cow::Borrowed(value)
    }
}
fn scalar(cell: &TypedCell, policy: FormulaPolicy) -> Result<Cow<'_, str>, WorkbenchError> {
    let value = match cell {
        TypedCell::Null => Cow::Borrowed(r"\N"),
        TypedCell::Bool { value } => Cow::Borrowed(if *value { "true" } else { "false" }),
        TypedCell::Text { value } => text_field(value, policy, true),
        TypedCell::I64 { value }
        | TypedCell::U64 { value }
        | TypedCell::Decimal { value, .. }
        | TypedCell::Float { value }
        | TypedCell::Temporal { value, .. } => Cow::Borrowed(value.as_str()),
        TypedCell::Bytes { base64 } => Cow::Borrowed(base64.as_str()),
        TypedCell::Unsupported { .. } | TypedCell::Large { .. } => {
            return Err(WorkbenchError::Unsupported)
        }
    };
    if value.len() > FIELD_LIMIT {
        return Err(WorkbenchError::Limit);
    }
    Ok(value)
}
fn csv_writer(delimiter: u8) -> csv_core::Writer {
    csv_core::WriterBuilder::new().delimiter(delimiter).build()
}
fn delimited<W: Write>(
    writer: &mut W,
    core: &mut csv_core::Writer,
    fields: &[Cow<'_, str>],
) -> Result<(), WorkbenchError> {
    let mut output = [0u8; WRITE_CHUNK];
    for (index, value) in fields.iter().enumerate() {
        if index > 0 {
            loop {
                let (result, written) = core.delimiter(&mut output);
                writer
                    .write_all(&output[..written])
                    .map_err(|_| WorkbenchError::Io)?;
                if result == WriteResult::InputEmpty {
                    break;
                }
            }
        }
        let mut position = 0;
        loop {
            let (result, used, written) = core.field(&value.as_bytes()[position..], &mut output);
            position += used;
            writer
                .write_all(&output[..written])
                .map_err(|_| WorkbenchError::Io)?;
            if result == WriteResult::InputEmpty {
                break;
            }
        }
    }
    loop {
        let (result, written) = core.terminator(&mut output);
        writer
            .write_all(&output[..written])
            .map_err(|_| WorkbenchError::Io)?;
        if result == WriteResult::InputEmpty {
            break;
        }
    }
    Ok(())
}
pub struct ExportWriter<W: Write> {
    writer: Counted<W>,
    format: ExportFormat,
    formula: FormulaPolicy,
    columns: usize,
    rows: u64,
    csv: csv_core::Writer,
    poisoned: bool,
}
impl<W: Write> ExportWriter<W> {
    pub fn new(
        writer: W,
        format: ExportFormat,
        columns: &[Column],
        formula: FormulaPolicy,
    ) -> Result<Self, WorkbenchError> {
        if columns.is_empty() || columns.len() > FIELD_COUNT {
            return Err(WorkbenchError::Limit);
        }
        crate::result_stream::encoded_size(&columns, RECORD_LIMIT)?;
        let delimiter = if format == ExportFormat::Tsv {
            b'\t'
        } else {
            b','
        };
        let mut result = Self {
            writer: Counted {
                inner: writer,
                bytes: 0,
            },
            format,
            formula,
            columns: columns.len(),
            rows: 0,
            csv: csv_writer(delimiter),
            poisoned: false,
        };
        if format == ExportFormat::TypedJson {
            result
                .writer
                .write_all(b"{\"columns\":")
                .map_err(|_| WorkbenchError::Io)?;
            serde_json::to_writer(&mut result.writer, columns).map_err(|_| WorkbenchError::Io)?;
            result
                .writer
                .write_all(b",\"rows\":[")
                .map_err(|_| WorkbenchError::Io)?;
        } else {
            let fields: Vec<_> = columns
                .iter()
                .map(|column| text_field(&column.name, formula, false))
                .collect();
            if fields.iter().any(|field| field.len() > FIELD_LIMIT) {
                return Err(WorkbenchError::Limit);
            }
            delimited(
                &mut RecordCount { bytes: 0 },
                &mut csv_writer(delimiter),
                &fields,
            )
            .map_err(|_| WorkbenchError::Limit)?;
            delimited(&mut result.writer, &mut result.csv, &fields)?;
        }
        Ok(result)
    }
    pub fn rows_written(&self) -> u64 {
        self.rows
    }
    pub fn bytes_written(&self) -> u64 {
        self.writer.bytes
    }
    pub fn stats(&self) -> CodecStats {
        CodecStats {
            rows: self.rows,
            bytes: self.writer.bytes,
        }
    }
    pub fn write_row(&mut self, cells: &[TypedCell]) -> Result<(), WorkbenchError> {
        if self.poisoned {
            return Err(WorkbenchError::Io);
        }
        let result = self.write_checked(cells);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }
    fn write_checked(&mut self, cells: &[TypedCell]) -> Result<(), WorkbenchError> {
        if cells.len() != self.columns {
            return Err(WorkbenchError::InvalidInput);
        }
        if cells.iter().any(|cell| {
            matches!(
                cell,
                TypedCell::Large { .. } | TypedCell::Unsupported { .. }
            )
        }) {
            return Err(WorkbenchError::Unsupported);
        }
        crate::types::validate_cells(cells)?;
        let next = self.rows.checked_add(1).ok_or(WorkbenchError::Limit)?;
        if self.format == ExportFormat::TypedJson {
            if self.rows > 0 {
                self.writer
                    .write_all(b",")
                    .map_err(|_| WorkbenchError::Io)?;
            }
            serde_json::to_writer(&mut self.writer, cells).map_err(|_| WorkbenchError::Io)?;
        } else {
            let fields = cells
                .iter()
                .map(|cell| scalar(cell, self.formula))
                .collect::<Result<Vec<_>, _>>()?;
            let delimiter = if self.format == ExportFormat::Tsv {
                b'\t'
            } else {
                b','
            };
            delimited(
                &mut RecordCount { bytes: 0 },
                &mut csv_writer(delimiter),
                &fields,
            )
            .map_err(|_| WorkbenchError::Limit)?;
            delimited(&mut self.writer, &mut self.csv, &fields)?;
        }
        self.rows = next;
        Ok(())
    }
    pub fn finish(self) -> Result<W, WorkbenchError> {
        self.finish_with_stats().map(|(writer, _)| writer)
    }
    pub fn finish_with_stats(mut self) -> Result<(W, CodecStats), WorkbenchError> {
        if self.poisoned {
            return Err(WorkbenchError::Io);
        }
        if self.format == ExportFormat::TypedJson {
            self.writer
                .write_all(b"],\"complete\":true}")
                .map_err(|_| WorkbenchError::Io)?;
        } else {
            let mut output = [0u8; WRITE_CHUNK];
            loop {
                let (result, written) = self.csv.finish(&mut output);
                self.writer
                    .write_all(&output[..written])
                    .map_err(|_| WorkbenchError::Io)?;
                if result == WriteResult::InputEmpty {
                    break;
                }
            }
        }
        let stats = self.stats();
        Ok((self.writer.inner, stats))
    }
}
