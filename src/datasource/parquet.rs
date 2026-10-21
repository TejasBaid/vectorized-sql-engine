//! Parquet support built on the low-level `parquet` crate APIs.
//!
//! The crate is used only to decode pages (decompression, RLE/dictionary
//! decoding, definition levels). Values are copied straight from the typed
//! column readers into the engine's own column vectors; no Arrow is involved.
//! Row groups are decoded in parallel.
//!
//! [`write_parquet`] exists for the data generator and tests.

use crate::catalog::MemTable;
use crate::column::{BATCH_SIZE, Batch, Bitmap, Column, ColumnData, Field, Schema, StringData};
use crate::err;
use crate::error::Result;
use crate::types::DataType;
use parquet::basic::{LogicalType, Type as PhysicalType};
use parquet::column::reader::ColumnReader;
use parquet::file::reader::{FileReader, SerializedFileReader};
use parquet::schema::types::ColumnDescriptor;
use std::fs::File;
use std::sync::Arc;

/// How to convert a Parquet leaf column into an engine type.
#[derive(Debug, Clone, Copy)]
enum Conversion {
    Bool,
    Int32,
    Int64,
    Date,
    Float,
    Double,
    /// Decimal stored as integer with the given scale -> DOUBLE.
    DecimalInt(i32),
    /// Decimal stored as big-endian fixed bytes -> DOUBLE.
    DecimalBytes(i32),
    Utf8,
}

fn conversion(col: &ColumnDescriptor) -> Result<(Conversion, DataType)> {
    if col.max_rep_level() > 0 {
        return err!(
            Unsupported,
            "nested/repeated Parquet column \"{}\" is not supported",
            col.name()
        );
    }
    let logical = col.logical_type_ref();
    let decimal_scale = match logical {
        Some(LogicalType::Decimal(d)) => Some(d.scale),
        _ => match col.converted_type() {
            parquet::basic::ConvertedType::DECIMAL => Some(col.type_scale()),
            _ => None,
        },
    };
    Ok(match col.physical_type() {
        PhysicalType::BOOLEAN => (Conversion::Bool, DataType::Boolean),
        PhysicalType::INT32 => match (logical, decimal_scale) {
            (_, Some(s)) => (Conversion::DecimalInt(s), DataType::Float64),
            (Some(LogicalType::Date), _) => (Conversion::Date, DataType::Date),
            _ if col.converted_type() == parquet::basic::ConvertedType::DATE => (Conversion::Date, DataType::Date),
            _ => (Conversion::Int32, DataType::Int64),
        },
        PhysicalType::INT64 => match decimal_scale {
            Some(s) => (Conversion::DecimalInt(s), DataType::Float64),
            None => (Conversion::Int64, DataType::Int64),
        },
        PhysicalType::FLOAT => (Conversion::Float, DataType::Float64),
        PhysicalType::DOUBLE => (Conversion::Double, DataType::Float64),
        PhysicalType::BYTE_ARRAY => (Conversion::Utf8, DataType::Utf8),
        PhysicalType::FIXED_LEN_BYTE_ARRAY => match decimal_scale {
            Some(s) => (Conversion::DecimalBytes(s), DataType::Float64),
            None => {
                return err!(
                    Unsupported,
                    "FIXED_LEN_BYTE_ARRAY column \"{}\" is not supported",
                    col.name()
                );
            }
        },
        PhysicalType::INT96 => return err!(Unsupported, "INT96 column \"{}\" is not supported", col.name()),
    })
}

/// Expand `values` (non-null only) to `n` slots according to definition levels.
fn spread<T: Copy + Default>(values: &[T], def: &[i16], max_def: i16, n: usize) -> (Vec<T>, Option<Bitmap>) {
    if max_def == 0 {
        return (values[..n].to_vec(), None);
    }
    let mut out = Vec::with_capacity(n);
    let mut valid = Vec::with_capacity(n);
    let mut k = 0;
    for &d in &def[..n] {
        if d == max_def {
            out.push(values[k]);
            k += 1;
            valid.push(true);
        } else {
            out.push(T::default());
            valid.push(false);
        }
    }
    (out, Some(Bitmap::from_bools(&valid)))
}

fn decimal_from_be(bytes: &[u8]) -> i128 {
    let mut v: i128 = if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        -1
    } else {
        0
    };
    for b in bytes {
        v = (v << 8) | *b as i128;
    }
    v
}

