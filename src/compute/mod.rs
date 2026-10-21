//! Vectorized compute kernels over [`Column`]s.
//!
//! Kernels operate on whole columns at a time with tight, branch-light loops
//! over the typed value vectors; NULL handling is done separately by combining
//! validity bitmaps. Operands may be a column or a scalar (`ColumnarValue`) so
//! that the very common `column <op> constant` case never materializes a
//! broadcast vector.

pub mod arith;
pub mod boolean;
pub mod cast;
pub mod cmp;
pub mod functions;
pub mod hash;
pub mod like;

use crate::column::{Column, ColumnData, ColumnRef};
use crate::types::{DataType, ScalarValue};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum ColumnarValue {
    Column(ColumnRef),
    Scalar(ScalarValue),
}

impl ColumnarValue {
    pub fn into_column(self, len: usize, dtype: DataType) -> ColumnRef {
        match self {
            ColumnarValue::Column(c) => c,
            ColumnarValue::Scalar(s) => Arc::new(Column::from_scalar(&s, len, dtype)),
        }
    }

    pub fn is_null_scalar(&self) -> bool {
        matches!(self, ColumnarValue::Scalar(s) if s.is_null())
    }

    pub fn validity(&self) -> Option<&crate::column::Bitmap> {
        match self {
            ColumnarValue::Column(c) => c.validity.as_ref(),
            ColumnarValue::Scalar(_) => None,
        }
    }
}

/// A borrowed operand of a primitive kernel: either a whole vector or a constant.
#[derive(Clone, Copy)]
pub(crate) enum Operand<'a, T> {
    Slice(&'a [T]),
    Scalar(T),
}

/// Apply `f` element-wise, specialised for every slice/scalar combination.
#[inline]
pub(crate) fn binary_map<T: Copy, O>(
    l: Operand<'_, T>,
    r: Operand<'_, T>,
    len: usize,
    f: impl Fn(T, T) -> O,
) -> Vec<O> {
    match (l, r) {
        (Operand::Slice(a), Operand::Slice(b)) => a.iter().zip(b).map(|(x, y)| f(*x, *y)).collect(),
        (Operand::Slice(a), Operand::Scalar(y)) => a.iter().map(|x| f(*x, y)).collect(),
        (Operand::Scalar(x), Operand::Slice(b)) => b.iter().map(|y| f(x, *y)).collect(),
        (Operand::Scalar(x), Operand::Scalar(y)) => (0..len).map(|_| f(x, y)).collect(),
    }
}

pub(crate) fn i64_operand(v: &ColumnarValue) -> Option<Operand<'_, i64>> {
    match v {
        ColumnarValue::Column(c) => match &c.data {
            ColumnData::Int64(x) => Some(Operand::Slice(x)),
            _ => None,
        },
        ColumnarValue::Scalar(ScalarValue::Int64(x)) => Some(Operand::Scalar(*x)),
        _ => None,
    }
}

pub(crate) fn f64_operand(v: &ColumnarValue) -> Option<Operand<'_, f64>> {
    match v {
        ColumnarValue::Column(c) => match &c.data {
            ColumnData::Float64(x) => Some(Operand::Slice(x)),
            _ => None,
        },
        ColumnarValue::Scalar(ScalarValue::Float64(x)) => Some(Operand::Scalar(*x)),
        ColumnarValue::Scalar(ScalarValue::Int64(x)) => Some(Operand::Scalar(*x as f64)),
        _ => None,
    }
}

pub(crate) fn date_operand(v: &ColumnarValue) -> Option<Operand<'_, i32>> {
    match v {
        ColumnarValue::Column(c) => match &c.data {
            ColumnData::Date(x) => Some(Operand::Slice(x)),
            _ => None,
        },
        ColumnarValue::Scalar(ScalarValue::Date(x)) => Some(Operand::Scalar(*x)),
        _ => None,
    }
}

pub(crate) fn bool_operand(v: &ColumnarValue) -> Option<Operand<'_, bool>> {
    match v {
        ColumnarValue::Column(c) => match &c.data {
            ColumnData::Boolean(x) => Some(Operand::Slice(x)),
            _ => None,
        },
        ColumnarValue::Scalar(ScalarValue::Boolean(x)) => Some(Operand::Scalar(*x)),
        _ => None,
    }
}

