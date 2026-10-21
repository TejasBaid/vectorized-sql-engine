//! Filter: evaluates a predicate vectorized and compacts the batch.

use super::expr::PhysicalExpr;
use super::{BatchStream, ExecutionPlan, Metrics};
use crate::column::{Batch, SchemaRef};
use crate::compute::{ColumnarValue, to_mask};
use crate::error::Result;
use crate::types::ScalarValue;
use std::sync::Arc;

#[derive(Debug)]
pub struct FilterExec {
    pub input: Arc<dyn ExecutionPlan>,
    pub predicate: Arc<PhysicalExpr>,
    metrics: Arc<Metrics>,
}

impl FilterExec {
    pub fn new(input: Arc<dyn ExecutionPlan>, predicate: PhysicalExpr) -> Self {
        FilterExec {
            input,
            predicate: Arc::new(predicate),
            metrics: Metrics::new(),
        }
    }
}

/// Apply a predicate to a batch; `None` if no rows survive.
pub fn filter_batch(batch: &Batch, predicate: &PhysicalExpr) -> Result<Option<Batch>> {
    match predicate.evaluate(batch)? {
        ColumnarValue::Scalar(ScalarValue::Boolean(true)) => Ok(Some(batch.clone())),
        ColumnarValue::Scalar(_) => Ok(None),
        ColumnarValue::Column(c) => {
            let mask = to_mask(&c);
            let idx: Vec<u32> = mask
                .iter()
                .enumerate()
                .filter_map(|(i, &m)| m.then_some(i as u32))
                .collect();
            if idx.is_empty() {
                Ok(None)
            } else if idx.len() == batch.num_rows {
                Ok(Some(batch.clone()))
            } else {
                Ok(Some(batch.take(&idx)))
            }
        }
    }
}

impl ExecutionPlan for FilterExec {
    fn name(&self) -> &'static str {
        "FilterExec"
    }
    fn details(&self) -> String {
        self.predicate.to_string()
    }
    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }
    fn output_partitions(&self) -> usize {
        self.input.output_partitions()
    }
    fn execute(&self, partition: usize) -> Result<BatchStream> {
        let mut input = self.input.execute(partition)?;
        let predicate = self.predicate.clone();
        let metrics = self.metrics.clone();
        Ok(Box::new(std::iter::from_fn(move || {
            loop {
                let batch = match input.next()? {
                    Ok(b) => b,
                    Err(e) => return Some(Err(e)),
                };
                let _t = metrics.timer();
                match filter_batch(&batch, &predicate) {
                    Ok(Some(b)) => {
                        metrics.record_batch(&b);
                        return Some(Ok(b));
                    }
                    Ok(None) => continue,
                    Err(e) => return Some(Err(e)),
                }
            }
        })))
    }
    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}