/// Read up to `max` records from one column reader into a Column.
fn read_column_chunk(reader: &mut ColumnReader, conv: Conversion, max_def: i16, max: usize) -> Result<Option<Column>> {
    let mut def: Vec<i16> = Vec::with_capacity(max);
    macro_rules! read {
        ($r:expr, $vals:ident) => {{
            let (records, _, _) =
                $r.read_records(max, if max_def > 0 { Some(&mut def) } else { None }, None, &mut $vals)?;
            records
        }};
    }
    let column = match (reader, conv) {
        (ColumnReader::BoolColumnReader(r), _) => {
            let mut vals: Vec<bool> = Vec::with_capacity(max);
            let (n, _, _) = r.read_records(max, if max_def > 0 { Some(&mut def) } else { None }, None, &mut vals)?;
            if n == 0 {
                return Ok(None);
            }
            let (v, b) = spread(&vals, &def, max_def, n);
            Column::new(ColumnData::Boolean(v), b)
        }
        (ColumnReader::Int32ColumnReader(r), conv) => {
            let mut vals: Vec<i32> = Vec::with_capacity(max);
            let (n, _, _) = r.read_records(max, if max_def > 0 { Some(&mut def) } else { None }, None, &mut vals)?;
            if n == 0 {
                return Ok(None);
            }
            let (v, b) = spread(&vals, &def, max_def, n);
            match conv {
                Conversion::Date => Column::new(ColumnData::Date(v), b),
                Conversion::DecimalInt(s) => {
                    let p = 10f64.powi(s);
                    Column::new(ColumnData::Float64(v.iter().map(|&x| x as f64 / p).collect()), b)
                }
                _ => Column::new(ColumnData::Int64(v.iter().map(|&x| x as i64).collect()), b),
            }
        }
        (ColumnReader::Int64ColumnReader(r), conv) => {
            let mut vals = Vec::with_capacity(max);
            let n = read!(r, vals);
            if n == 0 {
                return Ok(None);
            }
            let (v, b) = spread(&vals, &def, max_def, n);
            match conv {
                Conversion::DecimalInt(s) => {
                    let p = 10f64.powi(s);
                    Column::new(ColumnData::Float64(v.iter().map(|&x| x as f64 / p).collect()), b)
                }
                _ => Column::new(ColumnData::Int64(v), b),
            }
        }
        (ColumnReader::FloatColumnReader(r), _) => {
            let mut vals = Vec::with_capacity(max);
            let n = read!(r, vals);
            if n == 0 {
                return Ok(None);
            }
            let (v, b) = spread(&vals, &def, max_def, n);
            Column::new(ColumnData::Float64(v.iter().map(|&x| x as f64).collect()), b)
        }
        (ColumnReader::DoubleColumnReader(r), _) => {
            let mut vals = Vec::with_capacity(max);
            let n = read!(r, vals);
            if n == 0 {
                return Ok(None);
            }
            let (v, b) = spread(&vals, &def, max_def, n);
            Column::new(ColumnData::Float64(v), b)
        }
        (ColumnReader::ByteArrayColumnReader(r), _) => {
            let mut vals: Vec<parquet::data_type::ByteArray> = Vec::with_capacity(max);
            let (n, _, _) = r.read_records(max, if max_def > 0 { Some(&mut def) } else { None }, None, &mut vals)?;
            if n == 0 {
                return Ok(None);
            }
            let mut s = StringData::with_capacity(n, n * 16);
            let mut valid = Vec::with_capacity(n);
            let mut k = 0;
            for i in 0..n {
                if max_def == 0 || def.get(i) == Some(&max_def) {
                    s.push(&String::from_utf8_lossy(vals[k].data()));
                    k += 1;
                    valid.push(true);
                } else {
                    s.push("");
                    valid.push(false);
                }
            }
            Column::new(ColumnData::Utf8(s), (max_def > 0).then(|| Bitmap::from_bools(&valid)))
        }
        (ColumnReader::FixedLenByteArrayColumnReader(r), Conversion::DecimalBytes(scale)) => {
            let mut vals: Vec<parquet::data_type::FixedLenByteArray> = Vec::with_capacity(max);
            let (n, _, _) = r.read_records(max, if max_def > 0 { Some(&mut def) } else { None }, None, &mut vals)?;
            if n == 0 {
                return Ok(None);
            }
            let p = 10f64.powi(scale);
            let nums: Vec<f64> = vals.iter().map(|v| decimal_from_be(v.data()) as f64 / p).collect();
            let (v, b) = spread(&nums, &def, max_def, n);
            Column::new(ColumnData::Float64(v), b)
        }
        _ => return err!(Unsupported, "unsupported Parquet column type"),
    };
    Ok(Some(column))
}

