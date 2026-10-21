//! Arithmetic kernels: `+ - * / %` on integers, floats and dates, plus `||`.

use super::{ColumnarValue, Operand, binary_map, date_operand, f64_operand, i64_operand};
use crate::ast::BinaryOperator;
use crate::column::{Bitmap, Column, ColumnData, StringData, and_validity};
use crate::err;
use crate::error::Result;
use crate::types::{DataType, ScalarValue, add_interval};
use std::sync::Arc;

/// Evaluate `l op r` producing a value of type `out`.
pub fn arith(
    op: BinaryOperator,
    l: &ColumnarValue,
    r: &ColumnarValue,
    len: usize,
    out: DataType,
) -> Result<ColumnarValue> {
    if l.is_null_scalar() || r.is_null_scalar() {
        return Ok(ColumnarValue::Scalar(ScalarValue::Null(out)));
    }
    let scalar_result = matches!((l, r), (ColumnarValue::Scalar(_), ColumnarValue::Scalar(_)));
    let len = if scalar_result { 1 } else { len };
    let validity = and_validity(l.validity(), r.validity());
    let col = match op {
        BinaryOperator::Concat => concat(l, r, len, validity),
        _ => match out {
            DataType::Int64
                if matches!(l, ColumnarValue::Column(c) if c.data_type() == DataType::Date)
                    || matches!(l, ColumnarValue::Scalar(ScalarValue::Date(_))) =>
            {
                // date - date = days
                let (a, b) = (date_operand(l).unwrap(), date_operand(r).unwrap());
                Column::new(
                    ColumnData::Int64(binary_map(a, b, len, |x, y| x as i64 - y as i64)),
                    validity,
                )
            }
            DataType::Int64 => int_arith(op, i64_operand(l).unwrap(), i64_operand(r).unwrap(), len, validity)?,
            DataType::Float64 => float_arith(op, f64_operand(l).unwrap(), f64_operand(r).unwrap(), len, validity),
            DataType::Date => date_arith(op, l, r, len, validity)?,
            other => return err!(Execution, "arithmetic not supported for type {other}"),
        },
    };
    Ok(if scalar_result {
        ColumnarValue::Scalar(col.value(0))
    } else {
        ColumnarValue::Column(Arc::new(col))
    })
}

fn int_arith(
    op: BinaryOperator,
    a: Operand<i64>,
    b: Operand<i64>,
    len: usize,
    validity: Option<Bitmap>,
) -> Result<Column> {
    use BinaryOperator::*;
    match op {
        Plus | Minus | Multiply => {
            let f = match op {
                Plus => i64::overflowing_add,
                Minus => i64::overflowing_sub,
                _ => i64::overflowing_mul,
            };
            let pairs = binary_map(a, b, len, f);
            let overflow = pairs
                .iter()
                .enumerate()
                .any(|(i, (_, o))| *o && validity.as_ref().is_none_or(|v| v.get(i)));
            if overflow {
                return err!(Execution, "integer overflow in '{op}'");
            }
            Ok(Column::new(
                ColumnData::Int64(pairs.into_iter().map(|(v, _)| v).collect()),
                validity,
            ))
        }
        Divide | Modulo => {
            // Division by zero yields NULL (SQLite/DuckDB semantics).
            let vals = binary_map(a, b, len, |x, y| {
                if y == 0 {
                    0
                } else if op == Divide {
                    x.wrapping_div(y)
                } else {
                    x.wrapping_rem(y)
                }
            });
            let zero = binary_map(a, b, len, |_, y| y != 0);
            let validity = combine(validity, &zero);
            Ok(Column::new(ColumnData::Int64(vals), validity))
        }
        _ => err!(Execution, "invalid integer operator {op}"),
    }
}

fn float_arith(op: BinaryOperator, a: Operand<f64>, b: Operand<f64>, len: usize, validity: Option<Bitmap>) -> Column {
    use BinaryOperator::*;
    let vals = match op {
        Plus => binary_map(a, b, len, |x, y| x + y),
        Minus => binary_map(a, b, len, |x, y| x - y),
        Multiply => binary_map(a, b, len, |x, y| x * y),
        Divide => binary_map(a, b, len, |x, y| x / y),
        _ => binary_map(a, b, len, |x, y| x % y),
    };
    let validity = if matches!(op, Divide | Modulo) {
        let nonzero = binary_map(a, b, len, |_, y| y != 0.0);
        combine(validity, &nonzero)
    } else {
        validity
    };
    Column::new(ColumnData::Float64(vals), validity)
}

fn date_arith(
    op: BinaryOperator,
    l: &ColumnarValue,
    r: &ColumnarValue,
    len: usize,
    validity: Option<Bitmap>,
) -> Result<Column> {
    let sign = if op == BinaryOperator::Minus { -1 } else { 1 };
    // Normalise so the date is on the left (`n + date` == `date + n`).
    let (d, other) = if date_operand(l).is_some() { (l, r) } else { (r, l) };
    let d = date_operand(d).unwrap();
    let vals = match other {
        ColumnarValue::Scalar(ScalarValue::Interval { months, days }) => {
            let (m, dd) = (months * sign, days * sign);
            binary_map(d, Operand::Scalar(0), len, |x, _| add_interval(x, m, dd))
        }
        _ => {
            let Some(n) = i64_operand(other) else {
                return err!(Execution, "invalid date arithmetic");
            };
            match (d, n) {
                (Operand::Slice(a), Operand::Slice(b)) => {
                    a.iter().zip(b).map(|(x, y)| x + (*y as i32) * sign).collect()
                }
                (Operand::Slice(a), Operand::Scalar(y)) => a.iter().map(|x| x + (y as i32) * sign).collect(),
                (Operand::Scalar(x), Operand::Slice(b)) => b.iter().map(|y| x + (*y as i32) * sign).collect(),
                (Operand::Scalar(x), Operand::Scalar(y)) => vec![x + (y as i32) * sign; len],
            }
        }
    };
    Ok(Column::new(ColumnData::Date(vals), validity))
}

fn concat(l: &ColumnarValue, r: &ColumnarValue, len: usize, validity: Option<Bitmap>) -> Column {
    let get = |v: &ColumnarValue, i: usize| -> String {
        match v {
            ColumnarValue::Column(c) => match &c.data {
                ColumnData::Utf8(s) => s.get(i).to_string(),
                _ => c.value(i).to_string(),
            },
            ColumnarValue::Scalar(s) => s.to_string(),
        }
    };
    let mut out = StringData::with_capacity(len, len * 16);
    let mut buf = String::new();
    for i in 0..len {
        buf.clear();
        buf.push_str(&get(l, i));
        buf.push_str(&get(r, i));
        out.push(&buf);
    }
    Column::new(ColumnData::Utf8(out), validity)
}

fn combine(validity: Option<Bitmap>, extra: &[bool]) -> Option<Bitmap> {
    if extra.iter().all(|&b| b) {
        return validity;
    }
    let extra = Bitmap::from_bools(extra);
    Some(match validity {
        Some(v) => v.and(&extra),
        None => extra,
    })
}
