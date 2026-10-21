//! Partitioned hash join (INNER and LEFT OUTER).
//!
//! The build side is drained once (all partitions in parallel), concatenated,
//! and indexed in a hash table keyed by the 64-bit key hash with chained row
//! lists (`head[hash] -> row -> next[row] -> ...`). Probe partitions then run
//! concurrently against the shared, immutable table, so the join preserves
//! the probe side's partitioning and parallelism.

use super::expr::PhysicalExpr;
use super::{BatchStream, ExecutionPlan, Metrics, collect_partitions};
use crate::column::{Batch, Column, ColumnData, ColumnRef, NULL_INDEX, SchemaRef};
use crate::compute::hash::{IdentityBuildHasher, hash_column};
use crate::compute::to_mask;
use crate::error::{Error, Result};
use crate::logical::JoinType;
use crate::types::DataType;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

const NONE: u32 = u32::MAX;

struct BuildSide {
    batch: Batch,
    keys: Vec<ColumnRef>,
    head: HashMap<u64, u32, IdentityBuildHasher>,
    next: Vec<u32>,
}

#[derive(Debug)]
pub struct HashJoinExec {
    pub left: Arc<dyn ExecutionPlan>,
    pub right: Arc<dyn ExecutionPlan>,
    pub join_type: JoinType,
    /// `(left key, right key)` resolved against the left / right schemas.
    pub on: Vec<(PhysicalExpr, PhysicalExpr)>,
    pub key_types: Vec<DataType>,
    /// Residual predicate over the joined (left ++ right) row.
    pub filter: Option<PhysicalExpr>,
    /// If true the left input is hashed and the right input probes.
    pub build_left: bool,
    pub schema: SchemaRef,
    shared: Arc<Shared>,
    metrics: Arc<Metrics>,
}

struct Shared {
    build: OnceLock<std::result::Result<Arc<BuildSide>, Error>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Shared")
    }
}

impl HashJoinExec {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        left: Arc<dyn ExecutionPlan>,
        right: Arc<dyn ExecutionPlan>,
        join_type: JoinType,
        on: Vec<(PhysicalExpr, PhysicalExpr)>,
        key_types: Vec<DataType>,
        filter: Option<PhysicalExpr>,
        build_left: bool,
        schema: SchemaRef,
    ) -> Self {
        assert!(
            !(build_left && join_type == JoinType::Left),
            "LEFT JOIN must build the right side"
        );
        HashJoinExec {
            left,
            right,
            join_type,
            on,
            key_types,
            filter,
            build_left,
            schema,
            shared: Arc::new(Shared { build: OnceLock::new() }),
            metrics: Metrics::new(),
        }
    }

    fn build_plan(&self) -> &Arc<dyn ExecutionPlan> {
        if self.build_left { &self.left } else { &self.right }
    }

    fn probe_plan(&self) -> &Arc<dyn ExecutionPlan> {
        if self.build_left { &self.right } else { &self.left }
    }

    fn build_keys(&self) -> Vec<PhysicalExpr> {
        self.on
            .iter()
            .map(|(l, r)| if self.build_left { l.clone() } else { r.clone() })
            .collect()
    }

    fn probe_keys(&self) -> Vec<PhysicalExpr> {
        self.on
            .iter()
            .map(|(l, r)| if self.build_left { r.clone() } else { l.clone() })
            .collect()
    }

    fn build(&self) -> Result<Arc<BuildSide>> {
        let batches = collect_partitions(self.build_plan())?;
        let _t = self.metrics.timer();
        let schema = self.build_plan().schema();
        let batch = if batches.is_empty() {
            Batch::new_empty(&schema)
        } else {
            Batch::concat(&batches, schema.len())
        };
        let keys = self
            .build_keys()
            .iter()
            .zip(&self.key_types)
            .map(|(k, t)| k.evaluate_to_column(&batch, *t))
            .collect::<Result<Vec<_>>>()?;
        let n = batch.num_rows;
        let mut hashes = vec![0u64; n];
        for k in &keys {
            hash_column(k, &mut hashes);
        }
        let mut head: HashMap<u64, u32, IdentityBuildHasher> = HashMap::with_capacity_and_hasher(n, Default::default());
        let mut next = vec![NONE; n];
        for (i, &h) in hashes.iter().enumerate() {
            if keys.iter().any(|k| !k.is_valid(i)) {
                continue; // NULL keys never match
            }
            let slot = head.entry(h).or_insert(NONE);
            next[i] = *slot;
            *slot = i as u32;
        }
        Ok(Arc::new(BuildSide {
            batch,
            keys,
            head,
            next,
        }))
    }
}

/// Equality of build row `i` and probe row `j` across all key columns.
#[inline]
fn keys_equal(build: &[ColumnRef], i: usize, probe: &[ColumnRef], j: usize) -> bool {
    build.iter().zip(probe).all(|(b, p)| match (&b.data, &p.data) {
        (ColumnData::Int64(x), ColumnData::Int64(y)) => x[i] == y[j],
        (ColumnData::Date(x), ColumnData::Date(y)) => x[i] == y[j],
        (ColumnData::Float64(x), ColumnData::Float64(y)) => x[i] == y[j],
        (ColumnData::Boolean(x), ColumnData::Boolean(y)) => x[i] == y[j],
        (ColumnData::Utf8(x), ColumnData::Utf8(y)) => x.get_bytes(i) == y.get_bytes(j),
        _ => false,
    })
}