pub fn read_parquet(path: &str) -> Result<MemTable> {
    // Read the whole file into memory: column chunks are then decoded from
    // independent slices, which is what makes parallel row-group decoding safe
    // (cloned `File` handles would share one seek offset).
    let data = std::fs::read(path).map_err(|e| crate::Error::new(crate::ErrorKind::Io, format!("{path}: {e}")))?;
    let reader = SerializedFileReader::new(bytes::Bytes::from(data))?;
    let meta = reader.metadata();
    let schema_descr = meta.file_metadata().schema_descr();
    let mut convs = vec![];
    let mut fields = vec![];
    for col in schema_descr.columns() {
        let (conv, dt) = conversion(col)?;
        convs.push((conv, col.max_def_level()));
        fields.push(Field::new(col.name().to_lowercase(), dt, col.max_def_level() > 0));
    }
    let num_groups = reader.num_row_groups();
    let reader = Arc::new(reader);
    let read_group = |g: usize| -> Result<Vec<Batch>> {
        let rg = reader.get_row_group(g)?;
        let mut readers = (0..convs.len())
            .map(|c| rg.get_column_reader(c))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut batches = vec![];
        loop {
            let mut cols = Vec::with_capacity(convs.len());
            for (r, &(conv, max_def)) in readers.iter_mut().zip(&convs) {
                match read_column_chunk(r, conv, max_def, BATCH_SIZE)? {
                    Some(c) => cols.push(c),
                    None => break,
                }
            }
            if cols.len() < convs.len() || cols.is_empty() {
                break;
            }
            let n = cols[0].len();
            if cols.iter().any(|c| c.len() != n) {
                return err!(Io, "inconsistent column lengths in Parquet row group {g}");
            }
            batches.push(Batch::from_columns(cols));
        }
        Ok(batches)
    };
    let results: Vec<Result<Vec<Batch>>> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..num_groups).map(|g| s.spawn(move || read_group(g))).collect();
        hs.into_iter()
            .map(|h| h.join().expect("parquet worker panicked"))
            .collect()
    });
    let mut batches = vec![];
    for r in results {
        batches.extend(r?);
    }
    Ok(MemTable::new(Schema::new(fields), batches))
}

/// Write batches to a Parquet file (flat schema, all columns optional,
/// snappy-compressed, one row group per `rows_per_group` rows).
pub fn write_parquet(schema: &Schema, batches: &[Batch], path: &str, rows_per_group: usize) -> Result<()> {
    use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int32Type, Int64Type};
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use parquet::schema::parser::parse_message_type;

    let cols: Vec<String> = schema
        .fields
        .iter()
        .map(|f| {
            let (t, annotation) = match f.data_type {
                DataType::Boolean => ("BOOLEAN", ""),
                DataType::Int64 => ("INT64", ""),
                DataType::Float64 => ("DOUBLE", ""),
                DataType::Date => ("INT32", " (DATE)"),
                _ => ("BYTE_ARRAY", " (UTF8)"),
            };
            format!("OPTIONAL {t} {}{annotation};", f.name)
        })
        .collect();
    let message = format!("message schema {{ {} }}", cols.join(" "));
    let pschema = Arc::new(parse_message_type(&message)?);
    let props = Arc::new(
        WriterProperties::builder()
            .set_compression(parquet::basic::Compression::SNAPPY)
            .build(),
    );
    let file = File::create(path)?;
    let mut writer = SerializedFileWriter::new(file, pschema, props)?;
    let mut groups: Vec<Vec<&Batch>> = vec![vec![]];
    let mut rows = 0;
    for b in batches {
        if rows >= rows_per_group {
            groups.push(vec![]);
            rows = 0;
        }
        groups.last_mut().unwrap().push(b);
        rows += b.num_rows;
    }
    for group in groups.into_iter().filter(|g| !g.is_empty()) {
        let mut rg = writer.next_row_group()?;
        let mut ci = 0;
        while let Some(mut cw) = rg.next_column()? {
            let mut def: Vec<i16> = vec![];
            macro_rules! write_typed {
                ($ty:ty, $variant:ident, $conv:expr) => {{
                    let mut vals = vec![];
                    for b in &group {
                        let c = &b.columns[ci];
                        if let ColumnData::$variant(v) = &c.data {
                            for i in 0..c.len() {
                                if c.is_valid(i) {
                                    vals.push($conv(&v[i]));
                                    def.push(1);
                                } else {
                                    def.push(0);
                                }
                            }
                        }
                    }
                    cw.typed::<$ty>().write_batch(&vals, Some(&def), None)?;
                }};
            }
            match schema.fields[ci].data_type {
                DataType::Boolean => write_typed!(BoolType, Boolean, |x: &bool| *x),
                DataType::Int64 => write_typed!(Int64Type, Int64, |x: &i64| *x),
                DataType::Float64 => write_typed!(DoubleType, Float64, |x: &f64| *x),
                DataType::Date => write_typed!(Int32Type, Date, |x: &i32| *x),
                _ => {
                    let mut vals = vec![];
                    for b in &group {
                        let c = &b.columns[ci];
                        if let ColumnData::Utf8(s) = &c.data {
                            for i in 0..c.len() {
                                if c.is_valid(i) {
                                    vals.push(ByteArray::from(s.get(i)));
                                    def.push(1);
                                } else {
                                    def.push(0);
                                }
                            }
                        }
                    }
                    cw.typed::<ByteArrayType>().write_batch(&vals, Some(&def), None)?;
                }
            }
            cw.close()?;
            ci += 1;
        }
        rg.close()?;
    }
    writer.close()?;
    Ok(())
}
