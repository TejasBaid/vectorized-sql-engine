//! Projection pushdown (column pruning): compute which columns each operator
//! actually needs and only read/carry those from the scans upward.

use super::OptimizerRule;
use crate::error::Result;
use crate::logical::{ColumnRef, Expr, LogicalPlan};
use std::collections::BTreeSet;

pub struct ProjectionPushdown;

type Required = Option<BTreeSet<ColumnRef>>;

fn add_columns(req: &Required, exprs: impl IntoIterator<Item = Expr>) -> Required {
    let mut set = req.clone()?;
    for e in exprs {
        e.collect_columns(&mut set);
    }
    Some(set)
}

fn prune(plan: LogicalPlan, required: Required) -> Result<LogicalPlan> {
    match plan {
        LogicalPlan::TableScan(mut s) => {
            let Some(req) = required else {
                return Ok(LogicalPlan::TableScan(s));
            };
            let mut need = req;
            for f in &s.filters {
                f.collect_columns(&mut need);
            }
            let current: Vec<usize> = s
                .projection
                .clone()
                .unwrap_or_else(|| (0..s.source.schema.len()).collect());
            let keep: Vec<usize> = current
                .into_iter()
                .filter(|&i| {
                    let f = s.source.schema.field(i);
                    need.contains(&ColumnRef::new(Some(&s.qualifier), &f.name))
                })
                .collect();
            s.projection = Some(keep);
            s.recompute_schema();
            Ok(LogicalPlan::TableScan(s))
        }
        LogicalPlan::Filter { input, predicate } => {
            let child = add_columns(&required, [predicate.clone()]);
            Ok(LogicalPlan::filter(prune(*input, child)?, predicate))
        }
        LogicalPlan::Projection { input, exprs, schema } => {
            let exprs: Vec<Expr> = match &required {
                None => exprs,
                Some(req) => exprs
                    .into_iter()
                    .zip(&schema.fields)
                    .filter(|(_, f)| req.contains(&ColumnRef::new(f.qualifier.as_deref(), &f.name)))
                    .map(|(e, _)| e)
                    .collect(),
            };
            let child = add_columns(&Some(BTreeSet::new()), exprs.clone());
            LogicalPlan::projection(prune(*input, child)?, exprs)
        }
        LogicalPlan::Aggregate {
            input,
            group_exprs,
            aggr_exprs,
            schema,
        } => {
            let ng = group_exprs.len();
            let aggr_exprs: Vec<Expr> = match &required {
                None => aggr_exprs,
                Some(req) => aggr_exprs
                    .into_iter()
                    .zip(&schema.fields[ng..])
                    .filter(|(_, f)| req.contains(&ColumnRef::new(f.qualifier.as_deref(), &f.name)))
                    .map(|(e, _)| e)
                    .collect(),
            };
            let child = add_columns(&Some(BTreeSet::new()), group_exprs.iter().chain(&aggr_exprs).cloned());
            LogicalPlan::aggregate(prune(*input, child)?, group_exprs, aggr_exprs)
        }
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            filter,
            ..
        } => {
            let extra = on
                .iter()
                .flat_map(|(l, r)| [l.clone(), r.clone()])
                .chain(filter.clone());
            let need = add_columns(&required, extra);
            let split = |side: &LogicalPlan| -> Required {
                let need = need.as_ref()?;
                let s = side.schema();
                Some(
                    need.iter()
                        .filter(|c| matches!(s.index_of(c.relation.as_deref(), &c.name), Ok(Some(_))))
                        .cloned()
                        .collect(),
                )
            };
            let (lreq, rreq) = (split(&left), split(&right));
            Ok(LogicalPlan::join(
                prune(*left, lreq)?,
                prune(*right, rreq)?,
                join_type,
                on,
                filter,
            ))
        }
        LogicalPlan::Sort { input, exprs, fetch } => {
            let child = add_columns(&required, exprs.iter().map(|s| s.expr.clone()));
            Ok(LogicalPlan::Sort {
                input: Box::new(prune(*input, child)?),
                exprs,
                fetch,
            })
        }
        LogicalPlan::Limit { input, skip, fetch } => Ok(LogicalPlan::Limit {
            input: Box::new(prune(*input, required)?),
            skip,
            fetch,
        }),
        LogicalPlan::SubqueryAlias { input, alias, schema } => {
            let inner = input.schema();
            let child = required.map(|req| {
                req.iter()
                    .filter_map(|c| {
                        let i = schema.index_of(c.relation.as_deref(), &c.name).ok().flatten()?;
                        let f = inner.field(i);
                        Some(ColumnRef::new(f.qualifier.as_deref(), &f.name))
                    })
                    .collect()
            });
            Ok(LogicalPlan::subquery_alias(prune(*input, child)?, &alias))
        }
        v @ LogicalPlan::Values { .. } => Ok(v),
    }
}

impl OptimizerRule for ProjectionPushdown {
    fn name(&self) -> &'static str {
        "projection_pushdown"
    }
    fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        prune(plan, None)
    }
}
