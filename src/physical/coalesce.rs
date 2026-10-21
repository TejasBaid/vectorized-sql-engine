//! Merges N input partitions into one stream using worker threads and a
//! bounded channel (back-pressure). Dropping the receiver (e.g. LIMIT reached)
//! makes the workers stop at their next send.

use super::{BatchStream, ExecutionPlan, Metrics};
use crate::column::SchemaRef;
use crate::error::Result;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;

#[derive(Debug)]
pub struct CoalesceExec {
    pub input: Arc<dyn ExecutionPlan>,
    metrics: Arc<Metrics>,
}

impl CoalesceExec {
    pub fn new(input: Arc<dyn ExecutionPlan>) -> Self {
        CoalesceExec {
            input,
            metrics: Metrics::new(),
        }
    }
}

impl ExecutionPlan for CoalesceExec {
    fn name(&self) -> &'static str {
        "CoalesceExec"
    }
    fn details(&self) -> String {
        String::new()
    }
    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }
    fn execute(&self, _partition: usize) -> Result<BatchStream> {
        let n = self.input.output_partitions();
        if n == 1 {
            let metrics = self.metrics.clone();
            return Ok(Box::new(super::MeteredStream {
                inner: self.input.execute(0)?,
                metrics,
            }));
        }
        let (tx, rx) = sync_channel(n * 2);
        for p in 0..n {
            let stream = self.input.execute(p)?;
            let tx = tx.clone();
            std::thread::spawn(move || {
                for item in stream {
                    let is_err = item.is_err();
                    if tx.send(item).is_err() || is_err {
                        break;
                    }
                }
            });
        }
        drop(tx);
        Ok(Box::new(super::MeteredStream {
            inner: Box::new(rx.into_iter()),
            metrics: self.metrics.clone(),
        }))
    }
    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}
