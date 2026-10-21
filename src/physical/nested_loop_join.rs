//! Nested-loop join for cross joins and non-equi conditions.

use super::expr::PhysicalExpr;
use super::{BatchStream, ExecutionPlan, Metrics, collect_partitions};
use crate::column::{Batch, NULL_INDEX, SchemaRef};
use crate::compute::to_mask;
use crate::error::{Error, Result};
use crate::logical::JoinType;
use crate::types::DataType;
use std::sync::{Arc, OnceLock};

/// Upper bound on candidate pairs materialized at once.
const MAX_PAIRS: usize = 1 << 16;

#[derive(Debug)]
pub struct NestedLoopJoinExec {
    pub left: Arc<dyn ExecutionPlan>,
    pub right: Arc<dyn ExecutionPlan>,
    pub join_type: JoinType,
    pub filter: Option<PhysicalExpr>,
    pub schema: SchemaRef,
    right_data: Arc<OnceLock<std::result::Result<Arc<Batch>, Error>>>,
    metrics: Arc<Metrics>,
}

impl NestedLoopJoinExec {
    pub fn new(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        join_type: JoinType,
        filter: Option<PhysicalExpr>,
        schema: SchemaRef,
    ) -> Self {
        NestedLoopJoinExec {
            left,
            right,
            join_type,
            filter,
            schema,
            right_data: Arc::new(OnceLock::new()),
            metrics: Metrics::new(),
        }
    }
}

fn join_batch(left: &Batch, right: &Batch, filter: Option<&PhysicalExpr>, join_type: JoinType) -> Result<Vec<Batch>> {
    let rn = right.num_rows;
    let mut out = vec![];
    let chunk = (MAX_PAIRS / rn.max(1)).max(1);
    let mut start = 0;
    while start < left.num_rows {
        let end = (start + chunk).min(left.num_rows);
        let mut li: Vec<u32> = Vec::with_capacity((end - start) * rn);
        let mut ri: Vec<u32> = Vec::with_capacity((end - start) * rn);
        for l in start..end {
            for r in 0..rn {
                li.push(l as u32);
                ri.push(r as u32);
            }
        }
        let assemble = |li: &[u32], ri: &[u32]| {
            let mut cols: Vec<_> = left.columns.iter().map(|c| Arc::new(c.take(li))).collect();
            cols.extend(right.columns.iter().map(|c| Arc::new(c.take_nullable(ri))));
            Batch::new(cols, li.len())
        };
        if let Some(f) = filter
            && !li.is_empty()
        {
            let joined = assemble(&li, &ri);
            let cond = f.evaluate_to_column(&joined, DataType::Boolean)?;
            let mask = to_mask(&cond);
            let keep: Vec<usize> = (0..li.len()).filter(|&k| mask[k]).collect();
            li = keep.iter().map(|&k| li[k]).collect();
            ri = keep.iter().map(|&k| ri[k]).collect();
        }
        if join_type == JoinType::Left {
            let mut matched = vec![false; end - start];
            for &l in &li {
                matched[l as usize - start] = true;
            }
            for (k, m) in matched.iter().enumerate() {
                if !m {
                    li.push((start + k) as u32);
                    ri.push(NULL_INDEX);
                }
            }
        }
        if !li.is_empty() {
            out.push(assemble(&li, &ri));
        }
        start = end;
    }
    Ok(out)
}

impl ExecutionPlan for NestedLoopJoinExec {
    fn name(&self) -> &'static str {
        "NestedLoopJoinExec"
    }
    fn details(&self) -> String {
        match &self.filter {
            Some(f) => format!("{:?}, filter={f}", self.join_type),
            None => format!("{:?} (cross product)", self.join_type),
        }
    }
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.left.clone(), self.right.clone()]
    }
    fn output_partitions(&self) -> usize {
        self.left.output_partitions()
    }
    fn execute(&self, partition: usize) -> Result<BatchStream> {
        let right = self
            .right_data
            .get_or_init(|| {
                let batches = collect_partitions(&self.right)?;
                let schema = self.right.schema();
                Ok(Arc::new(if batches.is_empty() {
                    Batch::new_empty(&schema)
                } else {
                    Batch::concat(&batches, schema.len())
                }))
            })
            .clone()?;
        let mut input = self.left.execute(partition)?;
        let filter = self.filter.clone();
        let join_type = self.join_type;
        let metrics = self.metrics.clone();
        let mut pending: std::collections::VecDeque<Batch> = Default::default();
        Ok(Box::new(std::iter::from_fn(move || {
            loop {
                if let Some(b) = pending.pop_front() {
                    metrics.record_batch(&b);
                    return Some(Ok(b));
                }
                let batch = match input.next()? {
                    Ok(b) => b,
                    Err(e) => return Some(Err(e)),
                };
                let _t = metrics.timer();
                match join_batch(&batch, &right, filter.as_ref(), join_type) {
                    Ok(v) => pending.extend(v),
                    Err(e) => return Some(Err(e)),
                }
            }
        })))
    }
    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}
