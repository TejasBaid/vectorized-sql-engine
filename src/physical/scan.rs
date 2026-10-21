//! Table scan over an in-memory table with morsel-driven partitioning.

use super::{BatchStream, ExecutionPlan, Metrics};
use crate::catalog::MemTable;
use crate::column::SchemaRef;
use crate::error::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug)]
pub struct ScanExec {
    pub table_name: String,
    pub table: Arc<MemTable>,
    pub projection: Vec<usize>,
    pub schema: SchemaRef,
    pub partitions: usize,
    /// Shared morsel cursor: every partition claims the next unread batch.
    cursor: Arc<AtomicUsize>,
    metrics: Arc<Metrics>,
}

impl ScanExec {
    pub fn new(
        table_name: String,
        table: Arc<MemTable>,
        projection: Vec<usize>,
        schema: SchemaRef,
        partitions: usize,
    ) -> Self {
        let partitions = partitions.clamp(1, table.batches.len().max(1));
        ScanExec {
            table_name,
            table,
            projection,
            schema,
            partitions,
            cursor: Arc::new(AtomicUsize::new(0)),
            metrics: Metrics::new(),
        }
    }
}

impl ExecutionPlan for ScanExec {
    fn name(&self) -> &'static str {
        "ScanExec"
    }

    fn details(&self) -> String {
        let cols: Vec<&str> = self.schema.fields.iter().map(|f| f.name.as_str()).collect();
        format!("{} [{}]", self.table_name, cols.join(", "))
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![]
    }

    fn output_partitions(&self) -> usize {
        self.partitions
    }

    fn execute(&self, _partition: usize) -> Result<BatchStream> {
        let table = self.table.clone();
        let cursor = self.cursor.clone();
        let projection = self.projection.clone();
        let metrics = self.metrics.clone();
        Ok(Box::new(std::iter::from_fn(move || {
            let i = cursor.fetch_add(1, Ordering::Relaxed);
            let batch = table.batches.get(i)?;
            let _t = metrics.timer();
            let out = batch.project(&projection);
            metrics.record_batch(&out);
            Some(Ok(out))
        })))
    }

    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}
