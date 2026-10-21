//! Built-in scalar functions.

use super::{ColumnarValue, zip};
use crate::column::{Bitmap, Column, ColumnData, StringData};
use crate::err;
use crate::error::Result;
use crate::types::{DataType, civil_from_days};
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScalarFunc {
    Abs,
    Round,
    Floor,
    Ceil,
    Sqrt,
    Upper,
    Lower,
    Length,
    Trim,
    Substr,
    Coalesce,
    NullIf,
    Year,
    Month,
    Day,
}

impl ScalarFunc {
    pub fn from_name(name: &str) -> Option<Self> {
        use ScalarFunc::*;
        Some(match name {
            "abs" => Abs,
            "round" => Round,
            "floor" => Floor,
            "ceil" | "ceiling" => Ceil,
            "sqrt" => Sqrt,
            "upper" => Upper,
            "lower" => Lower,
            "length" | "char_length" => Length,
            "trim" => Trim,
            "substr" | "substring" => Substr,
            "coalesce" => Coalesce,
            "nullif" => NullIf,
            "year" => Year,
            "month" => Month,
            "day" => Day,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        use ScalarFunc::*;
        match self {
            Abs => "abs",
            Round => "round",
            Floor => "floor",
            Ceil => "ceil",
            Sqrt => "sqrt",
            Upper => "upper",
            Lower => "lower",
            Length => "length",
            Trim => "trim",
            Substr => "substr",
            Coalesce => "coalesce",
            NullIf => "nullif",
            Year => "year",
            Month => "month",
            Day => "day",
        }
    }

    /// Accepted argument counts (min, max).
    pub fn arity(self) -> (usize, usize) {
        use ScalarFunc::*;
        match self {
            Round => (1, 2),
            Substr => (2, 3),
            Coalesce => (1, usize::MAX),
            NullIf => (2, 2),
            _ => (1, 1),
        }
    }
}

impl fmt::Display for ScalarFunc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

fn map_f64(c: &Column, f: impl Fn(f64) -> f64) -> Column {
    let vals = c.as_f64().unwrap_or(&[]).iter().map(|&x| f(x)).collect();
    Column::new(ColumnData::Float64(vals), c.validity.clone())
}

fn map_str(c: &Column, f: impl Fn(&str) -> String) -> Column {
    let ColumnData::Utf8(s) = &c.data else {
        return c.clone();
    };
    let mut out = StringData::with_capacity(s.len(), s.total_bytes());
    for v in s.iter() {
        out.push(&f(v));
    }
    Column::new(ColumnData::Utf8(out), c.validity.clone())
}

fn map_date(c: &Column, f: impl Fn((i32, u32, u32)) -> i64) -> Column {
    let ColumnData::Date(d) = &c.data else {
        return c.clone();
    };
    let vals = d.iter().map(|&x| f(civil_from_days(x))).collect();
    Column::new(ColumnData::Int64(vals), c.validity.clone())
}

pub fn eval(func: ScalarFunc, args: &[ColumnarValue], len: usize, out: DataType) -> Result<ColumnarValue> {
    use ScalarFunc::*;
    let all_scalar = args.iter().all(|a| matches!(a, ColumnarValue::Scalar(_)));
    let n = if all_scalar { 1 } else { len };
    let cols: Vec<Arc<Column>> = args
        .iter()
        .map(|a| {
            let dt = match a {
                ColumnarValue::Column(c) => c.data_type(),
                ColumnarValue::Scalar(s) if s.data_type() == DataType::Null => out,
                ColumnarValue::Scalar(s) => s.data_type(),
            };
            a.clone().into_column(n, dt)
        })
        .collect();
    let c0 = &cols[0];
    let result = match func {
        Abs => match &c0.data {
            ColumnData::Int64(v) => Column::new(
                ColumnData::Int64(v.iter().map(|x| x.wrapping_abs()).collect()),
                c0.validity.clone(),
            ),
            _ => map_f64(c0, f64::abs),
        },
        Floor | Ceil if c0.data_type() == DataType::Int64 => (**c0).clone(),
        Floor => map_f64(c0, f64::floor),
        Ceil => map_f64(c0, f64::ceil),
        Sqrt => map_f64(c0, f64::sqrt),
        Round => {
            if c0.data_type() == DataType::Int64 {
                (**c0).clone()
            } else {
                let digits = cols.get(1).and_then(|c| c.as_i64().map(|v| v.to_vec()));
                let vals: Vec<f64> = c0
                    .as_f64()
                    .unwrap_or(&[])
                    .iter()
                    .enumerate()
                    .map(|(i, &x)| {
                        let d = digits.as_ref().map_or(0, |d| d[i]);
                        let p = 10f64.powi(d as i32);
                        (x * p).round() / p
                    })
                    .collect();
                let validity =
                    crate::column::and_validity(c0.validity.as_ref(), cols.get(1).and_then(|c| c.validity.as_ref()));
                Column::new(ColumnData::Float64(vals), validity)
            }
        }
        Upper => map_str(c0, |s| s.to_uppercase()),
        Lower => map_str(c0, |s| s.to_lowercase()),
        Trim => map_str(c0, |s| s.trim().to_string()),
        Length => {
            let ColumnData::Utf8(s) = &c0.data else {
                return err!(Execution, "length() expects a string");
            };
            let vals = s.iter().map(|v| v.chars().count() as i64).collect();
            Column::new(ColumnData::Int64(vals), c0.validity.clone())
        }
        Substr => {
            let ColumnData::Utf8(s) = &c0.data else {
                return err!(Execution, "substr() expects a string");
            };
            let starts = cols[1].as_i64().unwrap_or(&[]);
            let lens = cols.get(2).and_then(|c| c.as_i64());
            let mut out_s = StringData::with_capacity(n, s.total_bytes());
            for i in 0..n {
                let v = s.get(i);
                // SQL semantics: 1-based start; start < 1 eats into the length.
                let start = starts[i];
                let end = lens.map(|l| start.saturating_add(l[i].max(0)));
                let from = (start.max(1) - 1) as usize;
                let to = end.map(|e| (e.max(1) - 1) as usize);
                let sub: String = match to {
                    Some(t) if t <= from => String::new(),
                    Some(t) => v.chars().skip(from).take(t - from).collect(),
                    None => v.chars().skip(from).collect(),
                };
                out_s.push(&sub);
            }
            let mut validity = c0.validity.clone();
            for c in &cols[1..] {
                validity = crate::column::and_validity(validity.as_ref(), c.validity.as_ref());
            }
            Column::new(ColumnData::Utf8(out_s), validity)
        }
        Coalesce => {
            let mut acc = (*cols[cols.len() - 1]).clone();
            for c in cols[..cols.len() - 1].iter().rev() {
                let mask: Vec<bool> = (0..n).map(|i| c.is_valid(i)).collect();
                acc = zip(&mask, c, &acc);
            }
            acc
        }
        NullIf => {
            let eq = super::cmp::compare(
                crate::ast::BinaryOperator::Eq,
                &ColumnarValue::Column(cols[0].clone()),
                &ColumnarValue::Column(cols[1].clone()),
                n,
            )?
            .into_column(n, DataType::Boolean);
            let mask = super::to_mask(&eq);
            let valid: Vec<bool> = (0..n).map(|i| c0.is_valid(i) && !mask[i]).collect();
            Column::new(c0.data.clone(), Some(Bitmap::from_bools(&valid)))
        }
        Year => map_date(c0, |(y, _, _)| y as i64),
        Month => map_date(c0, |(_, m, _)| m as i64),
        Day => map_date(c0, |(_, _, d)| d as i64),
    };
    Ok(if all_scalar {
        ColumnarValue::Scalar(result.value(0))
    } else {
        ColumnarValue::Column(Arc::new(result))
    })
}
