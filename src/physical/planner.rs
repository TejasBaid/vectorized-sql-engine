//! Lowers an optimized [`LogicalPlan`] into an [`ExecutionPlan`] tree.

use super::ExecutionPlan;
use super::aggregate::{AggSpec, HashAggregateExec};
use super::coalesce::CoalesceExec;
use super::expr::create_physical_expr;
use super::filter::FilterExec;
use super::hash_join::HashJoinExec;
use super::limit::LimitExec;
use super::nested_loop_join::NestedLoopJoinExec;
use super::projection::ProjectionExec;
use super::scan::ScanExec;
use super::sort::{PhysicalSortExpr, SortExec};
use super::values::ValuesExec;
use crate::column::{Batch, ColumnBuilder};
use crate::err;
use crate::error::Result;
use crate::logical::expr::conjunction;
use crate::logical::{Expr, JoinType, LogicalPlan};
use crate::optimizer::cardinality::estimate_rows;
use std::sync::Arc;

pub struct PhysicalPlanner {
    pub target_partitions: usize,
}

impl PhysicalPlanner {
    pub fn new(target_partitions: usize) -> Self {
        PhysicalPlanner {
            target_partitions: target_partitions.max(1),
        }
    }

    pub fn create_physical_plan(&self, plan: &LogicalPlan) -> Result<Arc<dyn ExecutionPlan>> {
        let p = self.plan(plan)?;
        Ok(if p.output_partitions() > 1 {
            Arc::new(CoalesceExec::new(p))
        } else {
            p
        })
    }

    fn plan(&self, plan: &LogicalPlan) -> Result<Arc<dyn ExecutionPlan>> {
        Ok(match plan {
            LogicalPlan::TableScan(s) => {
                let projection = s
                    .projection
                    .clone()
                    .unwrap_or_else(|| (0..s.source.schema.len()).collect());
                let scan: Arc<dyn ExecutionPlan> = Arc::new(ScanExec::new(
                    s.table_name.clone(),
                    s.source.clone(),
                    projection,
                    s.schema.clone(),
                    self.target_partitions,
                ));
                match conjunction(s.filters.clone()) {
                    Some(pred) => Arc::new(FilterExec::new(scan, create_physical_expr(&pred, &s.schema)?)),
                    None => scan,
                }
            }
            LogicalPlan::Filter { input, predicate } => {
                let child = self.plan(input)?;
                Arc::new(FilterExec::new(
                    child,
                    create_physical_expr(predicate, &input.schema())?,
                ))
            }
            LogicalPlan::Projection { input, exprs, schema } => {
                let child = self.plan(input)?;
                let in_schema = input.schema();
                let exprs = exprs
                    .iter()
                    .map(|e| create_physical_expr(e, &in_schema))
                    .collect::<Result<_>>()?;
                Arc::new(ProjectionExec::new(child, exprs, schema.clone()))
            }
            LogicalPlan::Aggregate {
                input,
                group_exprs,
                aggr_exprs,
                schema,
            } => {
                let child = self.plan(input)?;
                let in_schema = input.schema();
                let groups = group_exprs
                    .iter()
                    .map(|e| create_physical_expr(e, &in_schema))
                    .collect::<Result<_>>()?;
                let group_types = group_exprs
                    .iter()
                    .map(|e| e.data_type(&in_schema))
                    .collect::<Result<_>>()?;
                let aggs = aggr_exprs
                    .iter()
                    .map(|e| {
                        let Expr::Aggregate(a) = e.unalias() else {
                            return err!(Plan, "expected aggregate expression, got {e}");
                        };
                        Ok(AggSpec {
                            func: a.func,
                            arg: a
                                .arg
                                .as_ref()
                                .map(|x| create_physical_expr(x, &in_schema))
                                .transpose()?,
                            distinct: a.distinct,
                            input_type: match &a.arg {
                                Some(x) => x.data_type(&in_schema)?,
                                None => crate::types::DataType::Int64,
                            },
                            output_type: e.data_type(&in_schema)?,
                        })
                    })
                    .collect::<Result<_>>()?;
                Arc::new(HashAggregateExec::new(
                    child,
                    groups,
                    group_types,
                    aggs,
                    schema.clone(),
                )?)
            }
            LogicalPlan::Join {
                left,
                right,
                join_type,
                on,
                filter,
                schema,
            } => {
                let (l, r) = (self.plan(left)?, self.plan(right)?);
                let filter = filter.as_ref().map(|f| create_physical_expr(f, schema)).transpose()?;
                if on.is_empty() {
                    return Ok(Arc::new(NestedLoopJoinExec::new(
                        l,
                        r,
                        *join_type,
                        filter,
                        schema.clone(),
                    )));
                }
                let (ls, rs) = (left.schema(), right.schema());
                let keys = on
                    .iter()
                    .map(|(a, b)| Ok((create_physical_expr(a, &ls)?, create_physical_expr(b, &rs)?)))
                    .collect::<Result<_>>()?;
                let key_types = on.iter().map(|(a, _)| a.data_type(&ls)).collect::<Result<_>>()?;
                // Build the hash table on the (estimated) smaller input.
                let build_left = *join_type == JoinType::Inner && estimate_rows(left) < estimate_rows(right);
                Arc::new(HashJoinExec::new(
                    l,
                    r,
                    *join_type,
                    keys,
                    key_types,
                    filter,
                    build_left,
                    schema.clone(),
                ))
            }
            LogicalPlan::Sort { input, exprs, fetch } => {
                let child = self.plan(input)?;
                let in_schema = input.schema();
                let exprs = exprs
                    .iter()
                    .map(|s| {
                        Ok(PhysicalSortExpr {
                            expr: create_physical_expr(&s.expr, &in_schema)?,
                            data_type: s.expr.data_type(&in_schema)?,
                            asc: s.asc,
                            nulls_first: s.nulls_first,
                        })
                    })
                    .collect::<Result<_>>()?;
                Arc::new(SortExec::new(child, exprs, *fetch))
            }
            LogicalPlan::Limit { input, skip, fetch } => {
                let mut child = self.plan(input)?;
                if child.output_partitions() > 1 {
                    child = Arc::new(CoalesceExec::new(child));
                }
                Arc::new(LimitExec::new(child, *skip, *fetch))
            }
            LogicalPlan::SubqueryAlias { input, .. } => self.plan(input)?,
            LogicalPlan::Values { rows, schema } => {
                if rows.is_empty() {
                    return Ok(Arc::new(ValuesExec::new(None, schema.clone())));
                }
                let empty = crate::column::Schema::empty();
                let one = Batch::new(vec![], 1);
                let mut builders: Vec<ColumnBuilder> =
                    schema.fields.iter().map(|f| ColumnBuilder::new(f.data_type)).collect();
                for row in rows {
                    for (b, e) in builders.iter_mut().zip(row) {
                        let v = create_physical_expr(e, &empty)?.evaluate(&one)?;
                        let v = match v {
                            crate::compute::ColumnarValue::Scalar(s) => s,
                            crate::compute::ColumnarValue::Column(c) => c.value(0),
                        };
                        b.push(&v);
                    }
                }
                let cols: Vec<_> = builders.into_iter().map(|b| Arc::new(b.finish())).collect();
                Arc::new(ValuesExec::new(Some(Batch::new(cols, rows.len())), schema.clone()))
            }
        })
    }
}
