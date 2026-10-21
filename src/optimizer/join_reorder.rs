//! Greedy join reordering for chains of inner joins.
//!
//! A maximal tree of inner joins is flattened into a set of relations and a
//! set of join conditions. We then build a left-deep tree: start from the
//! relation with the smallest estimated cardinality and repeatedly join the
//! smallest relation that is *connected* to the current set by an equi-join
//! predicate (avoiding cross products whenever possible). The physical planner
//! later builds the hash table on the smaller side of each join.

use super::cardinality::estimate_rows;
use super::predicate_pushdown::refs_subset_of;
use super::{OptimizerRule, map_children};
use crate::ast::BinaryOperator;
use crate::column::Schema;
use crate::error::Result;
use crate::logical::binder::split_equi;
use crate::logical::expr::{binary, conjunction, split_conjunction};
use crate::logical::{Expr, JoinType, LogicalPlan};

pub struct JoinReorder;

fn flatten(plan: LogicalPlan, rels: &mut Vec<LogicalPlan>, conds: &mut Vec<Expr>) {
    match plan {
        LogicalPlan::Join {
            left,
            right,
            join_type: JoinType::Inner,
            on,
            filter,
            ..
        } => {
            flatten(*left, rels, conds);
            flatten(*right, rels, conds);
            conds.extend(on.into_iter().map(|(l, r)| binary(l, BinaryOperator::Eq, r)));
            conds.extend(filter.map(split_conjunction).unwrap_or_default());
        }
        other => rels.push(other),
    }
}

fn reorder(plan: LogicalPlan) -> Result<LogicalPlan> {
    if !matches!(
        plan,
        LogicalPlan::Join {
            join_type: JoinType::Inner,
            ..
        }
    ) {
        return map_children(plan, &mut reorder);
    }
    let original_columns = plan.output_columns();
    let mut rels = vec![];
    let mut conds = vec![];
    flatten(plan.clone(), &mut rels, &mut conds);
    if rels.len() < 3 {
        return map_children(plan, &mut reorder);
    }
    let rels: Vec<LogicalPlan> = rels.into_iter().map(reorder).collect::<Result<_>>()?;
    let est: Vec<f64> = rels.iter().map(estimate_rows).collect();
    let schemas: Vec<std::sync::Arc<Schema>> = rels.iter().map(|r| r.schema()).collect();

    let mut used = vec![false; rels.len()];
    let mut cond_used = vec![false; conds.len()];
    let start = (0..rels.len()).min_by(|&a, &b| est[a].total_cmp(&est[b])).unwrap();
    used[start] = true;
    let mut rel_slots: Vec<Option<LogicalPlan>> = rels.into_iter().map(Some).collect();
    let mut current = rel_slots[start].take().unwrap();
    let mut cur_est = est[start];

    for _ in 1..rel_slots.len() {
        let cur_schema = current.schema();
        // Candidates connected to the current set by an unused equi predicate.
        let connected = |r: usize| {
            conds
                .iter()
                .enumerate()
                .any(|(ci, c)| !cond_used[ci] && split_equi(c, &cur_schema, &schemas[r]).is_some())
        };
        let candidates: Vec<usize> = (0..used.len()).filter(|&r| !used[r] && connected(r)).collect();
        let pool: Vec<usize> = if candidates.is_empty() {
            (0..used.len()).filter(|&r| !used[r]).collect()
        } else {
            candidates
        };
        let next = *pool.iter().min_by(|&&a, &&b| est[a].total_cmp(&est[b])).unwrap();
        used[next] = true;
        let right = rel_slots[next].take().unwrap();
        let combined = cur_schema.join(&schemas[next]);
        let mut on = vec![];
        let mut residual = vec![];
        for (ci, c) in conds.iter().enumerate() {
            if cond_used[ci] || !refs_subset_of(c, &combined) {
                continue;
            }
            cond_used[ci] = true;
            match split_equi(c, &cur_schema, &schemas[next]) {
                Some(pair) => on.push(pair),
                None => residual.push(c.clone()),
            }
        }
        cur_est = if on.is_empty() {
            cur_est * est[next]
        } else {
            cur_est.max(est[next])
        };
        current = LogicalPlan::join(current, right, JoinType::Inner, on, conjunction(residual));
    }
    let _ = cur_est;
    let leftover: Vec<Expr> = conds
        .into_iter()
        .zip(cond_used)
        .filter(|(_, u)| !u)
        .map(|(c, _)| c)
        .collect();
    if let Some(p) = conjunction(leftover) {
        current = LogicalPlan::filter(current, p);
    }
    if current.output_columns() != original_columns {
        current = LogicalPlan::projection(current, original_columns.into_iter().map(Expr::Column).collect())?;
    }
    Ok(current)
}

impl OptimizerRule for JoinReorder {
    fn name(&self) -> &'static str {
        "join_reorder"
    }
    fn optimize(&self, plan: LogicalPlan) -> Result<LogicalPlan> {
        reorder(plan)
    }
}
