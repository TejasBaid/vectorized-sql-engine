//! CSV reader with type inference, and a CSV writer.
//!
//! * The delimiter is sniffed from the header line (`,`, `|`, tab or `;`).
//! * Column types are inferred from a sample of rows using the lattice
//!   `NULL < BOOLEAN`, `NULL < BIGINT < DOUBLE`, `NULL < DATE`, anything else
//!   is `VARCHAR`. If a later value does not fit, the column is widened and the
//!   file is re-parsed, so inference never produces wrong results.
//! * Files without quoted fields are parsed in parallel: the buffer is split
//!   at newline boundaries and each chunk is decoded on its own thread.

use crate::catalog::MemTable;
use crate::column::{BATCH_SIZE, Batch, ColumnBuilder, ColumnData, Field, Schema};
use crate::err;
use crate::error::Result;
use crate::types::{DataType, format_date, parse_date};
use std::borrow::Cow;
use std::io::Write;

#[derive(Debug, Clone)]
pub struct CsvOptions {
    pub delimiter: Option<u8>,
    pub has_header: bool,
    /// Number of rows sampled for type inference.
    pub infer_rows: usize,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions {
            delimiter: None,
            has_header: true,
            infer_rows: 20_000,
        }
    }
}

pub fn read_csv(path: &str, opts: &CsvOptions) -> Result<MemTable> {
    let bytes = std::fs::read(path).map_err(|e| crate::Error::new(crate::ErrorKind::Io, format!("{path}: {e}")))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| crate::Error::new(crate::ErrorKind::Io, format!("{path}: file is not valid UTF-8")))?;
    read_csv_str(&text, opts)
}

/// Read one record starting at `*pos`. Returns false at end of input.
fn read_record<'a>(s: &'a str, pos: &mut usize, delim: u8, fields: &mut Vec<Cow<'a, str>>) -> bool {
    fields.clear();
    let b = s.as_bytes();
    if *pos >= b.len() {
        return false;
    }
    loop {
        // One field.
        if *pos < b.len() && b[*pos] == b'"' {
            *pos += 1;
            let mut out = String::new();
            let mut start = *pos;
            loop {
                if *pos >= b.len() {
                    out.push_str(&s[start..]);
                    break;
                }
                if b[*pos] == b'"' {
                    out.push_str(&s[start..*pos]);
                    if *pos + 1 < b.len() && b[*pos + 1] == b'"' {
                        out.push('"');
                        *pos += 2;
                        start = *pos;
                        continue;
                    }
                    *pos += 1;
                    break;
                }
                *pos += 1;
            }
            // Skip to delimiter / end of line.
            while *pos < b.len() && b[*pos] != delim && b[*pos] != b'\n' {
                *pos += 1;
            }
            fields.push(Cow::Owned(out));
        } else {
            let start = *pos;
            while *pos < b.len() && b[*pos] != delim && b[*pos] != b'\n' {
                *pos += 1;
            }
            let mut end = *pos;
            if end > start && b[end - 1] == b'\r' {
                end -= 1;
            }
            fields.push(Cow::Borrowed(&s[start..end]));
        }
        if *pos >= b.len() {
            return true;
        }
        if b[*pos] == b'\n' {
            *pos += 1;
            return true;
        }
        *pos += 1; // delimiter
    }
}

fn sniff_delimiter(first_line: &str) -> u8 {
    b",|\t;"
        .iter()
        .copied()
        .max_by_key(|d| first_line.bytes().filter(|b| b == d).count())
        .unwrap_or(b',')
}

/// Join two inferred types.
fn widen(cur: DataType, value: &str) -> DataType {
    use DataType::*;
    let v = value.trim();
    let fits = |t: DataType| match t {
        Null => false,
        Boolean => v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false"),
        Int64 => v.parse::<i64>().is_ok(),
        Float64 => {
            v.parse::<f64>().is_ok()
                && !v.eq_ignore_ascii_case("nan")
                && !v.eq_ignore_ascii_case("inf")
                && !v.eq_ignore_ascii_case("infinity")
        }
        Date => parse_date(v).is_some(),
        _ => true,
    };
    if cur != Null && fits(cur) {
        return cur;
    }
    let candidates: &[DataType] = match cur {
        Null => &[Boolean, Int64, Float64, Date, Utf8],
        Int64 => &[Float64, Utf8],
        _ => &[Utf8],
    };
    candidates.iter().copied().find(|t| fits(*t)).unwrap_or(Utf8)
}

