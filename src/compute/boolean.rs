//! Three-valued (Kleene) boolean logic kernels.

use super::ColumnarValue;
use crate::column::{Bitmap, Column, ColumnData};
use crate::types::{DataType, ScalarValue};
use std::sync::Arc;

/// Split a boolean operand into per-row (value, valid) accessors.
fn parts(v: &ColumnarValue, len: usize) -> (Vec<bool>, Vec<bool>) {
    match v {
        ColumnarValue::Scalar(ScalarValue::Boolean(b)) => (vec![*b; len], vec![true; len]),
        ColumnarValue::Scalar(_) => (vec![false; len], vec![false; len]),
        ColumnarValue::Column(c) => {
            let vals = match &c.data {
                ColumnData::Boolean(v) => v.clone(),
                _ => vec![false; len],
            };
            let valid = match &c.validity {
                None => vec![true; len],
                Some(b) => b.iter().collect(),
            };
            (vals, valid)
        }
    }
}

fn scalar_bool(v: &ColumnarValue) -> Option<Option<bool>> {
    match v {
        ColumnarValue::Scalar(ScalarValue::Boolean(b)) => Some(Some(*b)),
        ColumnarValue::Scalar(_) => Some(None),
        _ => None,
    }
}

fn finish(vals: Vec<bool>, valid: Vec<bool>) -> ColumnarValue {
    let validity = if valid.iter().all(|&v| v) {
        None
    } else {
        Some(Bitmap::from_bools(&valid))
    };
    ColumnarValue::Column(Arc::new(Column::new(ColumnData::Boolean(vals), validity)))
}

pub fn and(l: &ColumnarValue, r: &ColumnarValue, len: usize) -> ColumnarValue {
    if let (Some(a), Some(b)) = (scalar_bool(l), scalar_bool(r)) {
        let v = match (a, b) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        };
        return ColumnarValue::Scalar(v.map_or(ScalarValue::Null(DataType::Boolean), ScalarValue::Boolean));
    }
    // Fast path: neither side has NULLs.
    if let (ColumnarValue::Column(a), ColumnarValue::Column(b)) = (l, r)
        && a.validity.is_none()
        && b.validity.is_none()
        && let (ColumnData::Boolean(x), ColumnData::Boolean(y)) = (&a.data, &b.data)
    {
        let vals = x.iter().zip(y).map(|(p, q)| *p & *q).collect();
        return ColumnarValue::Column(Arc::new(Column::from_data(ColumnData::Boolean(vals))));
    }
    let (lv, lk) = parts(l, len);
    let (rv, rk) = parts(r, len);
    let mut vals = Vec::with_capacity(len);
    let mut valid = Vec::with_capacity(len);
    for i in 0..len {
        let l_false = lk[i] && !lv[i];
        let r_false = rk[i] && !rv[i];
        let is_valid = l_false || r_false || (lk[i] && rk[i]);
        vals.push(lv[i] && rv[i] && lk[i] && rk[i]);
        valid.push(is_valid);
    }
    finish(vals, valid)
}

pub fn or(l: &ColumnarValue, r: &ColumnarValue, len: usize) -> ColumnarValue {
    if let (Some(a), Some(b)) = (scalar_bool(l), scalar_bool(r)) {
        let v = match (a, b) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        };
        return ColumnarValue::Scalar(v.map_or(ScalarValue::Null(DataType::Boolean), ScalarValue::Boolean));
    }
    if let (ColumnarValue::Column(a), ColumnarValue::Column(b)) = (l, r)
        && a.validity.is_none()
        && b.validity.is_none()
        && let (ColumnData::Boolean(x), ColumnData::Boolean(y)) = (&a.data, &b.data)
    {
        let vals = x.iter().zip(y).map(|(p, q)| *p | *q).collect();
        return ColumnarValue::Column(Arc::new(Column::from_data(ColumnData::Boolean(vals))));
    }
    let (lv, lk) = parts(l, len);
    let (rv, rk) = parts(r, len);
    let mut vals = Vec::with_capacity(len);
    let mut valid = Vec::with_capacity(len);
    for i in 0..len {
        let l_true = lk[i] && lv[i];
        let r_true = rk[i] && rv[i];
        vals.push(l_true || r_true);
        valid.push(l_true || r_true || (lk[i] && rk[i]));
    }
    finish(vals, valid)
}

pub fn not(v: &ColumnarValue) -> ColumnarValue {
    match v {
        ColumnarValue::Scalar(ScalarValue::Boolean(b)) => ColumnarValue::Scalar(ScalarValue::Boolean(!b)),
        ColumnarValue::Scalar(s) => ColumnarValue::Scalar(s.clone()),
        ColumnarValue::Column(c) => {
            let vals = match &c.data {
                ColumnData::Boolean(v) => v.iter().map(|b| !b).collect(),
                _ => vec![false; c.len()],
            };
            ColumnarValue::Column(Arc::new(Column::new(ColumnData::Boolean(vals), c.validity.clone())))
        }
    }
}

pub fn is_null(v: &ColumnarValue, len: usize, negated: bool) -> ColumnarValue {
    match v {
        ColumnarValue::Scalar(s) => ColumnarValue::Scalar(ScalarValue::Boolean(s.is_null() != negated)),
        ColumnarValue::Column(c) => {
            let vals = match &c.validity {
                None => vec![negated; len],
                Some(b) => b.iter().map(|valid| valid == negated).collect(),
            };
            ColumnarValue::Column(Arc::new(Column::from_data(ColumnData::Boolean(vals))))
        }
    }
}
