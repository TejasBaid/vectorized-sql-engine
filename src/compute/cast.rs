//! CAST kernels.

use crate::column::{Bitmap, Column, ColumnBuilder, ColumnData, StringData};
use crate::error::{Error, ErrorKind, Result};
use crate::types::{DataType, ScalarValue, format_date, parse_date};

pub fn cast_column(col: &Column, to: DataType) -> Result<Column> {
    use ColumnData::*;
    let from = col.data_type();
    if from == to {
        return Ok(col.clone());
    }
    let data = match (&col.data, to) {
        (Int64(v), DataType::Float64) => Float64(v.iter().map(|&x| x as f64).collect()),
        (Float64(v), DataType::Int64) => Int64(v.iter().map(|&x| x.round() as i64).collect()),
        (Boolean(v), DataType::Int64) => Int64(v.iter().map(|&x| x as i64).collect()),
        (Int64(v), DataType::Boolean) => Boolean(v.iter().map(|&x| x != 0).collect()),
        (Boolean(v), DataType::Float64) => Float64(v.iter().map(|&x| x as i64 as f64).collect()),
        (_, DataType::Utf8) => {
            let mut out = StringData::with_capacity(col.len(), col.len() * 8);
            for i in 0..col.len() {
                if col.is_valid(i) {
                    match &col.data {
                        Date(d) => out.push(&format_date(d[i])),
                        _ => out.push(&col.value(i).to_string()),
                    }
                } else {
                    out.push("");
                }
            }
            Utf8(out)
        }
        (Utf8(s), _) => return cast_strings(s, col.validity.as_ref(), to),
        _ => {
            return Err(Error::new(ErrorKind::Execution, format!("cannot cast {from} to {to}")));
        }
    };
    Ok(Column::new(data, col.validity.clone()))
}

fn cast_strings(s: &StringData, validity: Option<&Bitmap>, to: DataType) -> Result<Column> {
    let mut b = ColumnBuilder::with_capacity(to, s.len());
    for i in 0..s.len() {
        if validity.is_some_and(|v| !v.get(i)) {
            b.push_null();
            continue;
        }
        let v = cast_str(s.get(i), to)?;
        b.push(&v);
    }
    Ok(b.finish())
}

/// Parse a string into a scalar of the given type (errors on invalid input).
pub fn cast_str(s: &str, to: DataType) -> Result<ScalarValue> {
    let t = s.trim();
    let bad = || Error::new(ErrorKind::Execution, format!("could not convert string '{s}' to {to}"));
    Ok(match to {
        DataType::Int64 => ScalarValue::Int64(t.parse().map_err(|_| bad())?),
        DataType::Float64 => ScalarValue::Float64(t.parse().map_err(|_| bad())?),
        DataType::Date => ScalarValue::Date(parse_date(t).ok_or_else(bad)?),
        DataType::Boolean => match t.to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" => ScalarValue::Boolean(true),
            "false" | "f" | "0" | "no" => ScalarValue::Boolean(false),
            _ => return Err(bad()),
        },
        DataType::Utf8 => ScalarValue::Utf8(s.to_string()),
        _ => return Err(bad()),
    })
}

/// Cast a single scalar (used by constant folding and the binder).
pub fn cast_scalar(v: &ScalarValue, to: DataType) -> Result<ScalarValue> {
    if v.is_null() {
        return Ok(ScalarValue::Null(to));
    }
    if v.data_type() == to {
        return Ok(v.clone());
    }
    let col = Column::from_scalar(v, 1, v.data_type());
    Ok(cast_column(&col, to)?.value(0))
}
