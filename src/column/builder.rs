//! Row-at-a-time column construction (used by loaders, INSERT and tests).

use super::{Bitmap, Column, ColumnData, StringData, empty_data};
use crate::types::{DataType, ScalarValue};

#[derive(Debug)]
pub struct ColumnBuilder {
    data: ColumnData,
    validity: Vec<bool>,
    has_null: bool,
}

impl ColumnBuilder {
    pub fn new(dtype: DataType) -> Self {
        Self::with_capacity(dtype, 0)
    }

    pub fn with_capacity(dtype: DataType, cap: usize) -> Self {
        let mut data = empty_data(dtype, 0);
        match &mut data {
            ColumnData::Boolean(v) => v.reserve(cap),
            ColumnData::Int64(v) => v.reserve(cap),
            ColumnData::Float64(v) => v.reserve(cap),
            ColumnData::Date(v) => v.reserve(cap),
            ColumnData::Utf8(v) => *v = StringData::with_capacity(cap, cap * 8),
        }
        ColumnBuilder {
            data,
            validity: Vec::with_capacity(cap),
            has_null: false,
        }
    }

    pub fn len(&self) -> usize {
        self.validity.len()
    }

    pub fn is_empty(&self) -> bool {
        self.validity.is_empty()
    }

    pub fn data_type(&self) -> DataType {
        self.data.data_type()
    }

    pub fn push_null(&mut self) {
        match &mut self.data {
            ColumnData::Boolean(v) => v.push(false),
            ColumnData::Int64(v) => v.push(0),
            ColumnData::Float64(v) => v.push(0.0),
            ColumnData::Date(v) => v.push(0),
            ColumnData::Utf8(v) => v.push(""),
        }
        self.validity.push(false);
        self.has_null = true;
    }

    #[inline]
    pub fn push_i64(&mut self, x: i64) {
        if let ColumnData::Int64(v) = &mut self.data {
            v.push(x);
            self.validity.push(true);
        }
    }
    #[inline]
    pub fn push_f64(&mut self, x: f64) {
        if let ColumnData::Float64(v) = &mut self.data {
            v.push(x);
            self.validity.push(true);
        }
    }
    #[inline]
    pub fn push_str(&mut self, x: &str) {
        if let ColumnData::Utf8(v) = &mut self.data {
            v.push(x);
            self.validity.push(true);
        }
    }
    #[inline]
    pub fn push_date(&mut self, x: i32) {
        if let ColumnData::Date(v) = &mut self.data {
            v.push(x);
            self.validity.push(true);
        }
    }
    #[inline]
    pub fn push_bool(&mut self, x: bool) {
        if let ColumnData::Boolean(v) = &mut self.data {
            v.push(x);
            self.validity.push(true);
        }
    }

    /// Push a scalar, converting between compatible types. Values that cannot
    /// be represented are pushed as NULL.
    pub fn push(&mut self, value: &ScalarValue) {
        match (&mut self.data, value) {
            (_, ScalarValue::Null(_)) => return self.push_null(),
            (ColumnData::Boolean(v), ScalarValue::Boolean(b)) => v.push(*b),
            (ColumnData::Int64(v), ScalarValue::Int64(x)) => v.push(*x),
            (ColumnData::Float64(v), ScalarValue::Float64(x)) => v.push(*x),
            (ColumnData::Float64(v), ScalarValue::Int64(x)) => v.push(*x as f64),
            (ColumnData::Utf8(v), ScalarValue::Utf8(s)) => v.push(s),
            (ColumnData::Utf8(v), other) => v.push(&other.to_string()),
            (ColumnData::Date(v), ScalarValue::Date(d)) => v.push(*d),
            _ => return self.push_null(),
        }
        self.validity.push(true);
    }

    pub fn finish(self) -> Column {
        let validity = self.has_null.then(|| Bitmap::from_bools(&self.validity));
        Column::new(self.data, validity)
    }
}
