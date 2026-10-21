//! The engine's own columnar in-memory format.
//!
//! A [`Column`] is a typed vector of values plus an optional validity
//! [`Bitmap`] (absent = no NULLs). Slots that are NULL hold an arbitrary
//! (usually default) value, so kernels can run branch-free over the whole
//! vector and combine validity bitmaps separately. A [`Batch`] is a set of
//! equal-length columns, the unit of data flowing between operators.

mod batch;
mod bitmap;
mod builder;
mod string;

pub use batch::{Batch, Field, Schema, SchemaRef};
pub use bitmap::{Bitmap, and_validity};
pub use builder::ColumnBuilder;
pub use string::StringData;

use crate::types::{DataType, ScalarValue};
use std::sync::Arc;

/// Number of rows per batch produced by scans.
pub const BATCH_SIZE: usize = 8192;

/// Sentinel index for `take_nullable`: produce a NULL in this slot.
pub const NULL_INDEX: u32 = u32::MAX;

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    Boolean(Vec<bool>),
    Int64(Vec<i64>),
    Float64(Vec<f64>),
    Utf8(StringData),
    /// Days since epoch.
    Date(Vec<i32>),
}

impl ColumnData {
    pub fn len(&self) -> usize {
        match self {
            ColumnData::Boolean(v) => v.len(),
            ColumnData::Int64(v) => v.len(),
            ColumnData::Float64(v) => v.len(),
            ColumnData::Utf8(v) => v.len(),
            ColumnData::Date(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn data_type(&self) -> DataType {
        match self {
            ColumnData::Boolean(_) => DataType::Boolean,
            ColumnData::Int64(_) => DataType::Int64,
            ColumnData::Float64(_) => DataType::Float64,
            ColumnData::Utf8(_) => DataType::Utf8,
            ColumnData::Date(_) => DataType::Date,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub data: ColumnData,
    pub validity: Option<Bitmap>,
}

pub type ColumnRef = Arc<Column>;

impl Column {
    pub fn new(data: ColumnData, validity: Option<Bitmap>) -> Self {
        if let Some(v) = &validity {
            debug_assert_eq!(v.len(), data.len());
        }
        // Normalise: an all-valid bitmap is dropped so fast paths can trigger.
        let validity = validity.filter(|v| v.count_set() != v.len());
        Column { data, validity }
    }

    pub fn from_data(data: ColumnData) -> Self {
        Column { data, validity: None }
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn data_type(&self) -> DataType {
        self.data.data_type()
    }

    #[inline]
    pub fn is_valid(&self, i: usize) -> bool {
        self.validity.as_ref().is_none_or(|v| v.get(i))
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |v| v.len() - v.count_set())
    }

    /// An all-NULL column of the given type.
    pub fn new_null(dtype: DataType, len: usize) -> Self {
        let data = empty_data(dtype, len);
        Column {
            data,
            validity: Some(Bitmap::new_unset(len)),
        }
    }

    /// Broadcast a scalar into a column of `len` rows.
    pub fn from_scalar(value: &ScalarValue, len: usize, dtype: DataType) -> Self {
        match value {
            ScalarValue::Null(_) => Column::new_null(dtype, len),
            ScalarValue::Boolean(b) => Column::from_data(ColumnData::Boolean(vec![*b; len])),
            ScalarValue::Int64(v) => match dtype {
                DataType::Float64 => Column::from_data(ColumnData::Float64(vec![*v as f64; len])),
                _ => Column::from_data(ColumnData::Int64(vec![*v; len])),
            },
            ScalarValue::Float64(v) => Column::from_data(ColumnData::Float64(vec![*v; len])),
            ScalarValue::Utf8(s) => {
                let mut d = StringData::with_capacity(len, s.len() * len);
                for _ in 0..len {
                    d.push(s);
                }
                Column::from_data(ColumnData::Utf8(d))
            }
            ScalarValue::Date(v) => Column::from_data(ColumnData::Date(vec![*v; len])),
            ScalarValue::Interval { .. } => Column::new_null(DataType::Int64, len),
        }
    }

    /// Read a single value (slow path: used for display, tests and constants).
    pub fn value(&self, i: usize) -> ScalarValue {
        if !self.is_valid(i) {
            return ScalarValue::Null(self.data_type());
        }
        match &self.data {
            ColumnData::Boolean(v) => ScalarValue::Boolean(v[i]),
            ColumnData::Int64(v) => ScalarValue::Int64(v[i]),
            ColumnData::Float64(v) => ScalarValue::Float64(v[i]),
            ColumnData::Utf8(v) => ScalarValue::Utf8(v.get(i).to_string()),
            ColumnData::Date(v) => ScalarValue::Date(v[i]),
        }
    }

    /// Gather rows by index.
    pub fn take(&self, idx: &[u32]) -> Column {
        let data = match &self.data {
            ColumnData::Boolean(v) => ColumnData::Boolean(idx.iter().map(|&i| v[i as usize]).collect()),
            ColumnData::Int64(v) => ColumnData::Int64(idx.iter().map(|&i| v[i as usize]).collect()),
            ColumnData::Float64(v) => ColumnData::Float64(idx.iter().map(|&i| v[i as usize]).collect()),
            ColumnData::Date(v) => ColumnData::Date(idx.iter().map(|&i| v[i as usize]).collect()),
            ColumnData::Utf8(v) => {
                let bytes = idx.iter().map(|&i| v.get_bytes(i as usize).len()).sum();
                let mut out = StringData::with_capacity(idx.len(), bytes);
                for &i in idx {
                    out.push(v.get(i as usize));
                }
                ColumnData::Utf8(out)
            }
        };
        let validity = self
            .validity
            .as_ref()
            .map(|b| Bitmap::from_bools(&idx.iter().map(|&i| b.get(i as usize)).collect::<Vec<_>>()));
        Column::new(data, validity)
    }

    /// Gather rows by index where [`NULL_INDEX`] produces a NULL.
    pub fn take_nullable(&self, idx: &[u32]) -> Column {
        if !idx.contains(&NULL_INDEX) {
            return self.take(idx);
        }
        let safe: Vec<u32> = idx.iter().map(|&i| if i == NULL_INDEX { 0 } else { i }).collect();
        let mut col = if self.is_empty() {
            Column::new_null(self.data_type(), idx.len())
        } else {
            self.take(&safe)
        };
        let valid: Vec<bool> = idx
            .iter()
            .enumerate()
            .map(|(k, &i)| i != NULL_INDEX && col.is_valid(k))
            .collect();
        col.validity = Some(Bitmap::from_bools(&valid));
        Column::new(col.data, col.validity)
    }

    /// Keep rows where `mask` is true.
    pub fn filter(&self, mask: &[bool]) -> Column {
        let idx: Vec<u32> = mask
            .iter()
            .enumerate()
            .filter_map(|(i, &m)| m.then_some(i as u32))
            .collect();
        self.take(&idx)
    }

    pub fn slice(&self, offset: usize, len: usize) -> Column {
        let idx: Vec<u32> = (offset as u32..(offset + len) as u32).collect();
        self.take(&idx)
    }

    /// Concatenate columns of the same type.
    pub fn concat(cols: &[&Column]) -> Column {
        let total: usize = cols.iter().map(|c| c.len()).sum();
        let dtype = cols.first().map_or(DataType::Int64, |c| c.data_type());
        let mut data = empty_data(dtype, 0);
        match &mut data {
            ColumnData::Boolean(out) => {
                out.reserve(total);
                for c in cols {
                    if let ColumnData::Boolean(v) = &c.data {
                        out.extend_from_slice(v);
                    }
                }
            }
            ColumnData::Int64(out) => {
                out.reserve(total);
                for c in cols {
                    if let ColumnData::Int64(v) = &c.data {
                        out.extend_from_slice(v);
                    }
                }
            }
            ColumnData::Float64(out) => {
                out.reserve(total);
                for c in cols {
                    if let ColumnData::Float64(v) = &c.data {
                        out.extend_from_slice(v);
                    }
                }
            }
            ColumnData::Date(out) => {
                out.reserve(total);
                for c in cols {
                    if let ColumnData::Date(v) = &c.data {
                        out.extend_from_slice(v);
                    }
                }
            }
            ColumnData::Utf8(out) => {
                let bytes: usize = cols
                    .iter()
                    .map(|c| match &c.data {
                        ColumnData::Utf8(s) => s.total_bytes(),
                        _ => 0,
                    })
                    .sum();
                *out = StringData::with_capacity(total, bytes);
                for c in cols {
                    if let ColumnData::Utf8(v) = &c.data {
                        for s in v.iter() {
                            out.push(s);
                        }
                    }
                }
            }
        }
        let validity = if cols.iter().any(|c| c.validity.is_some()) {
            let mut b = Bitmap::new_unset(0);
            for c in cols {
                for i in 0..c.len() {
                    b.push(c.is_valid(i));
                }
            }
            Some(b)
        } else {
            None
        };
        Column::new(data, validity)
    }

    // Typed accessors used by kernels.
    pub fn as_i64(&self) -> Option<&[i64]> {
        match &self.data {
            ColumnData::Int64(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<&[f64]> {
        match &self.data {
            ColumnData::Float64(v) => Some(v),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<&[bool]> {
        match &self.data {
            ColumnData::Boolean(v) => Some(v),
            _ => None,
        }
    }
}

pub(crate) fn empty_data(dtype: DataType, len: usize) -> ColumnData {
    match dtype {
        DataType::Boolean => ColumnData::Boolean(vec![false; len]),
        DataType::Float64 => ColumnData::Float64(vec![0.0; len]),
        DataType::Utf8 => {
            let mut s = StringData::with_capacity(len, 0);
            for _ in 0..len {
                s.push("");
            }
            ColumnData::Utf8(s)
        }
        DataType::Date => ColumnData::Date(vec![0; len]),
        DataType::Int64 | DataType::Null | DataType::Interval => ColumnData::Int64(vec![0; len]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ints(v: &[Option<i64>]) -> Column {
        let mut b = ColumnBuilder::new(DataType::Int64);
        for x in v {
            b.push(&x.map_or(ScalarValue::Null(DataType::Int64), ScalarValue::Int64));
        }
        b.finish()
    }

    #[test]
    fn take_filter_concat() {
        let c = ints(&[Some(1), None, Some(3), Some(4)]);
        assert_eq!(c.null_count(), 1);
        let t = c.take(&[3, 1, 0]);
        assert_eq!(t.value(0), ScalarValue::Int64(4));
        assert!(t.value(1).is_null());
        let f = c.filter(&[true, false, false, true]);
        assert_eq!(f.len(), 2);
        assert!(f.validity.is_none());
        let cc = Column::concat(&[&c, &f]);
        assert_eq!(cc.len(), 6);
        assert_eq!(cc.null_count(), 1);
        let n = c.take_nullable(&[0, NULL_INDEX]);
        assert!(n.value(1).is_null());
    }

    #[test]
    fn strings() {
        let s: StringData = ["a", "bc", ""].into_iter().collect();
        let c = Column::from_data(ColumnData::Utf8(s));
        assert_eq!(c.take(&[1, 1]).value(1), ScalarValue::Utf8("bc".into()));
    }
}
