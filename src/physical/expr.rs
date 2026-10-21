//! Physical (index-resolved) expressions and their vectorized evaluation.

use crate::ast::BinaryOperator;
use crate::column::{Batch, Bitmap, Column, ColumnData, Schema};
use crate::compute::hash::FxHashSet;
use crate::compute::like::Like;
use crate::compute::{ColumnarValue, arith, boolean, cast, cmp, functions, to_mask, zip};
use crate::err;
use crate::error::Result;
use crate::logical::Expr;
use crate::types::{DataType, ScalarValue};
use std::fmt;
use std::sync::Arc;

/// Pre-built lookup set for `IN (literal, ...)`.
#[derive(Debug, Clone)]
pub enum InSet {
    Int(FxHashSet<i64>),
    Date(FxHashSet<i32>),
    Str(FxHashSet<String>),
    Other(Vec<ScalarValue>),
}

#[derive(Debug, Clone)]
pub enum PhysicalExpr {
    Column(usize, String),
    Literal(ScalarValue),
    Binary {
        op: BinaryOperator,
        left: Box<PhysicalExpr>,
        right: Box<PhysicalExpr>,
        out: DataType,
    },
    Not(Box<PhysicalExpr>),
    Negative(Box<PhysicalExpr>),
    IsNull(Box<PhysicalExpr>, bool),
    Like {
        expr: Box<PhysicalExpr>,
        pattern: Box<PhysicalExpr>,
        /// Compiled once when the pattern is a literal.
        compiled: Option<Like>,
        negated: bool,
        case_insensitive: bool,
    },
    InList {
        expr: Box<PhysicalExpr>,
        set: InSet,
        has_null: bool,
        negated: bool,
        display: String,
    },
    Case {
        when_then: Vec<(PhysicalExpr, PhysicalExpr)>,
        else_expr: Box<PhysicalExpr>,
        out: DataType,
    },
    Cast(Box<PhysicalExpr>, DataType),
    Function {
        func: functions::ScalarFunc,
        args: Vec<PhysicalExpr>,
        out: DataType,
    },
}

/// Lower a logical expression, resolving column references against `schema`.
pub fn create_physical_expr(e: &Expr, schema: &Schema) -> Result<PhysicalExpr> {
    let rec = |x: &Expr| create_physical_expr(x, schema).map(Box::new);
    Ok(match e {
        Expr::Column(c) => PhysicalExpr::Column(c.index_in(schema)?, c.name.clone()),
        Expr::Literal(v) => PhysicalExpr::Literal(v.clone()),
        Expr::Alias(inner, _) => create_physical_expr(inner, schema)?,
        Expr::Binary { left, op, right } => PhysicalExpr::Binary {
            op: *op,
            left: rec(left)?,
            right: rec(right)?,
            out: e.data_type(schema)?,
        },
        Expr::Not(x) => PhysicalExpr::Not(rec(x)?),
        Expr::Negative(x) => PhysicalExpr::Negative(rec(x)?),
        Expr::IsNull { expr, negated } => PhysicalExpr::IsNull(rec(expr)?, *negated),
        Expr::Like {
            expr,
            pattern,
            negated,
            case_insensitive,
        } => {
            let compiled = match pattern.as_ref() {
                Expr::Literal(ScalarValue::Utf8(p)) => Some(Like::new(p, *case_insensitive)),
                _ => None,
            };
            PhysicalExpr::Like {
                expr: rec(expr)?,
                pattern: rec(pattern)?,
                compiled,
                negated: *negated,
                case_insensitive: *case_insensitive,
            }
        }
        Expr::InList { expr, list, negated } => {
            let values: Vec<ScalarValue> = list
                .iter()
                .map(|x| match x {
                    Expr::Literal(v) => Ok(v.clone()),
                    other => err!(Plan, "IN list item {other} is not a constant"),
                })
                .collect::<Result<_>>()?;
            let has_null = values.iter().any(|v| v.is_null());
            let non_null = values.iter().filter(|v| !v.is_null());
            let set = match expr.data_type(schema)? {
                DataType::Int64 => InSet::Int(non_null.filter_map(|v| v.as_i64()).collect()),
                DataType::Date => InSet::Date(
                    non_null
                        .filter_map(|v| match v {
                            ScalarValue::Date(d) => Some(*d),
                            _ => None,
                        })
                        .collect(),
                ),
                DataType::Utf8 => InSet::Str(
                    non_null
                        .filter_map(|v| match v {
                            ScalarValue::Utf8(s) => Some(s.clone()),
                            _ => None,
                        })
                        .collect(),
                ),
                _ => InSet::Other(non_null.cloned().collect()),
            };
            PhysicalExpr::InList {
                expr: rec(expr)?,
                set,
                has_null,
                negated: *negated,
                display: e.to_string(),
            }
        }
        Expr::Case { when_then, else_expr } => PhysicalExpr::Case {
            when_then: when_then
                .iter()
                .map(|(w, t)| Ok((create_physical_expr(w, schema)?, create_physical_expr(t, schema)?)))
                .collect::<Result<_>>()?,
            else_expr: match else_expr {
                Some(x) => rec(x)?,
                None => Box::new(PhysicalExpr::Literal(ScalarValue::Null(e.data_type(schema)?))),
            },
            out: e.data_type(schema)?,
        },
        Expr::Cast { expr, data_type } => PhysicalExpr::Cast(rec(expr)?, *data_type),
        Expr::Function { func, args } => PhysicalExpr::Function {
            func: *func,
            args: args
                .iter()
                .map(|a| create_physical_expr(a, schema))
                .collect::<Result<_>>()?,
            out: e.data_type(schema)?,
        },
        Expr::Aggregate(_) => return err!(Plan, "aggregate expression {e} in non-aggregate context"),
    })
}

