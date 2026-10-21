//! Constant folding and boolean simplification.

use super::{OptimizerRule, map_children};
use crate::ast::BinaryOperator;
use crate::column::{Batch, Schema};
use crate::error::Result;
use crate::logical::{Expr, LogicalPlan, SortExpr};
use crate::physical::expr::create_physical_expr;
use crate::types::ScalarValue;

pub struct ConstantFolding;

fn is_bool(e: &Expr, v: bool) -> bool {
    matches!(e, Expr::Literal(ScalarValue::Boolean(b)) if *b == v)
}

/// Evaluate an expression that references no columns.
fn evaluate_constant(e: &Expr) -> Option<ScalarValue> {
    let schema = Schema::empty();
    let phys = create_physical_expr(e, &schema).ok()?;
    let batch = Batch::new(vec![], 1);
    let v = phys.evaluate(&batch).ok()?;
    Some(match v {
        crate::compute::ColumnarValue::Scalar(s) => s,
        crate::compute::ColumnarValue::Column(c) => c.value(0),
    })
}

fn simplify(e: Expr) -> Result<Expr> {
    Ok(match e {
        Expr::Binary {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            if is_bool(&left, false) || is_bool(&right, false) {
                Expr::Literal(ScalarValue::Boolean(false))
            } else if is_bool(&left, true) {
                *right
            } else if is_bool(&right, true) {
                *left
            } else {
                fold(Expr::Binary {
                    left,
                    op: BinaryOperator::And,
                    right,
                })
            }
        }
        Expr::Binary {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            if is_bool(&left, true) || is_bool(&right, true) {
                Expr::Literal(ScalarValue::Boolean(true))
            } else if is_bool(&left, false) {
                *right
            } else if is_bool(&right, false) {
                *left
            } else {
                fold(Expr::Binary {
                    left,
                    op: BinaryOperator::Or,
                    right,
                })
            }
        }
        Expr::Not(inner) => match *inner {
            Expr::Not(x) => *x,
            other => fold(Expr::Not(Box::new(other))),
        },
        other => fold(other),
    })
}

fn fold(e: Expr) -> Expr {
    let foldable = !matches!(e, Expr::Literal(_) | Expr::Alias(..) | Expr::Column(_))
        && e.columns().is_empty()
        && !e.contains_aggregate()
        // Interval literals only exist as operands; keep them inside their parent.
        && !matches!(e, Expr::Literal(ScalarValue::Interval { .. }));
    if foldable && let Some(v) = evaluate_constant(&e) {
        return Expr::Literal(v);
    }
    e
}

pub fn fold_expr(e: Expr) -> Result<Expr> {
    e.transform_up(&mut simplify)
}

fn fold_plan(plan: LogicalPlan) -> Result<LogicalPlan> {
    let plan = map_children(plan, &mut fold_plan)?;
    Ok(match plan {
        LogicalPlan::Filter { input, predicate } => {
            let predicate = fold_expr(predicate)?;
            match &predicate {
                Expr::Literal(ScalarValue::Boolean(true)) => *input,
                Expr::Literal(_) => LogicalPlan::empty(input.schema()),
                _ => LogicalPlan::filter(*input, predicate),
            }
        }
        LogicalPlan::TableScan(mut s) => {
            s.filters = s.filters.into_iter().map(fold_expr).collect::<Result<_>>()?;
            LogicalPlan::TableScan(s)
        }
        LogicalPlan::Projection { input, exprs, .. } => {
            LogicalPlan::projection(*input, exprs.into_iter().map(fold_expr).collect::<Result<_>>()?)?
        }
        LogicalPlan::Aggregate {
            input,
            group_exprs,
            aggr_exprs,
            ..
        } => LogicalPlan::aggregate(
            *input,
            group_exprs.into_iter().map(fold_expr).collect::<Result<_>>()?,
            aggr_exprs.into_iter().map(fold_expr).collect::<Result<_>>()?,
        )?,
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            filter,
            ..
        } => {
            let filter = filter.map(fold_expr).transpose()?.filter(|f| !is_bool(f, true));
            let on = on
                .into_iter()
                .map(|(l, r)| Ok((fold_expr(l)?, fold_expr(r)?)))
                .collect::<Result<_>>()?;
            LogicalPlan::join(*left, *right, join_type, on, filter)
        }
        LogicalPlan::Sort { input, exprs, fetch } => LogicalPlan::Sort {
            input,
            exprs: exprs
                .into_iter()
                .map(|s| {
                    Ok(SortExpr {
                        expr: fold_expr(s.expr)?,
                        ..s
                    })
                })
                .collect::<Result<_>>()?,
            fetch,
        },
        other => other,
    })
}

impl OptimizerRule for ConstantFolding {
    fn name(&self) -> &'static str {
        "constant_folding"
    }
    fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        fold_plan(plan)
    }
}
