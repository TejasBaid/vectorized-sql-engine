//! Projection: evaluates expressions to produce new columns.

use super::expr::PhysicalExpr;
use super::{BatchStream, ExecutionPlan, Metrics, exprs_to_string};
use crate::column::{Batch, SchemaRef};
use crate::error::Result;
use std::sync::Arc;

#[derive(Debug)]
pub struct ProjectionExec {
    pub input: Arc<dyn ExecutionPlan>,
    pub exprs: Arc<Vec<PhysicalExpr>>,
    pub schema: SchemaRef,
    metrics: Arc<Metrics>,
}

impl ProjectionExec {
    pub fn new(input: Arc<dyn ExecutionPlan>, exprs: Vec<PhysicalExpr>, schema: SchemaRef) -> Self {
        ProjectionExec {
            input,
            exprs: Arc::new(exprs),
            schema,
            metrics: Metrics::new(),
        }
    }
}

impl ExecutionPlan for ProjectionExec {
    fn name(&self) -> &'static str {
        "ProjectionExec"
    }
    fn details(&self) -> String {
        exprs_to_string(&self.exprs)
    }
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }
    fn output_partitions(&self) -> usize {
        self.input.output_partitions()
    }
    fn execute(&self, partition: usize) -> Result<BatchStream> {
        let input = self.input.execute(partition)?;
        let exprs = self.exprs.clone();
        let schema = self.schema.clone();
        let metrics = self.metrics.clone();
        Ok(Box::new(input.map(move |b| {
            let b = b?;
            let _t = metrics.timer();
            let cols = exprs
                .iter()
                .zip(&schema.fields)
                .map(|(e, f)| e.evaluate_to_column(&b, f.data_type))
                .collect::<Result<Vec<_>>>()?;
            let out = Batch::new(cols, b.num_rows);
            metrics.record_batch(&out);
            Ok(out)
        })))
    }
    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}
