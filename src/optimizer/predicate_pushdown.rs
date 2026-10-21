//! Predicate pushdown.
//!
//! Filters are split into conjuncts and pushed down the tree:
//! * through projections and subquery aliases (rewriting column references),
//! * below aggregates when they only reference grouping keys,
//! * into the matching side of a join; equalities spanning both sides become
//!   hash-join keys (so `FROM a, b WHERE a.x = b.y` becomes an equi-join),
//! * finally into table scans, where they run on freshly read batches.
//!
//! A LEFT JOIN whose WHERE clause rejects NULLs from the right side is
//! converted to an INNER JOIN first, which unlocks further pushdown.

use super::OptimizerRule;
use crate::ast::BinaryOperator;
use crate::column::Schema;
use crate::compute::functions::ScalarFunc;
use crate::error::Result;
use crate::logical::binder::split_equi;
use crate::logical::expr::{conjunction, split_conjunction};
use crate::logical::{ColumnRef, Expr, JoinType, LogicalPlan};

pub struct PredicatePushdown;

pub(crate) fn refs_subset_of(e: &Expr, schema: &Schema) -> bool {
    e.columns()
        .iter()
        .all(|c| matches!(schema.index_of(c.relation.as_deref(), &c.name), Ok(Some(_))))
}

/// Replace column references using `f` (returns `None` to keep the column).
pub(crate) fn replace_columns(e: Expr, f: &dyn Fn(&ColumnRef) -> Option<Expr>) -> Result<Expr> {
    e.transform_up(&mut |x| {
        Ok(match &x {
            Expr::Column(c) => f(c).unwrap_or(x),
            _ => x,
        })
    })
}

/// Does `p` evaluate to NULL/false whenever every column of `side` is NULL?
fn is_null_rejecting(p: &Expr, side: &Schema) -> bool {
    let references_side = p
        .columns()
        .iter()
        .any(|c| matches!(side.index_of(c.relation.as_deref(), &c.name), Ok(Some(_))));
    if !references_side {
        return false;
    }
    let unsafe_node = p.any(&|e| {
        matches!(
            e,
            Expr::IsNull { negated: false, .. }
                | Expr::Case { .. }
                | Expr::Function {
                    func: ScalarFunc::Coalesce | ScalarFunc::NullIf,
                    ..
                }
                | Expr::Binary {
                    op: BinaryOperator::Or,
                    ..
                }
        )
    });
    if unsafe_node {
        return false;
    }
    match p {
        Expr::Binary { op, .. } if op.is_comparison() => true,
        Expr::Like { .. } | Expr::InList { .. } | Expr::IsNull { negated: true, .. } => true,
        _ => false,
    }
}

fn wrap(plan: LogicalPlan, preds: Vec<Expr>) -> LogicalPlan {
    match conjunction(preds) {
        Some(p) => LogicalPlan::filter(plan, p),
        None => plan,
    }
}

