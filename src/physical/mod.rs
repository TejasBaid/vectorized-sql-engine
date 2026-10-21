//! Physical operators and the vectorized, partitioned execution model.
//!
//! ## Execution model
//!
//! Operators form a tree of [`ExecutionPlan`] nodes. Execution is
//! **pull-based** (Volcano style, but batch-at-a-time): `execute(partition)`
//! returns an iterator of [`Batch`]es and each operator pulls from its
//! children. Pull-based iteration composes naturally with `LIMIT` (early
//! termination is just "stop pulling") and keeps operators independent.
//!
//! Parallelism is **partitioned / morsel-driven**: a scan exposes `N`
//! partitions that dynamically claim batches (morsels) from a shared atomic
//! cursor, so fast threads take more work. Streaming operators (filter,
//! projection, hash-join probe) preserve partitioning, which means a
//! `scan -> filter -> project -> probe` pipeline runs on `N` threads without
//! any exchange. Pipeline breakers (hash aggregate, sort, hash-join build)
//! drain all input partitions in parallel and merge the results.

pub mod aggregate;
pub mod coalesce;
pub mod expr;
pub mod filter;
pub mod hash_join;
pub mod limit;
pub mod nested_loop_join;
pub mod planner;
pub mod projection;
pub mod scan;
pub mod sort;
pub mod values;

use crate::column::{Batch, SchemaRef};
use crate::error::Result;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub type BatchStream = Box<dyn Iterator<Item = Result<Batch>> + Send>;

pub trait ExecutionPlan: Send + Sync + fmt::Debug {
    /// Operator name, e.g. `HashJoinExec`.
    fn name(&self) -> &'static str;
    /// One-line description of the operator's parameters for EXPLAIN.
    fn details(&self) -> String;
    fn schema(&self) -> SchemaRef;
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>>;
    fn output_partitions(&self) -> usize {
        1
    }
    fn execute(&self, partition: usize) -> Result<BatchStream>;
    fn metrics(&self) -> &Arc<Metrics>;
}

/// Per-operator runtime statistics (shared by all partitions).
#[derive(Debug, Default)]
pub struct Metrics {
    pub output_rows: AtomicU64,
    pub output_batches: AtomicU64,
    /// CPU time spent in this operator's own work, summed over threads
    /// (excludes time spent waiting on children).
    pub elapsed_ns: AtomicU64,
}

impl Metrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Metrics::default())
    }

    pub fn record_batch(&self, b: &Batch) {
        self.output_rows.fetch_add(b.num_rows as u64, Ordering::Relaxed);
        self.output_batches.fetch_add(1, Ordering::Relaxed);
    }

    pub fn timer(&self) -> Timer<'_> {
        Timer {
            metrics: self,
            start: Instant::now(),
        }
    }

    pub fn rows(&self) -> u64 {
        self.output_rows.load(Ordering::Relaxed)
    }

    pub fn elapsed_ms(&self) -> f64 {
        self.elapsed_ns.load(Ordering::Relaxed) as f64 / 1e6
    }
}

/// Adds the elapsed time to the owning metrics on drop.
pub struct Timer<'a> {
    metrics: &'a Metrics,
    start: Instant,
}

impl Drop for Timer<'_> {
    fn drop(&mut self) {
        self.metrics
            .elapsed_ns
            .fetch_add(self.start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
}

/// Drain every partition of `plan` concurrently (one thread per partition)
/// and return all batches (partition order, not row order).
pub fn collect_partitions(plan: &Arc<dyn ExecutionPlan>) -> Result<Vec<Batch>> {
    let n = plan.output_partitions();
    if n == 1 {
        return plan.execute(0)?.collect();
    }
    let streams = (0..n).map(|p| plan.execute(p)).collect::<Result<Vec<_>>>()?;
    let results: Vec<Result<Vec<Batch>>> = std::thread::scope(|s| {
        let handles: Vec<_> = streams
            .into_iter()
            .map(|st| s.spawn(move || st.collect::<Result<Vec<_>>>()))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker thread panicked"))
            .collect()
    });
    let mut out = vec![];
    for r in results {
        out.extend(r?);
    }
    Ok(out)
}

/// Execute a plan to completion, returning all output batches.
pub fn collect(plan: &Arc<dyn ExecutionPlan>) -> Result<Vec<Batch>> {
    collect_partitions(plan)
}

/// Render the operator tree, optionally with runtime metrics (EXPLAIN ANALYZE).
pub fn display_plan(plan: &Arc<dyn ExecutionPlan>, with_metrics: bool) -> String {
    let mut out = String::new();
    fn walk(p: &Arc<dyn ExecutionPlan>, depth: usize, with_metrics: bool, out: &mut String) {
        out.push_str(&"  ".repeat(depth));
        out.push_str(p.name());
        let d = p.details();
        if !d.is_empty() {
            out.push_str(": ");
            out.push_str(&d);
        }
        if p.output_partitions() > 1 {
            out.push_str(&format!(" partitions={}", p.output_partitions()));
        }
        if with_metrics {
            let m = p.metrics();
            out.push_str(&format!(
                "  [rows={}, batches={}, cpu={:.2}ms]",
                m.rows(),
                m.output_batches.load(Ordering::Relaxed),
                m.elapsed_ms()
            ));
        }
        out.push('\n');
        for c in p.children() {
            walk(&c, depth + 1, with_metrics, out);
        }
    }
    walk(plan, 0, with_metrics, &mut out);
    out
}

/// Helper so operators can write `iter.map(...)` style streams that record metrics.
pub(crate) struct MeteredStream {
    pub inner: BatchStream,
    pub metrics: Arc<Metrics>,
}

impl Iterator for MeteredStream {
    type Item = Result<Batch>;
    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next();
        if let Some(Ok(b)) = &item {
            self.metrics.record_batch(b);
        }
        item
    }
}

pub(crate) fn exprs_to_string<T: fmt::Display>(v: &[T]) -> String {
    v.iter().map(|e| e.to_string()).collect::<Vec<_>>().join(", ")
}