impl PhysicalExpr {
    pub fn evaluate(&self, batch: &Batch) -> Result<ColumnarValue> {
        let n = batch.num_rows;
        Ok(match self {
            PhysicalExpr::Column(i, _) => ColumnarValue::Column(batch.columns[*i].clone()),
            PhysicalExpr::Literal(v) => ColumnarValue::Scalar(v.clone()),
            PhysicalExpr::Binary { op, left, right, out } => {
                let l = left.evaluate(batch)?;
                let r = right.evaluate(batch)?;
                match op {
                    BinaryOperator::And => boolean::and(&l, &r, n),
                    BinaryOperator::Or => boolean::or(&l, &r, n),
                    op if op.is_comparison() => cmp::compare(*op, &l, &r, n)?,
                    op => arith::arith(*op, &l, &r, n, *out)?,
                }
            }
            PhysicalExpr::Not(x) => boolean::not(&x.evaluate(batch)?),
            PhysicalExpr::Negative(x) => negate(x.evaluate(batch)?)?,
            PhysicalExpr::IsNull(x, negated) => boolean::is_null(&x.evaluate(batch)?, n, *negated),
            PhysicalExpr::Like {
                expr,
                pattern,
                compiled,
                negated,
                case_insensitive,
            } => {
                let v = expr.evaluate(batch)?;
                let p = pattern.evaluate(batch)?;
                eval_like(v, p, compiled.as_ref(), *negated, *case_insensitive, n)?
            }
            PhysicalExpr::InList {
                expr,
                set,
                has_null,
                negated,
                ..
            } => eval_in(expr.evaluate(batch)?, set, *has_null, *negated, n),
            PhysicalExpr::Case {
                when_then,
                else_expr,
                out,
            } => {
                let mut result = else_expr.evaluate(batch)?.into_column(n, *out);
                for (w, t) in when_then.iter().rev() {
                    let cond = w.evaluate(batch)?.into_column(n, DataType::Boolean);
                    let mask = to_mask(&cond);
                    let then = t.evaluate(batch)?.into_column(n, *out);
                    result = Arc::new(zip(&mask, &then, &result));
                }
                ColumnarValue::Column(result)
            }
            PhysicalExpr::Cast(x, to) => match x.evaluate(batch)? {
                ColumnarValue::Scalar(s) => ColumnarValue::Scalar(cast::cast_scalar(&s, *to)?),
                ColumnarValue::Column(c) => ColumnarValue::Column(Arc::new(cast::cast_column(&c, *to)?)),
            },
            PhysicalExpr::Function { func, args, out } => {
                let vals = args.iter().map(|a| a.evaluate(batch)).collect::<Result<Vec<_>>>()?;
                functions::eval(*func, &vals, n, *out)?
            }
        })
    }

    /// Evaluate and always return a full column of `n` rows.
    pub fn evaluate_to_column(&self, batch: &Batch, dtype: DataType) -> Result<Arc<Column>> {
        Ok(self.evaluate(batch)?.into_column(batch.num_rows, dtype))
    }
}

fn negate(v: ColumnarValue) -> Result<ColumnarValue> {
    Ok(match v {
        ColumnarValue::Scalar(ScalarValue::Int64(x)) => ColumnarValue::Scalar(ScalarValue::Int64(-x)),
        ColumnarValue::Scalar(ScalarValue::Float64(x)) => ColumnarValue::Scalar(ScalarValue::Float64(-x)),
        ColumnarValue::Scalar(s) => ColumnarValue::Scalar(s),
        ColumnarValue::Column(c) => {
            let data = match &c.data {
                ColumnData::Int64(v) => ColumnData::Int64(v.iter().map(|x| x.wrapping_neg()).collect()),
                ColumnData::Float64(v) => ColumnData::Float64(v.iter().map(|x| -x).collect()),
                _ => return err!(Execution, "cannot negate {}", c.data_type()),
            };
            ColumnarValue::Column(Arc::new(Column::new(data, c.validity.clone())))
        }
    })
}

