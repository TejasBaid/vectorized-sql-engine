//! Parallel hash aggregation.
//!
//! Each input partition is aggregated on its own thread into a thread-local
//! hash table (no synchronization on the hot path). The partial tables are
//! then merged. Accumulators are *vectorized*: the group table first maps a
//! whole batch to a vector of group ids, then each accumulator updates its
//! columnar state in one tight loop (`state[group_ids[i]] += values[i]`).

use super::expr::PhysicalExpr;
use super::{BatchStream, ExecutionPlan, Metrics, exprs_to_string};
use crate::column::{BATCH_SIZE, Batch, Bitmap, Column, ColumnBuilder, ColumnData, ColumnRef, SchemaRef, StringData};
use crate::compute::hash::{FxHashMap, FxHashSet, IdentityBuildHasher, hash_column};
use crate::err;
use crate::error::Result;
use crate::logical::AggFunc;
use crate::types::DataType;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct AggSpec {
    pub func: AggFunc,
    pub arg: Option<PhysicalExpr>,
    pub distinct: bool,
    pub input_type: DataType,
    pub output_type: DataType,
}

impl fmt::Display for AggSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.arg {
            None => write!(f, "{}(*)", self.func.name()),
            Some(a) => write!(
                f,
                "{}({}{a})",
                self.func.name(),
                if self.distinct { "DISTINCT " } else { "" }
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Group key table
// ---------------------------------------------------------------------------

/// Maps group keys to dense group ids.
enum GroupKeys {
    /// No GROUP BY: everything is group 0.
    Global,
    /// Single integer-like key (BIGINT or DATE): direct `i64` hash map.
    Int {
        map: FxHashMap<i64, u32>,
        null_group: Option<u32>,
        keys: Vec<i64>,
        valid: Vec<bool>,
        dtype: DataType,
    },
    /// Arbitrary (multi-column) keys: vectorized hashing, then a hash table
    /// from key hash to group id with collision chains, verifying candidates
    /// against the typed key columns stored per group.
    Columns {
        map: HashMap<u64, u32, IdentityBuildHasher>,
        /// Next group with the same hash (`NO_GROUP` terminates the chain).
        chain: Vec<u32>,
        hashes: Vec<u64>,
        stores: Vec<KeyStore>,
    },
}

const NO_GROUP: u32 = u32::MAX;

/// Typed storage for the values of one GROUP BY column, one slot per group.
enum KeyStore {
    Int(Vec<i64>, Vec<bool>),
    Float(Vec<f64>, Vec<bool>),
    Date(Vec<i32>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    Str(StringData, Vec<bool>),
}

impl KeyStore {
    fn new(t: DataType) -> Self {
        match t {
            DataType::Float64 => KeyStore::Float(vec![], vec![]),
            DataType::Date => KeyStore::Date(vec![], vec![]),
            DataType::Boolean => KeyStore::Bool(vec![], vec![]),
            DataType::Utf8 => KeyStore::Str(StringData::new(), vec![]),
            _ => KeyStore::Int(vec![], vec![]),
        }
    }

    fn push_from(&mut self, col: &Column, j: usize) {
        let valid = col.is_valid(j);
        match (self, &col.data) {
            (KeyStore::Int(v, k), ColumnData::Int64(x)) => {
                v.push(x[j]);
                k.push(valid);
            }
            (KeyStore::Float(v, k), ColumnData::Float64(x)) => {
                v.push(x[j]);
                k.push(valid);
            }
            (KeyStore::Date(v, k), ColumnData::Date(x)) => {
                v.push(x[j]);
                k.push(valid);
            }
            (KeyStore::Bool(v, k), ColumnData::Boolean(x)) => {
                v.push(x[j]);
                k.push(valid);
            }
            (KeyStore::Str(v, k), ColumnData::Utf8(x)) => {
                v.push(x.get(j));
                k.push(valid);
            }
            _ => unreachable!("group key type mismatch"),
        }
    }

    fn push_from_store(&mut self, other: &KeyStore, g: usize) {
        match (self, other) {
            (KeyStore::Int(v, k), KeyStore::Int(x, ok)) => {
                v.push(x[g]);
                k.push(ok[g]);
            }
            (KeyStore::Float(v, k), KeyStore::Float(x, ok)) => {
                v.push(x[g]);
                k.push(ok[g]);
            }
            (KeyStore::Date(v, k), KeyStore::Date(x, ok)) => {
                v.push(x[g]);
                k.push(ok[g]);
            }
            (KeyStore::Bool(v, k), KeyStore::Bool(x, ok)) => {
                v.push(x[g]);
                k.push(ok[g]);
            }
            (KeyStore::Str(v, k), KeyStore::Str(x, ok)) => {
                v.push(x.get(g));
                k.push(ok[g]);
            }
            _ => unreachable!("group key type mismatch"),
        }
    }

    /// Grouping equality: NULL groups with NULL.
    #[inline]
    fn eq_col(&self, g: usize, col: &Column, j: usize) -> bool {
        let valid = col.is_valid(j);
        match (self, &col.data) {
            (KeyStore::Int(v, k), ColumnData::Int64(x)) => k[g] == valid && (!valid || v[g] == x[j]),
            (KeyStore::Float(v, k), ColumnData::Float64(x)) => k[g] == valid && (!valid || float_key_eq(v[g], x[j])),
            (KeyStore::Date(v, k), ColumnData::Date(x)) => k[g] == valid && (!valid || v[g] == x[j]),
            (KeyStore::Bool(v, k), ColumnData::Boolean(x)) => k[g] == valid && (!valid || v[g] == x[j]),
            (KeyStore::Str(v, k), ColumnData::Utf8(x)) => {
                k[g] == valid && (!valid || bytes_eq(v.get_bytes(g), x.get_bytes(j)))
            }
            _ => false,
        }
    }

    fn eq_store(&self, g: usize, other: &KeyStore, h: usize) -> bool {
        match (self, other) {
            (KeyStore::Int(v, k), KeyStore::Int(x, ok)) => k[g] == ok[h] && (!k[g] || v[g] == x[h]),
            (KeyStore::Float(v, k), KeyStore::Float(x, ok)) => k[g] == ok[h] && (!k[g] || float_key_eq(v[g], x[h])),
            (KeyStore::Date(v, k), KeyStore::Date(x, ok)) => k[g] == ok[h] && (!k[g] || v[g] == x[h]),
            (KeyStore::Bool(v, k), KeyStore::Bool(x, ok)) => k[g] == ok[h] && (!k[g] || v[g] == x[h]),
            (KeyStore::Str(v, k), KeyStore::Str(x, ok)) => k[g] == ok[h] && (!k[g] || v.get_bytes(g) == x.get_bytes(h)),
            _ => false,
        }
    }

    fn into_column(self) -> Column {
        let bitmap = |k: &[bool]| k.iter().any(|v| !v).then(|| Bitmap::from_bools(k));
        match self {
            KeyStore::Int(v, k) => Column::new(ColumnData::Int64(v), bitmap(&k)),
            KeyStore::Float(v, k) => Column::new(ColumnData::Float64(v), bitmap(&k)),
            KeyStore::Date(v, k) => Column::new(ColumnData::Date(v), bitmap(&k)),
            KeyStore::Bool(v, k) => Column::new(ColumnData::Boolean(v), bitmap(&k)),
            KeyStore::Str(v, k) => Column::new(ColumnData::Utf8(v), bitmap(&k)),
        }
    }
}

/// Byte equality that avoids a `memcmp` call for the short strings typical of
/// grouping keys.
#[inline]
fn bytes_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len()
        && if a.len() <= 16 {
            a.iter().zip(b).all(|(x, y)| x == y)
        } else {
            a == b
        }
}

#[inline]
fn float_key_eq(a: f64, b: f64) -> bool {
    a == b || (a.is_nan() && b.is_nan())
}

fn encode_value(col: &Column, i: usize, buf: &mut Vec<u8>) {
    if !col.is_valid(i) {
        buf.push(0);
        return;
    }
    buf.push(1);
    match &col.data {
        ColumnData::Int64(v) => buf.extend_from_slice(&v[i].to_le_bytes()),
        ColumnData::Date(v) => buf.extend_from_slice(&v[i].to_le_bytes()),
        ColumnData::Float64(v) => {
            let x = if v[i] == 0.0 { 0.0 } else { v[i] };
            let bits = if x.is_nan() { f64::NAN.to_bits() } else { x.to_bits() };
            buf.extend_from_slice(&bits.to_le_bytes());
        }
        ColumnData::Boolean(v) => buf.push(v[i] as u8),
        ColumnData::Utf8(s) => {
            let b = s.get_bytes(i);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
    }
}

impl GroupKeys {
    fn new(types: &[DataType]) -> Self {
        match types {
            [] => GroupKeys::Global,
            [t @ (DataType::Int64 | DataType::Date)] => GroupKeys::Int {
                map: FxHashMap::default(),
                null_group: None,
                keys: vec![],
                valid: vec![],
                dtype: *t,
            },
            _ => GroupKeys::Columns {
                map: HashMap::default(),
                chain: vec![],
                hashes: vec![],
                stores: types.iter().map(|t| KeyStore::new(*t)).collect(),
            },
        }
    }

    fn num_groups(&self) -> usize {
        match self {
            GroupKeys::Global => 1,
            GroupKeys::Int { keys, .. } => keys.len(),
            GroupKeys::Columns { hashes, .. } => hashes.len(),
        }
    }

    /// Assign a group id to every row of the batch.
    fn group_ids(&mut self, cols: &[ColumnRef], n: usize, out: &mut Vec<u32>) {
        out.clear();
        match self {
            GroupKeys::Global => out.resize(n, 0),
            GroupKeys::Int {
                map,
                null_group,
                keys,
                valid,
                ..
            } => {
                let col = &cols[0];
                let mut lookup = |k: i64, keys: &mut Vec<i64>, valid: &mut Vec<bool>| -> u32 {
                    *map.entry(k).or_insert_with(|| {
                        keys.push(k);
                        valid.push(true);
                        (keys.len() - 1) as u32
                    })
                };
                match &col.data {
                    ColumnData::Int64(v) => {
                        for (i, &k) in v.iter().enumerate() {
                            if col.is_valid(i) {
                                out.push(lookup(k, keys, valid));
                            } else {
                                out.push(*null_group.get_or_insert_with(|| {
                                    keys.push(0);
                                    valid.push(false);
                                    (keys.len() - 1) as u32
                                }));
                            }
                        }
                    }
                    ColumnData::Date(v) => {
                        for (i, &k) in v.iter().enumerate() {
                            if col.is_valid(i) {
                                out.push(lookup(k as i64, keys, valid));
                            } else {
                                out.push(*null_group.get_or_insert_with(|| {
                                    keys.push(0);
                                    valid.push(false);
                                    (keys.len() - 1) as u32
                                }));
                            }
                        }
                    }
                    _ => unreachable!("integer group key"),
                }
            }
            GroupKeys::Columns {
                map,
                chain,
                hashes,
                stores,
            } => {
                let mut row_hashes = vec![0u64; n];
                for c in cols {
                    hash_column(c, &mut row_hashes);
                }
                for (j, &h) in row_hashes.iter().enumerate() {
                    let head = map.entry(h).or_insert(NO_GROUP);
                    let mut g = *head;
                    while g != NO_GROUP && !stores.iter().zip(cols).all(|(s, c)| s.eq_col(g as usize, c, j)) {
                        g = chain[g as usize];
                    }
                    if g == NO_GROUP {
                        g = hashes.len() as u32;
                        for (s, c) in stores.iter_mut().zip(cols) {
                            s.push_from(c, j);
                        }
                        hashes.push(h);
                        chain.push(*head);
                        *head = g;
                    }
                    out.push(g);
                }
            }
        }
    }

    /// Merge `other` into `self`; returns `mapping[other_id] = self_id`.
    fn merge(&mut self, other: GroupKeys) -> Vec<u32> {
        match (self, other) {
            (GroupKeys::Global, GroupKeys::Global) => vec![0],
            (
                GroupKeys::Int {
                    map,
                    null_group,
                    keys,
                    valid,
                    ..
                },
                GroupKeys::Int {
                    keys: okeys,
                    valid: ovalid,
                    ..
                },
            ) => okeys
                .iter()
                .zip(&ovalid)
                .map(|(&k, &v)| {
                    if v {
                        *map.entry(k).or_insert_with(|| {
                            keys.push(k);
                            valid.push(true);
                            (keys.len() - 1) as u32
                        })
                    } else {
                        *null_group.get_or_insert_with(|| {
                            keys.push(0);
                            valid.push(false);
                            (keys.len() - 1) as u32
                        })
                    }
                })
                .collect(),
            (
                GroupKeys::Columns {
                    map,
                    chain,
                    hashes,
                    stores,
                },
                GroupKeys::Columns {
                    hashes: ohashes,
                    stores: ostores,
                    ..
                },
            ) => {
                let mut mapping = Vec::with_capacity(ohashes.len());
                for (o, &h) in ohashes.iter().enumerate() {
                    let head = map.entry(h).or_insert(NO_GROUP);
                    let mut g = *head;
                    while g != NO_GROUP && !stores.iter().zip(&ostores).all(|(s, os)| s.eq_store(g as usize, os, o)) {
                        g = chain[g as usize];
                    }
                    if g == NO_GROUP {
                        g = hashes.len() as u32;
                        for (s, os) in stores.iter_mut().zip(&ostores) {
                            s.push_from_store(os, o);
                        }
                        hashes.push(h);
                        chain.push(*head);
                        *head = g;
                    }
                    mapping.push(g);
                }
                mapping
            }
            _ => unreachable!("mismatched group tables"),
        }
    }

    fn into_columns(self) -> Vec<Column> {
        match self {
            GroupKeys::Global => vec![],
            GroupKeys::Int { keys, valid, dtype, .. } => {
                let validity = valid.iter().any(|v| !v).then(|| Bitmap::from_bools(&valid));
                let data = match dtype {
                    DataType::Date => ColumnData::Date(keys.iter().map(|&k| k as i32).collect()),
                    _ => ColumnData::Int64(keys),
                };
                vec![Column::new(data, validity)]
            }
            GroupKeys::Columns { stores, .. } => stores.into_iter().map(KeyStore::into_column).collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Accumulators
// ---------------------------------------------------------------------------

enum MinMax {
    Int(Vec<i64>, Vec<bool>),
    Float(Vec<f64>, Vec<bool>),
    Date(Vec<i32>, Vec<bool>),
    Bool(Vec<bool>, Vec<bool>),
    Str(Vec<Option<String>>),
}

/// Additive accumulators keep `LANES` partial states per group, indexed by
/// `group * LANES + row % LANES`. With few groups, consecutive rows hit the
/// same state slot, and a single slot would serialize every update on one
/// load-add-store dependency chain; interleaving lanes breaks that chain
/// (worth ~2x on TPC-H Q1). Lanes are folded together in `finish`.
const LANES: usize = 4;

enum Accumulator {
    CountStar(Vec<i64>),
    Count(Vec<i64>),
    CountDistinct(Vec<FxHashSet<Vec<u8>>>),
    SumInt { sums: Vec<i64>, seen: Vec<bool> },
    SumFloat { sums: Vec<f64>, seen: Vec<bool> },
    Avg { sums: Vec<f64>, counts: Vec<i64> },
    MinMax { is_min: bool, state: MinMax },
}

/// Generic min/max update over a primitive slice.
fn minmax_update<T: Copy + PartialOrd>(
    state: &mut [T],
    seen: &mut [bool],
    groups: &[u32],
    vals: &[T],
    col: &Column,
    is_min: bool,
) {
    for (i, (&g, &v)) in groups.iter().zip(vals).enumerate() {
        if !col.is_valid(i) {
            continue;
        }
        let g = g as usize;
        if !seen[g] || (is_min && v < state[g]) || (!is_min && v > state[g]) {
            state[g] = v;
            seen[g] = true;
        }
    }
}

fn minmax_merge<T: Copy + PartialOrd>(
    state: &mut [T],
    seen: &mut [bool],
    ostate: &[T],
    oseen: &[bool],
    mapping: &[u32],
    is_min: bool,
) {
    for (o, &g) in mapping.iter().enumerate() {
        if !oseen[o] {
            continue;
        }
        let g = g as usize;
        let v = ostate[o];
        if !seen[g] || (is_min && v < state[g]) || (!is_min && v > state[g]) {
            state[g] = v;
            seen[g] = true;
        }
    }
}

impl Accumulator {
    fn new(spec: &AggSpec) -> Result<Self> {
        Ok(match (spec.func, spec.distinct, spec.input_type) {
            (AggFunc::Count, _, _) if spec.arg.is_none() => Accumulator::CountStar(vec![]),
            (AggFunc::Count, true, _) => Accumulator::CountDistinct(vec![]),
            (AggFunc::Count, false, _) => Accumulator::Count(vec![]),
            (AggFunc::Sum | AggFunc::Avg, true, _) => {
                return err!(Unsupported, "DISTINCT is only supported for COUNT, MIN and MAX");
            }
            (AggFunc::Sum, _, DataType::Int64) => Accumulator::SumInt {
                sums: vec![],
                seen: vec![],
            },
            (AggFunc::Sum, _, _) => Accumulator::SumFloat {
                sums: vec![],
                seen: vec![],
            },
            (AggFunc::Avg, _, _) => Accumulator::Avg {
                sums: vec![],
                counts: vec![],
            },
            (AggFunc::Min | AggFunc::Max, _, t) => Accumulator::MinMax {
                is_min: spec.func == AggFunc::Min,
                state: match t {
                    DataType::Float64 => MinMax::Float(vec![], vec![]),
                    DataType::Date => MinMax::Date(vec![], vec![]),
                    DataType::Boolean => MinMax::Bool(vec![], vec![]),
                    DataType::Utf8 => MinMax::Str(vec![]),
                    _ => MinMax::Int(vec![], vec![]),
                },
            },
        })
    }

    fn resize(&mut self, n: usize) {
        match self {
            Accumulator::CountStar(c) | Accumulator::Count(c) => c.resize(n * LANES, 0),
            Accumulator::CountDistinct(s) => s.resize_with(n, Default::default),
            Accumulator::SumInt { sums, seen } => {
                sums.resize(n * LANES, 0);
                seen.resize(n, false);
            }
            Accumulator::SumFloat { sums, seen } => {
                sums.resize(n * LANES, 0.0);
                seen.resize(n, false);
            }
            Accumulator::Avg { sums, counts } => {
                sums.resize(n * LANES, 0.0);
                counts.resize(n * LANES, 0);
            }
            Accumulator::MinMax { state, .. } => match state {
                MinMax::Int(v, s) => {
                    v.resize(n, 0);
                    s.resize(n, false);
                }
                MinMax::Float(v, s) => {
                    v.resize(n, 0.0);
                    s.resize(n, false);
                }
                MinMax::Date(v, s) => {
                    v.resize(n, 0);
                    s.resize(n, false);
                }
                MinMax::Bool(v, s) => {
                    v.resize(n, false);
                    s.resize(n, false);
                }
                MinMax::Str(v) => v.resize(n, None),
            },
        }
    }

    fn update(&mut self, groups: &[u32], num_groups: usize, values: Option<&Column>) -> Result<()> {
        self.resize(num_groups);
        let lane = |i: usize, g: u32| g as usize * LANES + (i & (LANES - 1));
        let Some(col) = values else {
            if let Accumulator::CountStar(c) = self {
                for (i, &g) in groups.iter().enumerate() {
                    c[lane(i, g)] += 1;
                }
            }
            return Ok(());
        };
        let all_valid = col.validity.is_none();
        match self {
            Accumulator::CountStar(c) => {
                for (i, &g) in groups.iter().enumerate() {
                    c[lane(i, g)] += 1;
                }
            }
            Accumulator::Count(c) => {
                for (i, &g) in groups.iter().enumerate() {
                    c[lane(i, g)] += col.is_valid(i) as i64;
                }
            }
            Accumulator::CountDistinct(sets) => {
                let mut buf = vec![];
                for (i, &g) in groups.iter().enumerate() {
                    if col.is_valid(i) {
                        buf.clear();
                        encode_value(col, i, &mut buf);
                        sets[g as usize].insert(buf.clone());
                    }
                }
            }
            Accumulator::SumInt { sums, seen } => {
                let vals = col.as_i64().unwrap_or(&[]);
                let mut overflow = false;
                for (i, (&g, &v)) in groups.iter().zip(vals).enumerate() {
                    if all_valid || col.is_valid(i) {
                        let k = lane(i, g);
                        let (s, o) = sums[k].overflowing_add(v);
                        sums[k] = s;
                        overflow |= o;
                        seen[g as usize] = true;
                    }
                }
                if overflow {
                    return err!(Execution, "integer overflow in SUM");
                }
            }
            Accumulator::SumFloat { sums, seen } => {
                let ints;
                let vals: &[f64] = match &col.data {
                    ColumnData::Float64(v) => v,
                    ColumnData::Int64(v) => {
                        ints = v.iter().map(|&x| x as f64).collect::<Vec<_>>();
                        &ints
                    }
                    _ => &[],
                };
                if all_valid {
                    for (i, (&g, &v)) in groups.iter().zip(vals).enumerate() {
                        sums[lane(i, g)] += v;
                    }
                    for &g in groups {
                        seen[g as usize] = true;
                    }
                } else {
                    for (i, (&g, &v)) in groups.iter().zip(vals).enumerate() {
                        if col.is_valid(i) {
                            sums[lane(i, g)] += v;
                            seen[g as usize] = true;
                        }
                    }
                }
            }
            Accumulator::Avg { sums, counts } => {
                let ints;
                let vals: &[f64] = match &col.data {
                    ColumnData::Float64(v) => v,
                    ColumnData::Int64(v) => {
                        ints = v.iter().map(|&x| x as f64).collect::<Vec<_>>();
                        &ints
                    }
                    _ => &[],
                };
                if all_valid {
                    for (i, (&g, &v)) in groups.iter().zip(vals).enumerate() {
                        let k = lane(i, g);
                        sums[k] += v;
                        counts[k] += 1;
                    }
                } else {
                    for (i, (&g, &v)) in groups.iter().zip(vals).enumerate() {
                        if col.is_valid(i) {
                            let k = lane(i, g);
                            sums[k] += v;
                            counts[k] += 1;
                        }
                    }
                }
            }
            Accumulator::MinMax { is_min, state } => {
                let is_min = *is_min;
                match (state, &col.data) {
                    (MinMax::Int(s, seen), ColumnData::Int64(v)) => minmax_update(s, seen, groups, v, col, is_min),
                    (MinMax::Float(s, seen), ColumnData::Float64(v)) => minmax_update(s, seen, groups, v, col, is_min),
                    (MinMax::Date(s, seen), ColumnData::Date(v)) => minmax_update(s, seen, groups, v, col, is_min),
                    (MinMax::Bool(s, seen), ColumnData::Boolean(v)) => minmax_update(s, seen, groups, v, col, is_min),
                    (MinMax::Str(s), ColumnData::Utf8(v)) => {
                        for (i, &g) in groups.iter().enumerate() {
                            if !col.is_valid(i) {
                                continue;
                            }
                            let x = v.get(i);
                            let slot = &mut s[g as usize];
                            let replace = match slot {
                                None => true,
                                Some(cur) => (is_min && x < cur.as_str()) || (!is_min && x > cur.as_str()),
                            };
                            if replace {
                                *slot = Some(x.to_string());
                            }
                        }
                    }
                    _ => return err!(Execution, "MIN/MAX type mismatch"),
                }
            }
        }
        Ok(())
    }

    fn merge(&mut self, other: Accumulator, mapping: &[u32], num_groups: usize) -> Result<()> {
        self.resize(num_groups);
        match (self, other) {
            (Accumulator::CountStar(a), Accumulator::CountStar(b)) | (Accumulator::Count(a), Accumulator::Count(b)) => {
                for (k, v) in b.iter().enumerate() {
                    a[mapping[k / LANES] as usize * LANES + k % LANES] += v;
                }
            }
            (Accumulator::CountDistinct(a), Accumulator::CountDistinct(b)) => {
                for (o, set) in b.into_iter().enumerate() {
                    a[mapping[o] as usize].extend(set);
                }
            }
            (Accumulator::SumInt { sums, seen }, Accumulator::SumInt { sums: os, seen: oseen }) => {
                for (k, v) in os.iter().enumerate() {
                    let t = mapping[k / LANES] as usize * LANES + k % LANES;
                    sums[t] = sums[t]
                        .checked_add(*v)
                        .ok_or_else(|| crate::Error::new(crate::ErrorKind::Execution, "integer overflow in SUM"))?;
                }
                for (o, s) in oseen.iter().enumerate() {
                    seen[mapping[o] as usize] |= s;
                }
            }
            (Accumulator::SumFloat { sums, seen }, Accumulator::SumFloat { sums: os, seen: oseen }) => {
                for (k, v) in os.iter().enumerate() {
                    sums[mapping[k / LANES] as usize * LANES + k % LANES] += v;
                }
                for (o, s) in oseen.iter().enumerate() {
                    seen[mapping[o] as usize] |= s;
                }
            }
            (Accumulator::Avg { sums, counts }, Accumulator::Avg { sums: os, counts: oc }) => {
                for (k, (v, c)) in os.iter().zip(&oc).enumerate() {
                    let t = mapping[k / LANES] as usize * LANES + k % LANES;
                    sums[t] += v;
                    counts[t] += c;
                }
            }
            (Accumulator::MinMax { is_min, state }, Accumulator::MinMax { state: ostate, .. }) => {
                let is_min = *is_min;
                match (state, ostate) {
                    (MinMax::Int(s, seen), MinMax::Int(os, oseen)) => {
                        minmax_merge(s, seen, &os, &oseen, &mapping[..os.len()], is_min)
                    }
                    (MinMax::Float(s, seen), MinMax::Float(os, oseen)) => {
                        minmax_merge(s, seen, &os, &oseen, &mapping[..os.len()], is_min)
                    }
                    (MinMax::Date(s, seen), MinMax::Date(os, oseen)) => {
                        minmax_merge(s, seen, &os, &oseen, &mapping[..os.len()], is_min)
                    }
                    (MinMax::Bool(s, seen), MinMax::Bool(os, oseen)) => {
                        minmax_merge(s, seen, &os, &oseen, &mapping[..os.len()], is_min)
                    }
                    (MinMax::Str(s), MinMax::Str(os)) => {
                        for (o, v) in os.into_iter().enumerate() {
                            let Some(v) = v else { continue };
                            let slot = &mut s[mapping[o] as usize];
                            let replace = match slot {
                                None => true,
                                Some(cur) => (is_min && v < *cur) || (!is_min && v > *cur),
                            };
                            if replace {
                                *slot = Some(v);
                            }
                        }
                    }
                    _ => return err!(Execution, "MIN/MAX merge mismatch"),
                }
            }
            _ => return err!(Execution, "accumulator merge mismatch"),
        }
        Ok(())
    }

    fn finish(self, num_groups: usize) -> Result<Column> {
        let mut this = self;
        this.resize(num_groups);
        let opt = |seen: &[bool]| (!seen.iter().all(|&s| s)).then(|| Bitmap::from_bools(seen));
        let fold_i = |v: &[i64]| -> Vec<i64> { v.as_chunks::<LANES>().0.iter().map(|c| c.iter().sum()).collect() };
        let fold_f = |v: &[f64]| -> Vec<f64> { v.as_chunks::<LANES>().0.iter().map(|c| c.iter().sum()).collect() };
        Ok(match this {
            Accumulator::CountStar(c) | Accumulator::Count(c) => Column::from_data(ColumnData::Int64(fold_i(&c))),
            Accumulator::CountDistinct(s) => {
                Column::from_data(ColumnData::Int64(s.iter().map(|x| x.len() as i64).collect()))
            }
            Accumulator::SumInt { sums, seen } => {
                let totals = sums
                    .as_chunks::<LANES>()
                    .0
                    .iter()
                    .map(|c| c.iter().try_fold(0i64, |a, &b| a.checked_add(b)))
                    .collect::<Option<Vec<i64>>>()
                    .ok_or_else(|| crate::Error::new(crate::ErrorKind::Execution, "integer overflow in SUM"))?;
                Column::new(ColumnData::Int64(totals), opt(&seen))
            }
            Accumulator::SumFloat { sums, seen } => Column::new(ColumnData::Float64(fold_f(&sums)), opt(&seen)),
            Accumulator::Avg { sums, counts } => {
                let (sums, counts) = (fold_f(&sums), fold_i(&counts));
                let seen: Vec<bool> = counts.iter().map(|&c| c > 0).collect();
                let vals = sums
                    .iter()
                    .zip(&counts)
                    .map(|(s, &c)| if c > 0 { s / c as f64 } else { 0.0 })
                    .collect();
                Column::new(ColumnData::Float64(vals), opt(&seen))
            }
            Accumulator::MinMax { state, .. } => match state {
                MinMax::Int(v, s) => Column::new(ColumnData::Int64(v), opt(&s)),
                MinMax::Float(v, s) => Column::new(ColumnData::Float64(v), opt(&s)),
                MinMax::Date(v, s) => Column::new(ColumnData::Date(v), opt(&s)),
                MinMax::Bool(v, s) => Column::new(ColumnData::Boolean(v), opt(&s)),
                MinMax::Str(v) => {
                    let mut b = ColumnBuilder::with_capacity(DataType::Utf8, v.len());
                    for x in &v {
                        match x {
                            Some(s) => b.push_str(s),
                            None => b.push_null(),
                        }
                    }
                    b.finish()
                }
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Operator
// ---------------------------------------------------------------------------

struct PartialAgg {
    keys: GroupKeys,
    accs: Vec<Accumulator>,
}

#[derive(Debug)]
pub struct HashAggregateExec {
    pub input: Arc<dyn ExecutionPlan>,
    pub group_exprs: Arc<Vec<PhysicalExpr>>,
    pub group_types: Vec<DataType>,
    pub aggs: Arc<Vec<AggSpec>>,
    pub schema: SchemaRef,
    metrics: Arc<Metrics>,
}

impl HashAggregateExec {
    pub fn new(
        input: Arc<dyn ExecutionPlan>,
        group_exprs: Vec<PhysicalExpr>,
        group_types: Vec<DataType>,
        aggs: Vec<AggSpec>,
        schema: SchemaRef,
    ) -> Result<Self> {
        for a in &aggs {
            Accumulator::new(a)?; // validate up front
        }
        Ok(HashAggregateExec {
            input,
            group_exprs: Arc::new(group_exprs),
            group_types,
            aggs: Arc::new(aggs),
            schema,
            metrics: Metrics::new(),
        })
    }

    fn aggregate_partition(&self, stream: super::BatchStream) -> Result<PartialAgg> {
        let mut partial = PartialAgg {
            keys: GroupKeys::new(&self.group_types),
            accs: self.aggs.iter().map(Accumulator::new).collect::<Result<_>>()?,
        };
        let mut ids = Vec::with_capacity(BATCH_SIZE);
        for batch in stream {
            let batch = batch?;
            let _t = self.metrics.timer();
            let keys = self
                .group_exprs
                .iter()
                .zip(&self.group_types)
                .map(|(e, t)| e.evaluate_to_column(&batch, *t))
                .collect::<Result<Vec<_>>>()?;
            partial.keys.group_ids(&keys, batch.num_rows, &mut ids);
            let ng = partial.keys.num_groups();
            for (acc, spec) in partial.accs.iter_mut().zip(self.aggs.iter()) {
                let col = spec
                    .arg
                    .as_ref()
                    .map(|a| a.evaluate_to_column(&batch, spec.input_type))
                    .transpose()?;
                acc.update(&ids, ng, col.as_deref())?;
            }
        }
        Ok(partial)
    }

    fn run(&self) -> Result<Vec<Batch>> {
        let n = self.input.output_partitions();
        let streams = (0..n).map(|p| self.input.execute(p)).collect::<Result<Vec<_>>>()?;
        let partials: Vec<Result<PartialAgg>> = if n == 1 {
            streams.into_iter().map(|s| self.aggregate_partition(s)).collect()
        } else {
            std::thread::scope(|scope| {
                let handles: Vec<_> = streams
                    .into_iter()
                    .map(|s| scope.spawn(move || self.aggregate_partition(s)))
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("aggregation thread panicked"))
                    .collect()
            })
        };
        let _t = self.metrics.timer();
        let mut iter = partials.into_iter();
        let mut acc = iter.next().expect("at least one partition")?;
        for p in iter {
            let p = p?;
            let mapping = acc.keys.merge(p.keys);
            let ng = acc.keys.num_groups();
            for (a, b) in acc.accs.iter_mut().zip(p.accs) {
                a.merge(b, &mapping, ng)?;
            }
        }
        let num_groups = acc.keys.num_groups();
        // A global aggregate over zero rows still yields one row (COUNT = 0).
        let mut columns = acc.keys.into_columns();
        for a in acc.accs {
            columns.push(a.finish(num_groups)?);
        }
        let total = if self.group_types.is_empty() { 1 } else { num_groups };
        if total == 0 {
            return Ok(vec![]);
        }
        let batch = Batch::new(columns.into_iter().map(Arc::new).collect(), total);
        Ok((0..total)
            .step_by(BATCH_SIZE)
            .map(|off| {
                if total <= BATCH_SIZE {
                    batch.clone()
                } else {
                    batch.slice(off, BATCH_SIZE.min(total - off))
                }
            })
            .collect())
    }
}

impl ExecutionPlan for HashAggregateExec {
    fn name(&self) -> &'static str {
        "HashAggregateExec"
    }
    fn details(&self) -> String {
        format!(
            "groups=[{}], aggs=[{}]",
            exprs_to_string(&self.group_exprs),
            exprs_to_string(&self.aggs)
        )
    }
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        vec![self.input.clone()]
    }
    fn execute(&self, _partition: usize) -> Result<BatchStream> {
        let batches = self.run()?;
        for b in &batches {
            self.metrics.record_batch(b);
        }
        Ok(Box::new(batches.into_iter().map(Ok)))
    }
    fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }
}