struct ProbeContext {
    build: Arc<BuildSide>,
    probe_keys: Vec<PhysicalExpr>,
    key_types: Vec<DataType>,
    filter: Option<PhysicalExpr>,
    join_type: JoinType,
    build_left: bool,
}

impl ProbeContext {
    fn assemble(&self, probe: &Batch, probe_idx: &[u32], build_idx: &[u32]) -> Batch {
        let p_cols = probe.columns.iter().map(|c| Arc::new(c.take(probe_idx)));
        let b_cols = self
            .build
            .batch
            .columns
            .iter()
            .map(|c| Arc::new(c.take_nullable(build_idx)));
        let columns: Vec<Arc<Column>> = if self.build_left {
            b_cols.chain(p_cols).collect()
        } else {
            p_cols.chain(b_cols).collect()
        };
        Batch::new(columns, probe_idx.len())
    }

    fn probe(&self, batch: &Batch) -> Result<Option<Batch>> {
        let keys = self
            .probe_keys
            .iter()
            .zip(&self.key_types)
            .map(|(k, t)| k.evaluate_to_column(batch, *t))
            .collect::<Result<Vec<_>>>()?;
        let n = batch.num_rows;
        let mut hashes = vec![0u64; n];
        for k in &keys {
            hash_column(k, &mut hashes);
        }
        let mut probe_idx: Vec<u32> = Vec::with_capacity(n);
        let mut build_idx: Vec<u32> = Vec::with_capacity(n);
        let any_null = keys.iter().any(|k| k.validity.is_some());
        // Fast path for the overwhelmingly common single BIGINT key.
        let single_int = match (keys.as_slice(), self.build.keys.as_slice()) {
            ([p], [b]) => match (&p.data, &b.data) {
                (ColumnData::Int64(pv), ColumnData::Int64(bv)) => Some((pv, bv)),
                _ => None,
            },
            _ => None,
        };
        for (j, h) in hashes.iter().enumerate() {
            if any_null && keys.iter().any(|k| !k.is_valid(j)) {
                continue;
            }
            let Some(&first) = self.build.head.get(h) else { continue };
            let mut i = first;
            while i != NONE {
                let eq = match single_int {
                    Some((pv, bv)) => pv[j] == bv[i as usize],
                    None => keys_equal(&self.build.keys, i as usize, &keys, j),
                };
                if eq {
                    probe_idx.push(j as u32);
                    build_idx.push(i);
                }
                i = self.build.next[i as usize];
            }
        }
        // Residual filter on candidate pairs.
        if let Some(f) = &self.filter
            && !probe_idx.is_empty()
        {
            let joined = self.assemble(batch, &probe_idx, &build_idx);
            let cond = f.evaluate_to_column(&joined, DataType::Boolean)?;
            let mask = to_mask(&cond);
            let mut k = 0;
            probe_idx.retain(|_| {
                k += 1;
                mask[k - 1]
            });
            let mut k = 0;
            build_idx.retain(|_| {
                k += 1;
                mask[k - 1]
            });
        }
        if self.join_type == JoinType::Left {
            let mut matched = vec![false; n];
            for &j in &probe_idx {
                matched[j as usize] = true;
            }
            for (j, m) in matched.iter().enumerate() {
                if !m {
                    probe_idx.push(j as u32);
                    build_idx.push(NULL_INDEX);
                }
            }
        }
        if probe_idx.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.assemble(batch, &probe_idx, &build_idx)))
    }
}

impl ExecutionPlan for HashJoinExec {
    fn name(&self) -> &'static str {
        "HashJoinExec"
    }
    fn details(&self) -> String {
        let keys: Vec<String> = self.on.iter().map(|(l, r)| format!("{l} = {r}")).collect();
        let mut s = format!(
            "{:?}, on=[{}], build={}",
            self.join_type,
            keys.join(", "),
            if self.build_left { "left" } else { "right" }
        );
        if let Some(f) = &self.filter {
            s.push_str(&format!(", filter={f}"));
        }
        s
    }
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.left.clone(), self.right.clone()]
    }
    fn output_partitions(&self) -> usize {
        self.probe_plan().output_partitions()
    }
    fn execute(&self, partition: usize) -> Result<BatchStream> {
        // The first partition to arrive builds; the others block until it is ready.
        let build = self.shared.build.get_or_init(|| self.build()).clone()?;
        let ctx = ProbeContext {
            build,
            probe_keys: self.probe_keys(),
            key_types: self.key_types.clone(),
            filter: self.filter.clone(),
            join_type: self.join_type,
            build_left: self.build_left,
        };
        let mut input = self.probe_plan().execute(partition)?;
        let metrics = self.metrics.clone();
        Ok(Box::new(std::iter::from_fn(move || {
            loop {
                let batch = match input.next()? {
                    Ok(b) => b,
                    Err(e) => return Some(Err(e)),
                };
                let _t = metrics.timer();
                match ctx.probe(&batch) {
                    Ok(Some(out)) => {
                        metrics.record_batch(&out);
                        return Some(Ok(out));
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