/// Failure to parse a value under the current type assignment.
struct Mismatch {
    col: usize,
    value: String,
}

fn parse_chunk(text: &str, delim: u8, types: &[DataType]) -> std::result::Result<Vec<Batch>, Mismatch> {
    let mut batches = vec![];
    let mut builders: Vec<ColumnBuilder> = types
        .iter()
        .map(|t| ColumnBuilder::with_capacity(*t, BATCH_SIZE))
        .collect();
    let mut fields = Vec::with_capacity(types.len());
    let mut pos = 0;
    let mut rows = 0;
    while read_record(text, &mut pos, delim, &mut fields) {
        if fields.len() == 1 && fields[0].is_empty() {
            continue; // blank line
        }
        for (i, b) in builders.iter_mut().enumerate() {
            let v = fields.get(i).map(|c| c.as_ref()).unwrap_or("");
            if v.is_empty() {
                b.push_null();
                continue;
            }
            let ok = match types[i] {
                DataType::Int64 => v.trim().parse::<i64>().map(|x| b.push_i64(x)).is_ok(),
                DataType::Float64 => v.trim().parse::<f64>().map(|x| b.push_f64(x)).is_ok(),
                DataType::Date => parse_date(v).map(|x| b.push_date(x)).is_some(),
                DataType::Boolean => match v.trim().to_ascii_lowercase().as_str() {
                    "true" => {
                        b.push_bool(true);
                        true
                    }
                    "false" => {
                        b.push_bool(false);
                        true
                    }
                    _ => false,
                },
                DataType::Null => {
                    b.push_null();
                    true
                }
                _ => {
                    b.push_str(v);
                    true
                }
            };
            if !ok {
                return Err(Mismatch {
                    col: i,
                    value: v.to_string(),
                });
            }
        }
        rows += 1;
        if rows == BATCH_SIZE {
            let done = std::mem::replace(
                &mut builders,
                types
                    .iter()
                    .map(|t| ColumnBuilder::with_capacity(*t, BATCH_SIZE))
                    .collect(),
            );
            batches.push(Batch::from_columns(done.into_iter().map(|b| b.finish()).collect()));
            rows = 0;
        }
    }
    if rows > 0 {
        batches.push(Batch::from_columns(builders.into_iter().map(|b| b.finish()).collect()));
    }
    Ok(batches)
}

