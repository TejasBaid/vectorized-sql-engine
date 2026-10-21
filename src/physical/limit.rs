//! LIMIT / OFFSET with early termination.

use super::{BatchStream, ExecutionPlan, Metrics};
use crate::column::SchemaRef;
use crate::error::Result;
use std::sync::Arc;

#[derive(Debug)]
pub struct LimitExec {
    pub input: Arc<dyn ExecutionPlan>,
    pub skip: usize,
    pub fetch: Option<usize>,
    metrics: Arc<Metrics>,
}

impl LimitExec {
    pub fn new(input: Arc<dyn ExecutionPlan>, skip: usize, fetch: Option<usize>) -> Self {
        assert_eq!(
            input.output_partitions(),
            1,
            "LimitExec requires a single input partition"
        );
        LimitExec {
            input,
            skip,
            fetch,
            metrics: Metrics::new(),
        }
    }
}

impl ExecutionPlan for LimitExec {
    fn name(&self) -> &'static str {
        "LimitExec"
    }
    fn details(&self) -> String {
        match self.fetch {
            Some(f) => format!("skip={}, fetch={f}", self.skip),
            None => format!("skip={}", self.skip),
        }
    }
    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }
    fn execute(&self, _partition: usize) -> Result<BatchStream> {
        let mut input = self.input.execute(0)?;
        let mut to_skip = self.skip;
        let mut remaining = self.fetch.unwrap_or(usize::MAX);
        let metrics = self.metrics.clone();
        Ok(Box::new(std::iter::from_fn(move || {
            while remaining > 0 {
                let batch = match input.next()? {
                    Ok(b) => b,
                    Err(e) => return Some(Err(e)),
                };
                let _t = metrics.timer();
                let n = batch.num_rows;
                if to_skip >= n {
                    to_skip -= n;
                    continue;
                }
                let take = (n - to_skip).min(remaining);
                let out = if to_skip == 0 && take == n {
                    batch
                } else {
                    batch.slice(to_skip, take)
                };
                to_skip = 0;
                remaining -= take;
                metrics.record_batch(&out);
                return Some(Ok(out));
            }
            None // stop pulling: upstream work is abandoned
        })))
    }
    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}
