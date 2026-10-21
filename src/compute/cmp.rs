//! Comparison kernels producing BOOLEAN columns.

use super::{ColumnarValue, binary_map, bool_operand, date_operand, f64_operand, i64_operand};
use crate::ast::BinaryOperator;
use crate::column::{Column, ColumnData, and_validity};
use crate::err;
use crate::error::Result;
use crate::types::{DataType, ScalarValue};
use std::cmp::Ordering;
use std::sync::Arc;

#[inline]
fn apply<T: PartialOrd>(op: BinaryOperator, a: T, b: T) -> bool {
    match op {
        BinaryOperator::Eq => a == b,
        BinaryOperator::NotEq => a != b,
        BinaryOperator::Lt => a < b,
        BinaryOperator::LtEq => a <= b,
        BinaryOperator::Gt => a > b,
        _ => a >= b,
    }
}

/// Macro so each (type, operator) pair gets its own monomorphized loop: the
/// operator match is hoisted out of the per-row loop.
macro_rules! cmp_loop {
    ($op:expr, $a:expr, $b:expr, $len:expr) => {{
        use BinaryOperator::*;
        match $op {
            Eq => binary_map($a, $b, $len, |x, y| x == y),
            NotEq => binary_map($a, $b, $len, |x, y| x != y),
            Lt => binary_map($a, $b, $len, |x, y| x < y),
            LtEq => binary_map($a, $b, $len, |x, y| x <= y),
            Gt => binary_map($a, $b, $len, |x, y| x > y),
            _ => binary_map($a, $b, $len, |x, y| x >= y),
        }
    }};
}

pub fn compare(op: BinaryOperator, l: &ColumnarValue, r: &ColumnarValue, len: usize) -> Result<ColumnarValue> {
    if l.is_null_scalar() || r.is_null_scalar() {
        return Ok(ColumnarValue::Scalar(ScalarValue::Null(DataType::Boolean)));
    }
    if let (ColumnarValue::Scalar(a), ColumnarValue::Scalar(b)) = (l, r) {
        let ord = a.total_cmp(b);
        let v = match op {
            BinaryOperator::Eq => ord == Ordering::Equal,
            BinaryOperator::NotEq => ord != Ordering::Equal,
            BinaryOperator::Lt => ord == Ordering::Less,
            BinaryOperator::LtEq => ord != Ordering::Greater,
            BinaryOperator::Gt => ord == Ordering::Greater,
            _ => ord != Ordering::Less,
        };
        return Ok(ColumnarValue::Scalar(ScalarValue::Boolean(v)));
    }
    let validity = and_validity(l.validity(), r.validity());
    let vals: Vec<bool> = if let (Some(a), Some(b)) = (i64_operand(l), i64_operand(r)) {
        cmp_loop!(op, a, b, len)
    } else if let (Some(a), Some(b)) = (date_operand(l), date_operand(r)) {
        cmp_loop!(op, a, b, len)
    } else if let (Some(a), Some(b)) = (f64_operand(l), f64_operand(r)) {
        cmp_loop!(op, a, b, len)
    } else if let (Some(a), Some(b)) = (bool_operand(l), bool_operand(r)) {
        cmp_loop!(op, a, b, len)
    } else {
        compare_strings(op, l, r, len)?
    };
    Ok(ColumnarValue::Column(Arc::new(Column::new(
        ColumnData::Boolean(vals),
        validity,
    ))))
}

fn as_strs(v: &ColumnarValue) -> Option<std::result::Result<&crate::column::StringData, &str>> {
    match v {
        ColumnarValue::Column(c) => match &c.data {
            ColumnData::Utf8(s) => Some(Ok(s)),
            _ => None,
        },
        ColumnarValue::Scalar(ScalarValue::Utf8(s)) => Some(Err(s.as_str())),
        _ => None,
    }
}

fn compare_strings(op: BinaryOperator, l: &ColumnarValue, r: &ColumnarValue, len: usize) -> Result<Vec<bool>> {
    match (as_strs(l), as_strs(r)) {
        (Some(Ok(a)), Some(Ok(b))) => Ok((0..len).map(|i| apply(op, a.get(i), b.get(i))).collect()),
        (Some(Ok(a)), Some(Err(s))) => Ok(match op {
            BinaryOperator::Eq => (0..len).map(|i| a.get_bytes(i) == s.as_bytes()).collect(),
            BinaryOperator::NotEq => (0..len).map(|i| a.get_bytes(i) != s.as_bytes()).collect(),
            _ => (0..len).map(|i| apply(op, a.get(i), s)).collect(),
        }),
        (Some(Err(s)), Some(Ok(b))) => Ok((0..len).map(|i| apply(op, s, b.get(i))).collect()),
        _ => err!(Execution, "cannot compare {l:?} with {r:?}"),
    }
}