/// Split `text` into roughly equal chunks ending at newlines.
fn split_chunks(text: &str, n: usize) -> Vec<&str> {
    let b = text.as_bytes();
    let target = (b.len() / n).max(1 << 20);
    let mut chunks = vec![];
    let mut start = 0;
    while start < b.len() {
        let mut end = (start + target).min(b.len());
        while end < b.len() && b[end - 1] != b'\n' {
            end += 1;
        }
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

pub fn read_csv_str(text: &str, opts: &CsvOptions) -> Result<MemTable> {
    let first_line = text.lines().next().unwrap_or("");
    let delim = opts.delimiter.unwrap_or_else(|| sniff_delimiter(first_line));
    let mut pos = 0;
    let mut fields = vec![];
    let names: Vec<String> = if opts.has_header {
        if !read_record(text, &mut pos, delim, &mut fields) {
            return err!(Io, "CSV input is empty");
        }
        fields.iter().map(|f| f.trim().to_lowercase()).collect()
    } else {
        let mut p = 0;
        read_record(text, &mut p, delim, &mut fields);
        (1..=fields.len()).map(|i| format!("column{i}")).collect()
    };
    let body = &text[pos..];
    // Type inference on a sample.
    let mut types = vec![DataType::Null; names.len()];
    let mut p = 0;
    let mut sampled = 0;
    while sampled < opts.infer_rows && read_record(body, &mut p, delim, &mut fields) {
        for (i, f) in fields.iter().enumerate().take(names.len()) {
            if !f.is_empty() {
                types[i] = widen(types[i], f);
            }
        }
        sampled += 1;
    }
    for t in types.iter_mut() {
        if *t == DataType::Null {
            *t = DataType::Utf8;
        }
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let parallel = !body.contains('"') && body.len() > (4 << 20);
    let batches = loop {
        let result: std::result::Result<Vec<Batch>, Mismatch> = if parallel {
            let chunks = split_chunks(body, threads);
            let types_ref = &types;
            let parts: Vec<_> = std::thread::scope(|s| {
                let hs: Vec<_> = chunks
                    .iter()
                    .map(|c| s.spawn(move || parse_chunk(c, delim, types_ref)))
                    .collect();
                hs.into_iter().map(|h| h.join().expect("csv worker panicked")).collect()
            });
            parts.into_iter().try_fold(vec![], |mut acc, p| {
                acc.extend(p?);
                Ok(acc)
            })
        } else {
            parse_chunk(body, delim, &types)
        };
        match result {
            Ok(b) => break b,
            Err(m) => {
                let new = widen(types[m.col], &m.value);
                if new == types[m.col] {
                    return err!(Io, "could not parse value '{}' in column \"{}\"", m.value, names[m.col]);
                }
                types[m.col] = new;
            }
        }
    };
    let fields = names.iter().zip(&types).map(|(n, t)| Field::new(n, *t, true)).collect();
    Ok(MemTable::new(Schema::new(fields), batches))
}

fn csv_escape(s: &str, delim: u8) -> Cow<'_, str> {
    if s.bytes().any(|b| b == delim || b == b'"' || b == b'\n' || b == b'\r') {
        Cow::Owned(format!("\"{}\"", s.replace('"', "\"\"")))
    } else {
        Cow::Borrowed(s)
    }
}

/// Write a table as CSV with a header row.
pub fn write_csv(schema: &Schema, batches: &[Batch], path: &str) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::with_capacity(1 << 20, file);
    let header: Vec<&str> = schema.fields.iter().map(|f| f.name.as_str()).collect();
    writeln!(w, "{}", header.join(","))?;
    let mut line = String::new();
    for b in batches {
        for r in 0..b.num_rows {
            line.clear();
            for (ci, c) in b.columns.iter().enumerate() {
                if ci > 0 {
                    line.push(',');
                }
                if !c.is_valid(r) {
                    continue;
                }
                match &c.data {
                    ColumnData::Int64(v) => line.push_str(&v[r].to_string()),
                    ColumnData::Float64(v) => line.push_str(&format!("{:?}", v[r])),
                    ColumnData::Date(v) => line.push_str(&format_date(v[r])),
                    ColumnData::Boolean(v) => line.push_str(if v[r] { "true" } else { "false" }),
                    ColumnData::Utf8(s) => line.push_str(&csv_escape(s.get(r), b',')),
                }
            }
            line.push('\n');
            w.write_all(line.as_bytes())?;
        }
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ScalarValue;

    #[test]
    fn infers_types_and_nulls() {
        let csv = "id,price,name,day,flag\n1,2.5,alice,2020-01-01,true\n2,,\"bob, jr\",2020-02-03,false\n3,4,\"say \"\"hi\"\"\",,\n";
        let t = read_csv_str(csv, &CsvOptions::default()).unwrap();
        let types: Vec<DataType> = t.schema.fields.iter().map(|f| f.data_type).collect();
        assert_eq!(
            types,
            vec![
                DataType::Int64,
                DataType::Float64,
                DataType::Utf8,
                DataType::Date,
                DataType::Boolean
            ]
        );
        assert_eq!(t.num_rows, 3);
        let b = &t.batches[0];
        assert!(b.columns[1].value(1).is_null());
        assert_eq!(b.columns[2].value(1), ScalarValue::Utf8("bob, jr".into()));
        assert_eq!(b.columns[2].value(2), ScalarValue::Utf8("say \"hi\"".into()));
        assert_eq!(b.columns[1].value(2), ScalarValue::Float64(4.0));
    }

    #[test]
    fn widens_after_sample() {
        let mut csv = String::from("a|b\n");
        for i in 0..50 {
            csv.push_str(&format!("{i}|x\n"));
        }
        csv.push_str("oops|y\n");
        let t = read_csv_str(
            &csv,
            &CsvOptions {
                infer_rows: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(t.schema.fields[0].data_type, DataType::Utf8);
        assert_eq!(t.num_rows, 51);
    }
}