/// Convert a boolean column into a selection mask: NULL counts as false.
pub fn to_mask(col: &Column) -> Vec<bool> {
    match (&col.data, &col.validity) {
        (ColumnData::Boolean(v), None) => v.clone(),
        (ColumnData::Boolean(v), Some(b)) => v.iter().enumerate().map(|(i, &x)| x && b.get(i)).collect(),
        _ => vec![false; col.len()],
    }
}

/// Row-wise select: `mask[i] ? a[i] : b[i]` (both columns have the same type).
pub fn zip(mask: &[bool], a: &Column, b: &Column) -> Column {
    use ColumnData::*;
    let data = match (&a.data, &b.data) {
        (Boolean(x), Boolean(y)) => Boolean(
            mask.iter()
                .enumerate()
                .map(|(i, &m)| if m { x[i] } else { y[i] })
                .collect(),
        ),
        (Int64(x), Int64(y)) => Int64(
            mask.iter()
                .enumerate()
                .map(|(i, &m)| if m { x[i] } else { y[i] })
                .collect(),
        ),
        (Float64(x), Float64(y)) => Float64(
            mask.iter()
                .enumerate()
                .map(|(i, &m)| if m { x[i] } else { y[i] })
                .collect(),
        ),
        (Date(x), Date(y)) => Date(
            mask.iter()
                .enumerate()
                .map(|(i, &m)| if m { x[i] } else { y[i] })
                .collect(),
        ),
        (Utf8(x), Utf8(y)) => Utf8(
            mask.iter()
                .enumerate()
                .map(|(i, &m)| if m { x.get(i) } else { y.get(i) })
                .collect(),
        ),
        _ => return b.clone(),
    };
    let validity = if a.validity.is_none() && b.validity.is_none() {
        None
    } else {
        Some(crate::column::Bitmap::from_bools(
            &mask
                .iter()
                .enumerate()
                .map(|(i, &m)| if m { a.is_valid(i) } else { b.is_valid(i) })
                .collect::<Vec<_>>(),
        ))
    };
    Column::new(data, validity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::BinaryOperator::*;
    use crate::column::ColumnBuilder;

    fn ints(v: &[Option<i64>]) -> ColumnarValue {
        let mut b = ColumnBuilder::new(DataType::Int64);
        for x in v {
            b.push(&x.map_or(ScalarValue::Null(DataType::Int64), ScalarValue::Int64));
        }
        ColumnarValue::Column(Arc::new(b.finish()))
    }

    fn bools(v: &[Option<bool>]) -> ColumnarValue {
        let mut b = ColumnBuilder::new(DataType::Boolean);
        for x in v {
            b.push(&x.map_or(ScalarValue::Null(DataType::Boolean), ScalarValue::Boolean));
        }
        ColumnarValue::Column(Arc::new(b.finish()))
    }

    fn values(v: ColumnarValue, n: usize, t: DataType) -> Vec<ScalarValue> {
        let c = v.into_column(n, t);
        (0..n).map(|i| c.value(i)).collect()
    }

    #[test]
    fn arithmetic_propagates_nulls_and_div_by_zero() {
        let a = ints(&[Some(7), None, Some(-7), Some(1)]);
        let b = ints(&[Some(2), Some(1), Some(2), Some(0)]);
        let r = arith::arith(Divide, &a, &b, 4, DataType::Int64).unwrap();
        assert_eq!(
            values(r, 4, DataType::Int64),
            vec![
                ScalarValue::Int64(3),
                ScalarValue::Null(DataType::Int64),
                ScalarValue::Int64(-3),
                ScalarValue::Null(DataType::Int64)
            ]
        );
        let s = ColumnarValue::Scalar(ScalarValue::Int64(10));
        let r = arith::arith(Minus, &s, &a, 4, DataType::Int64).unwrap();
        assert_eq!(values(r, 4, DataType::Int64)[0], ScalarValue::Int64(3));
    }

    #[test]
    fn overflow_in_null_slots_is_ignored() {
        // Row 1 is NULL but holds garbage that would overflow; only valid rows count.
        let big = ColumnarValue::Column(Arc::new(crate::column::Column::new(
            ColumnData::Int64(vec![1, i64::MAX]),
            Some(crate::column::Bitmap::from_bools(&[true, false])),
        )));
        let one = ColumnarValue::Scalar(ScalarValue::Int64(1));
        assert!(arith::arith(Plus, &big, &one, 2, DataType::Int64).is_ok());
        let max = ColumnarValue::Scalar(ScalarValue::Int64(i64::MAX));
        assert!(arith::arith(Plus, &max, &one, 1, DataType::Int64).is_err());
    }

    #[test]
    fn comparisons_with_scalars_and_strings() {
        let a = ints(&[Some(1), Some(5), None]);
        let r = cmp::compare(Gt, &a, &ColumnarValue::Scalar(ScalarValue::Int64(2)), 3).unwrap();
        assert_eq!(
            values(r, 3, DataType::Boolean),
            vec![
                ScalarValue::Boolean(false),
                ScalarValue::Boolean(true),
                ScalarValue::Null(DataType::Boolean)
            ]
        );
        let s: crate::column::StringData = ["b", "a", "c"].into_iter().collect();
        let sc = ColumnarValue::Column(Arc::new(crate::column::Column::from_data(ColumnData::Utf8(s))));
        let r = cmp::compare(LtEq, &sc, &ColumnarValue::Scalar(ScalarValue::Utf8("b".into())), 3).unwrap();
        assert_eq!(to_mask(&r.into_column(3, DataType::Boolean)), vec![true, true, false]);
    }

    #[test]
    fn kleene_logic_truth_table() {
        // All 9 combinations of {true, false, NULL} x {true, false, NULL}.
        let t = [Some(true), Some(false), None];
        let mut l = vec![];
        let mut r = vec![];
        for a in t {
            for b in t {
                l.push(a);
                r.push(b);
            }
        }
        let (lv, rv) = (bools(&l), bools(&r));
        let and = values(boolean::and(&lv, &rv, 9), 9, DataType::Boolean);
        let or = values(boolean::or(&lv, &rv, 9), 9, DataType::Boolean);
        let b = |x: Option<bool>| x.map_or(ScalarValue::Null(DataType::Boolean), ScalarValue::Boolean);
        let expect_and = [
            Some(true),
            Some(false),
            None,
            Some(false),
            Some(false),
            Some(false),
            None,
            Some(false),
            None,
        ];
        let expect_or = [
            Some(true),
            Some(true),
            Some(true),
            Some(true),
            Some(false),
            None,
            Some(true),
            None,
            None,
        ];
        assert_eq!(and, expect_and.map(b).to_vec());
        assert_eq!(or, expect_or.map(b).to_vec());
    }

    #[test]
    fn casts() {
        use cast::cast_scalar;
        assert_eq!(
            cast_scalar(&ScalarValue::Utf8("42".into()), DataType::Int64).unwrap(),
            ScalarValue::Int64(42)
        );
        assert_eq!(
            cast_scalar(&ScalarValue::Int64(3), DataType::Float64).unwrap(),
            ScalarValue::Float64(3.0)
        );
        assert_eq!(
            cast_scalar(&ScalarValue::Utf8("2020-02-29".into()), DataType::Date).unwrap(),
            ScalarValue::Date(crate::types::parse_date("2020-02-29").unwrap())
        );
        assert_eq!(
            cast_scalar(&ScalarValue::Date(0), DataType::Utf8).unwrap(),
            ScalarValue::Utf8("1970-01-01".into())
        );
        assert!(cast_scalar(&ScalarValue::Utf8("x".into()), DataType::Int64).is_err());
    }

    #[test]
    fn zip_selects_per_row() {
        let a = ints(&[Some(1), Some(2), None]).into_column(3, DataType::Int64);
        let b = ints(&[Some(9), None, Some(7)]).into_column(3, DataType::Int64);
        let z = zip(&[true, false, false], &a, &b);
        assert_eq!(z.value(0), ScalarValue::Int64(1));
        assert!(z.value(1).is_null());
        assert_eq!(z.value(2), ScalarValue::Int64(7));
    }
}
