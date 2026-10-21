//! Heuristic cardinality estimation (no histograms: table row counts plus
//! fixed selectivities, in the spirit of System R's "magic numbers").

use crate::ast::BinaryOperator;
use crate::logical::{Expr, JoinType, LogicalPlan};

pub fn selectivity(e: &Expr) -> f64 {
    match e {
        Expr::Binary {
            op: BinaryOperator::And,
            left,
            right,
        } => selectivity(left) * selectivity(right),
        Expr::Binary {
            op: BinaryOperator::Or,
            left,
            right,
        } => {
            let (a, b) = (selectivity(left), selectivity(right));
            a + b - a * b
        }
        Expr::Binary { op, .. } => match op {
            BinaryOperator::Eq => 0.1,
            BinaryOperator::NotEq => 0.9,
            BinaryOperator::Lt | BinaryOperator::LtEq | BinaryOperator::Gt | BinaryOperator::GtEq => 1.0 / 3.0,
            _ => 0.5,
        },
        Expr::Not(x) => 1.0 - selectivity(x),
        Expr::Like { negated, .. } => {
            if *negated {
                0.75
            } else {
                0.25
            }
        }
        Expr::InList { list, negated, .. } => {
            let s = (0.1 * list.len() as f64).min(0.5);
            if *negated { 1.0 - s } else { s }
        }
        Expr::IsNull { negated, .. } => {
            if *negated {
                0.9
            } else {
                0.1
            }
        }
        Expr::Literal(_) => 1.0,
        _ => 0.5,
    }
}

/// Estimated output row count of a plan.
pub fn estimate_rows(plan: &LogicalPlan) -> f64 {
    match plan {
        LogicalPlan::TableScan(s) => s.source.num_rows as f64 * s.filters.iter().map(selectivity).product::<f64>(),
        LogicalPlan::Filter { input, predicate } => estimate_rows(input) * selectivity(predicate),
        LogicalPlan::Projection { input, .. } | LogicalPlan::SubqueryAlias { input, .. } => estimate_rows(input),
        LogicalPlan::Sort { input, fetch, .. } => {
            let n = estimate_rows(input);
            fetch.map_or(n, |f| n.min(f as f64))
        }
        LogicalPlan::Limit { input, fetch, .. } => {
            let n = estimate_rows(input);
            fetch.map_or(n, |f| n.min(f as f64))
        }
        LogicalPlan::Aggregate { input, group_exprs, .. } => {
            if group_exprs.is_empty() {
                1.0
            } else {
                (estimate_rows(input) * 0.1).max(1.0)
            }
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            filter,
            ..
        } => {
            let (l, r) = (estimate_rows(left), estimate_rows(right));
            let base = if on.is_empty() { l * r } else { l.max(r) }; // FK-PK assumption
            let base = base * filter.as_ref().map_or(1.0, selectivity);
            if *join_type == JoinType::Left {
                base.max(l)
            } else {
                base
            }
        }
        LogicalPlan::Values { rows, .. } => rows.len() as f64,
    }
}