fn eval_like(
    v: ColumnarValue,
    p: ColumnarValue,
    compiled: Option<&Like>,
    negated: bool,
    ci: bool,
    n: usize,
) -> Result<ColumnarValue> {
    if v.is_null_scalar() || p.is_null_scalar() {
        return Ok(ColumnarValue::Scalar(ScalarValue::Null(DataType::Boolean)));
    }
    let col = v.into_column(n, DataType::Utf8);
    let ColumnData::Utf8(s) = &col.data else {
        return err!(Execution, "LIKE expects strings");
    };
    let mut validity = col.validity.clone();
    let vals: Vec<bool> = match (compiled, &p) {
        (Some(m), _) => s.iter().map(|x| m.matches(x) != negated).collect(),
        (None, ColumnarValue::Scalar(ScalarValue::Utf8(pat))) => {
            let m = Like::new(pat, ci);
            s.iter().map(|x| m.matches(x) != negated).collect()
        }
        (None, _) => {
            let pc = p.clone().into_column(n, DataType::Utf8);
            let ColumnData::Utf8(ps) = &pc.data else {
                return err!(Execution, "LIKE pattern must be a string");
            };
            validity = crate::column::and_validity(validity.as_ref(), pc.validity.as_ref());
            (0..n)
                .map(|i| Like::new(ps.get(i), ci).matches(s.get(i)) != negated)
                .collect()
        }
    };
    Ok(ColumnarValue::Column(Arc::new(Column::new(
        ColumnData::Boolean(vals),
        validity,
    ))))
}

fn eval_in(v: ColumnarValue, set: &InSet, has_null: bool, negated: bool, n: usize) -> ColumnarValue {
    let (col, scalar) = match v {
        ColumnarValue::Scalar(s) => (Arc::new(Column::from_scalar(&s, 1, s.data_type())), true),
        ColumnarValue::Column(c) => (c, false),
    };
    let len = if scalar { 1 } else { n };
    let found: Vec<bool> = match (&col.data, set) {
        (ColumnData::Int64(v), InSet::Int(s)) => v.iter().map(|x| s.contains(x)).collect(),
        (ColumnData::Date(v), InSet::Date(s)) => v.iter().map(|x| s.contains(x)).collect(),
        (ColumnData::Utf8(v), InSet::Str(s)) => v.iter().map(|x| s.contains(x)).collect(),
        (_, InSet::Other(list)) => (0..len)
            .map(|i| {
                let x = col.value(i);
                list.iter().any(|y| x.total_cmp(y) == std::cmp::Ordering::Equal)
            })
            .collect(),
        _ => vec![false; len],
    };
    // SQL semantics: no match + NULL in list => NULL.
    let valid: Vec<bool> = (0..len).map(|i| col.is_valid(i) && (found[i] || !has_null)).collect();
    let vals: Vec<bool> = found.into_iter().map(|f| f != negated).collect();
    let out = Column::new(ColumnData::Boolean(vals), Some(Bitmap::from_bools(&valid)));
    if scalar {
        ColumnarValue::Scalar(out.value(0))
    } else {
        ColumnarValue::Column(Arc::new(out))
    }
}

impl fmt::Display for PhysicalExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhysicalExpr::Column(i, name) => write!(f, "{name}@{i}"),
            PhysicalExpr::Literal(ScalarValue::Utf8(s)) => write!(f, "'{s}'"),
            PhysicalExpr::Literal(v) => write!(f, "{v}"),
            PhysicalExpr::Binary { op, left, right, .. } => write!(f, "({left} {op} {right})"),
            PhysicalExpr::Not(x) => write!(f, "NOT {x}"),
            PhysicalExpr::Negative(x) => write!(f, "-{x}"),
            PhysicalExpr::IsNull(x, neg) => write!(f, "{x} IS {}NULL", if *neg { "NOT " } else { "" }),
            PhysicalExpr::Like {
                expr, pattern, negated, ..
            } => {
                write!(f, "{expr} {}LIKE {pattern}", if *negated { "NOT " } else { "" })
            }
            PhysicalExpr::InList { display, .. } => f.write_str(display),
            PhysicalExpr::Case {
                when_then, else_expr, ..
            } => {
                write!(f, "CASE")?;
                for (w, t) in when_then {
                    write!(f, " WHEN {w} THEN {t}")?;
                }
                write!(f, " ELSE {else_expr} END")
            }
            PhysicalExpr::Cast(x, t) => write!(f, "CAST({x} AS {t})"),
            PhysicalExpr::Function { func, args, .. } => {
                let a: Vec<String> = args.iter().map(|x| x.to_string()).collect();
                write!(f, "{func}({})", a.join(", "))
            }
        }
    }
}
