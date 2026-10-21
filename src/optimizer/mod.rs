//! Rule-based logical optimizer.
//!
//! Rules run in a fixed order, each one a pure `LogicalPlan -> LogicalPlan`
//! rewrite:
//!
//! 1. [`ConstantFolding`] – evaluate constant sub-expressions, simplify
//!    boolean logic, drop `WHERE true`, turn `WHERE false` into an empty relation.
//! 2. [`PredicatePushdown`] – move filters as close to the scans as possible,
//!    turn cross joins + equality predicates into equi-joins, convert LEFT
//!    joins with null-rejecting WHERE predicates into INNER joins.
//! 3. [`JoinReorder`] – greedy, cardinality-driven ordering of multi-way inner joins.
//! 4. [`LimitPushdown`] – turn `LIMIT k` over `ORDER BY` into a top-k sort.
//! 5. [`ProjectionPushdown`] – only read and carry the columns that are needed.

pub mod cardinality;
pub mod constant_folding;
pub mod join_reorder;
pub mod limit_pushdown;
pub mod predicate_pushdown;
pub mod projection_pushdown;

pub use constant_folding::ConstantFolding;
pub use join_reorder::JoinReorder;
pub use limit_pushdown::LimitPushdown;
pub use predicate_pushdown::PredicatePushdown;
pub use projection_pushdown::ProjectionPushdown;

use crate::error::Result;
use crate::logical::LogicalPlan;

pub trait OptimizerRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan>;
}

pub struct Optimizer {
    rules: Vec<Box<dyn OptimizerRule>>,
}

impl Default for Optimizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Optimizer {
    pub fn new() -> Self {
        Optimizer {
            rules: vec![
                Box::new(ConstantFolding),
                Box::new(PredicatePushdown),
                Box::new(JoinReorder),
                Box::new(LimitPushdown),
                Box::new(ProjectionPushdown),
            ],
        }
    }

    pub fn with_rules(rules: Vec<Box<dyn OptimizerRule>>) -> Self {
        Optimizer { rules }
    }

    pub fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        self.rules.iter().try_fold(plan, |p, r| r.optimize(p))
    }

    /// Optimize, recording the plan after every rule (for `EXPLAIN VERBOSE`).
    pub fn optimize_traced(&self, plan: LogicalPlan) -> Result<(LogicalPlan, Vec<(&'static str, String)>)> {
        let mut trace = vec![];
        let mut p = plan;
        for r in &self.rules {
            p = r.optimize(p)?;
            trace.push((r.name(), p.to_string()));
        }
        Ok((p, trace))
    }
}

/// Rebuild `plan` with each direct child transformed by `f`, recomputing
/// derived schemas.
pub fn map_children(plan: LogicalPlan, f: &mut dyn FnMut(LogicalPlan) -> Result<LogicalPlan>) -> Result<LogicalPlan> {
    Ok(match plan {
        p @ (LogicalPlan::TableScan(_) | LogicalPlan::Values { .. }) => p,
        LogicalPlan::Filter { input, predicate } => LogicalPlan::filter(f(*input)?, predicate),
        LogicalPlan::Projection { input, exprs, .. } => LogicalPlan::projection(f(*input)?, exprs)?,
        LogicalPlan::Aggregate {
            input,
            group_exprs,
            aggr_exprs,
            ..
        } => LogicalPlan::aggregate(f(*input)?, group_exprs, aggr_exprs)?,
        LogicalPlan::Join {
            left,
            right,
            join_type,
            on,
            filter,
            ..
        } => LogicalPlan::join(f(*left)?, f(*right)?, join_type, on, filter),
        LogicalPlan::Sort { input, exprs, fetch } => LogicalPlan::Sort {
            input: Box::new(f(*input)?),
            exprs,
            fetch,
        },
        LogicalPlan::Limit { input, skip, fetch } => LogicalPlan::Limit {
            input: Box::new(f(*input)?),
            skip,
            fetch,
        },
        LogicalPlan::SubqueryAlias { input, alias, .. } => LogicalPlan::subquery_alias(f(*input)?, &alias),
    })
}