fn push(plan: LogicalPlan, mut preds: Vec<Expr>) -> Result<LogicalPlan> {
    match plan {
        LogicalPlan::Filter { input, predicate } => {
            preds.extend(split_conjunction(predicate));
            push(*input, preds)
        }
        LogicalPlan::Projection { input, exprs, schema } => {
            let subst = |c: &ColumnRef| -> Option<Expr> {
                let i = schema.index_of(c.relation.as_deref(), &c.name).ok().flatten()?;
                Some(exprs[i].unalias().clone())
            };
            let pushed = preds
                .into_iter()
                .map(|p| replace_columns(p, &subst))
                .collect::<Result<Vec<_>>>()?;
            LogicalPlan::projection(push(*input, pushed)?, exprs)
        }
        LogicalPlan::SubqueryAlias { input, alias, schema } => {
            let inner = input.schema();
            let subst = |c: &ColumnRef| -> Option<Expr> {
                let i = schema.index_of(c.relation.as_deref(), &c.name).ok().flatten()?;
                let f = inner.field(i);
                Some(Expr::Column(ColumnRef::new(f.qualifier.as_deref(), &f.name)))
            };
            let pushed = preds
                .into_iter()
                .map(|p| replace_columns(p, &subst))
                .collect::<Result<Vec<_>>>()?;
            Ok(LogicalPlan::subquery_alias(push(*input, pushed)?, &alias))
        }
        LogicalPlan::Aggregate {
            input,
            group_exprs,
            aggr_exprs,
            schema,
        } => {
            let ng = group_exprs.len();
            let mut below = vec![];
            let mut above = vec![];
            for p in preds {
                let cols = p.columns();
                let group_only = !cols.is_empty()
                    && cols
                        .iter()
                        .all(|c| matches!(schema.index_of(c.relation.as_deref(), &c.name), Ok(Some(i)) if i < ng));
                if group_only {
                    let subst = |c: &ColumnRef| -> Option<Expr> {
                        let i = schema.index_of(c.relation.as_deref(), &c.name).ok().flatten()?;
                        Some(group_exprs[i].unalias().clone())
                    };
                    below.push(replace_columns(p, &subst)?);
                } else {
                    above.push(p);
                }
            }
            let agg = LogicalPlan::aggregate(push(*input, below)?, group_exprs, aggr_exprs)?;
            Ok(wrap(agg, above))
        }
        LogicalPlan::Join {
            left,
            right,
            mut join_type,
            on,
            filter,
            ..
        } => {
            let (ls, rs) = (left.schema(), right.schema());
            if join_type == JoinType::Left && preds.iter().any(|p| is_null_rejecting(p, &rs)) {
                join_type = JoinType::Inner;
            }
            let mut left_preds = vec![];
            let mut right_preds = vec![];
            let mut keys = on;
            let mut residual = vec![];
            let mut above = vec![];
            let on_conds = filter.map(split_conjunction).unwrap_or_default();
            match join_type {
                JoinType::Inner => {
                    for p in preds.into_iter().chain(on_conds) {
                        if refs_subset_of(&p, &ls) {
                            left_preds.push(p);
                        } else if refs_subset_of(&p, &rs) {
                            right_preds.push(p);
                        } else if let Some(pair) = split_equi(&p, &ls, &rs) {
                            keys.push(pair);
                        } else {
                            residual.push(p);
                        }
                    }
                }
                JoinType::Left => {
                    for p in preds {
                        if refs_subset_of(&p, &ls) {
                            left_preds.push(p);
                        } else {
                            above.push(p);
                        }
                    }
                    // ON conditions may only filter the inner (right) side early.
                    for p in on_conds {
                        if refs_subset_of(&p, &rs) && !p.columns().is_empty() {
                            right_preds.push(p);
                        } else if let Some(pair) = split_equi(&p, &ls, &rs) {
                            keys.push(pair);
                        } else {
                            residual.push(p);
                        }
                    }
                }
            }
            let join = LogicalPlan::join(
                push(*left, left_preds)?,
                push(*right, right_preds)?,
                join_type,
                keys,
                conjunction(residual),
            );
            Ok(wrap(join, above))
        }
        LogicalPlan::Sort { input, exprs, fetch } => Ok(LogicalPlan::Sort {
            input: Box::new(push(*input, preds)?),
            exprs,
            fetch,
        }),
        LogicalPlan::TableScan(mut s) => {
            for p in preds {
                if !s.filters.contains(&p) {
                    s.filters.push(p);
                }
            }
            Ok(LogicalPlan::TableScan(s))
        }
        LogicalPlan::Limit { input, skip, fetch } => {
            let inner = LogicalPlan::Limit {
                input: Box::new(push(*input, vec![])?),
                skip,
                fetch,
            };
            Ok(wrap(inner, preds))
        }
        v @ LogicalPlan::Values { .. } => Ok(wrap(v, preds)),
    }
}

impl OptimizerRule for PredicatePushdown {
    fn name(&self) -> &'static str {
        "predicate_pushdown"
    }
    fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        push(plan, vec![])
    }
}
