//! Literal rows (`SELECT 1`, empty relations).

use super::{BatchStream, ExecutionPlan, Metrics};
use crate::column::{Batch, SchemaRef};
use crate::error::Result;
use std::sync::Arc;

#[derive(Debug)]
pub struct ValuesExec {
    pub batch: Option<Batch>,
    pub schema: SchemaRef,
    metrics: Arc<Metrics>,
}

impl ValuesExec {
    pub fn new(batch: Option<Batch>, schema: SchemaRef) -> Self {
        ValuesExec {
            batch,
            schema,
            metrics: Metrics::new(),
        }
    }
}

impl ExecutionPlan for ValuesExec {
    fn name(&self) -> &'static str {
        if self.batch.is_some() {
            "ValuesExec"
        } else {
            "EmptyExec"
        }
    }
    fn details(&self) -> String {
        self.batch
            .as_ref()
            .map(|b| format!("rows={}", b.num_rows))
            .unwrap_or_default()
    }
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![]
    }
    fn execute(&self, _partition: usize) -> Result<BatchStream> {
        if let Some(b) = &self.batch {
            self.metrics.record_batch(b);
        }
        Ok(Box::new(self.batch.clone().into_iter().map(Ok)))
    }
    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}
