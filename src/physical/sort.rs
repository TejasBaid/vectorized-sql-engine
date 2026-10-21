//! Sort (with optional top-k `fetch`).

use super::expr::PhysicalExpr;
use super::{BatchStream, ExecutionPlan, Metrics, collect_partitions};
use crate::column::{BATCH_SIZE, Batch, Column, ColumnData, SchemaRef};
use crate::error::Result;
use crate::types::DataType;
use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct PhysicalSortExpr {
    pub expr: PhysicalExpr,
    pub data_type: DataType,
    pub asc: bool,
    pub nulls_first: bool,
}

impl fmt::Display for PhysicalSortExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.expr, if self.asc { "ASC" } else { "DESC" })?;
        if self.nulls_first == self.asc {
            write!(f, " NULLS FIRST")?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct SortExec {
    pub input: Arc<dyn ExecutionPlan>,
    pub exprs: Vec<PhysicalSortExpr>,
    pub fetch: Option<usize>,
    metrics: Arc<Metrics>,
}

type RowCmp<'a> = Box<dyn Fn(usize, usize) -> Ordering + 'a>;

/// Build a comparator over one sort key column.
fn comparator(col: &Column, asc: bool, nulls_first: bool) -> RowCmp<'_> {
    fn typed<'a, T: Copy + 'a>(v: &'a [T], f: impl Fn(T, T) -> Ordering + 'a) -> RowCmp<'a> {
        Box::new(move |a, b| f(v[a], v[b]))
    }
    let values: RowCmp<'_> = match &col.data {
        ColumnData::Int64(v) => typed(v, |a: i64, b| a.cmp(&b)),
        ColumnData::Date(v) => typed(v, |a: i32, b| a.cmp(&b)),
        ColumnData::Float64(v) => typed(v, |a: f64, b| a.total_cmp(&b)),
        ColumnData::Boolean(v) => typed(v, |a: bool, b| a.cmp(&b)),
        ColumnData::Utf8(s) => Box::new(move |a, b| s.get_bytes(a).cmp(s.get_bytes(b))),
    };
    let values: RowCmp<'_> = if asc {
        values
    } else {
        Box::new(move |a, b| values(b, a))
    };
    match &col.validity {
        None => values,
        Some(valid) => Box::new(move |a, b| match (valid.get(a), valid.get(b)) {
            (true, true) => values(a, b),
            (false, false) => Ordering::Equal,
            (false, true) => {
                if nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (true, false) => {
                if nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
        }),
    }
}

/// Compute the sorted row permutation of `batch` (first `fetch` rows only, if given).
pub fn sort_indices(batch: &Batch, exprs: &[PhysicalSortExpr], fetch: Option<usize>) -> Result<Vec<u32>> {
    let keys = exprs
        .iter()
        .map(|e| e.expr.evaluate_to_column(batch, e.data_type))
        .collect::<Result<Vec<_>>>()?;
    let cmps: Vec<RowCmp<'_>> = keys
        .iter()
        .zip(exprs)
        .map(|(k, e)| comparator(k, e.asc, e.nulls_first))
        .collect();
    let cmp = |a: &u32, b: &u32| {
        for c in &cmps {
            match c(*a as usize, *b as usize) {
                Ordering::Equal => continue,
                o => return o,
            }
        }
        a.cmp(b) // stable tie-break
    };
    let mut perm: Vec<u32> = (0..batch.num_rows as u32).collect();
    match fetch {
        Some(k) if k < perm.len() => {
            if k == 0 {
                return Ok(vec![]);
            }
            perm.select_nth_unstable_by(k - 1, cmp);
            perm.truncate(k);
            perm.sort_unstable_by(cmp);
        }
        _ => perm.sort_unstable_by(cmp),
    }
    Ok(perm)
}

impl SortExec {
    pub fn new(input: Arc<dyn ExecutionPlan>, exprs: Vec<PhysicalSortExpr>, fetch: Option<usize>) -> Self {
        SortExec {
            input,
            exprs,
            fetch,
            metrics: Metrics::new(),
        }
    }
}

impl ExecutionPlan for SortExec {
    fn name(&self) -> &'static str {
        "SortExec"
    }
    fn details(&self) -> String {
        let mut s = super::exprs_to_string(&self.exprs);
        if let Some(k) = self.fetch {
            s.push_str(&format!(", fetch={k}"));
        }
        s
    }
    fn schema(&self) -> SchemaRef {
        self.input.schema()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }
    fn execute(&self, _partition: usize) -> Result<BatchStream> {
        let batches = collect_partitions(&self.input)?;
        let _t = self.metrics.timer();
        let schema = self.input.schema();
        if batches.is_empty() {
            return Ok(Box::new(std::iter::empty()));
        }
        let batch = Batch::concat(&batches, schema.len());
        let perm = sort_indices(&batch, &self.exprs, self.fetch)?;
        let sorted = batch.take(&perm);
        let total = sorted.num_rows;
        let out: Vec<Batch> = if total <= BATCH_SIZE {
            vec![sorted]
        } else {
            (0..total)
                .step_by(BATCH_SIZE)
                .map(|o| sorted.slice(o, BATCH_SIZE.min(total - o)))
                .collect()
        };
        for b in &out {
            self.metrics.record_batch(b);
        }
        Ok(Box::new(out.into_iter().map(Ok)))
    }
    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnBuilder;
    use crate::types::ScalarValue;

    fn batch(v: &[Option<i64>]) -> Batch {
        let mut b = ColumnBuilder::new(DataType::Int64);
        for x in v {
            b.push(&x.map_or(ScalarValue::Null(DataType::Int64), ScalarValue::Int64));
        }
        Batch::from_columns(vec![b.finish()])
    }

    fn key(asc: bool, nulls_first: bool) -> Vec<PhysicalSortExpr> {
        vec![PhysicalSortExpr {
            expr: PhysicalExpr::Column(0, "x".into()),
            data_type: DataType::Int64,
            asc,
            nulls_first,
        }]
    }

    #[test]
    fn null_ordering_and_direction() {
        let b = batch(&[Some(3), None, Some(1), Some(2)]);
        assert_eq!(sort_indices(&b, &key(true, false), None).unwrap(), vec![2, 3, 0, 1]);
        assert_eq!(sort_indices(&b, &key(true, true), None).unwrap(), vec![1, 2, 3, 0]);
        assert_eq!(sort_indices(&b, &key(false, true), None).unwrap(), vec![1, 0, 3, 2]);
    }

    #[test]
    fn top_k_matches_full_sort_prefix() {
        let vals: Vec<Option<i64>> = (0..1000).map(|i| Some((i * 7919) % 1009)).collect();
        let b = batch(&vals);
        let full = sort_indices(&b, &key(false, false), None).unwrap();
        let top = sort_indices(&b, &key(false, false), Some(10)).unwrap();
        assert_eq!(top, full[..10].to_vec());
    }
}
