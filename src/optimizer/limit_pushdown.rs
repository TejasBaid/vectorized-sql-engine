//! Push `LIMIT` into an underlying `ORDER BY` as a top-k fetch.

use super::{OptimizerRule, map_children};
use crate::error::Result;
use crate::logical::LogicalPlan;

pub struct LimitPushdown;

fn with_fetch(plan: LogicalPlan, k: usize) -> Result<LogicalPlan> {
    Ok(match plan {
        LogicalPlan::Sort { input, exprs, fetch } => LogicalPlan::Sort {
            input,
            exprs,
            fetch: Some(fetch.map_or(k, |f| f.min(k))),
        },
        LogicalPlan::Projection { input, exprs, .. } => LogicalPlan::projection(with_fetch(*input, k)?, exprs)?,
        other => other,
    })
}

fn rewrite(plan: LogicalPlan) -> Result<LogicalPlan> {
    let plan = map_children(plan, &mut rewrite)?;
    Ok(match plan {
        LogicalPlan::Limit {
            input,
            skip,
            fetch: Some(f),
        } => LogicalPlan::Limit {
            input: Box::new(with_fetch(*input, skip + f)?),
            skip,
            fetch: Some(f),
        },
        other => other,
    })
}

impl OptimizerRule for LimitPushdown {
    fn name(&self) -> &'static str {
        "limit_pushdown"
    }
    fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        rewrite(plan)
    }
}
