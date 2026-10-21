//! Fast non-cryptographic hashing for hash joins and aggregation.
//!
//! Uses the FxHash multiply-rotate scheme (as in rustc) plus a final mixer so
//! the high bits used by hashbrown's SIMD probing are well distributed.

use crate::column::{Column, ColumnData};
use std::hash::{BuildHasherDefault, Hasher};

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

#[inline]
fn fx_add(h: u64, word: u64) -> u64 {
    (h.rotate_left(5) ^ word).wrapping_mul(SEED)
}

#[inline]
pub fn mix(mut x: u64) -> u64 {
    // murmur3 finalizer
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^ (x >> 33)
}

#[inline]
pub fn hash_bytes(mut h: u64, bytes: &[u8]) -> u64 {
    let (chunks, rest) = bytes.as_chunks::<8>();
    for c in chunks {
        h = fx_add(h, u64::from_le_bytes(*c));
    }
    let mut tail = 0u64;
    for (i, b) in rest.iter().enumerate() {
        tail |= (*b as u64) << (i * 8);
    }
    fx_add(fx_add(h, tail), bytes.len() as u64)
}

/// Combine the hash of every row of `col` into `hashes` (which must have
/// `col.len()` entries). NULLs hash to a fixed value.
pub fn hash_column(col: &Column, hashes: &mut [u64]) {
    const NULL_HASH: u64 = 0x9e37_79b9_7f4a_7c15;
    match &col.data {
        ColumnData::Int64(v) => {
            for (h, x) in hashes.iter_mut().zip(v) {
                *h = mix(fx_add(*h, *x as u64));
            }
        }
        ColumnData::Date(v) => {
            for (h, x) in hashes.iter_mut().zip(v) {
                *h = mix(fx_add(*h, *x as u64));
            }
        }
        ColumnData::Float64(v) => {
            for (h, x) in hashes.iter_mut().zip(v) {
                let x = if *x == 0.0 { 0.0 } else { *x }; // -0.0 == 0.0
                *h = mix(fx_add(*h, x.to_bits()));
            }
        }
        ColumnData::Boolean(v) => {
            for (h, x) in hashes.iter_mut().zip(v) {
                *h = mix(fx_add(*h, *x as u64));
            }
        }
        ColumnData::Utf8(s) => {
            for (i, h) in hashes.iter_mut().enumerate() {
                *h = mix(hash_bytes(*h, s.get_bytes(i)));
            }
        }
    }
    if let Some(validity) = &col.validity {
        for (i, h) in hashes.iter_mut().enumerate() {
            if !validity.get(i) {
                *h = mix(fx_add(*h, NULL_HASH));
            }
        }
    }
}

/// A `Hasher` that FxHashes its input; for `HashMap`s keyed by byte strings or ints.
#[derive(Default, Clone, Copy)]
pub struct FxHasher {
    hash: u64,
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        self.hash = hash_bytes(self.hash, bytes);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.hash = fx_add(self.hash, i);
    }
    #[inline]
    fn write_i64(&mut self, i: i64) {
        self.hash = fx_add(self.hash, i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.hash = fx_add(self.hash, i as u64);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.hash = fx_add(self.hash, i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        mix(self.hash)
    }
}

pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
pub type FxHashMap<K, V> = std::collections::HashMap<K, V, FxBuildHasher>;
pub type FxHashSet<K> = std::collections::HashSet<K, FxBuildHasher>;

/// Hasher for keys that are already hashes: passes the value through.
#[derive(Default, Clone, Copy)]
pub struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 = (self.0 << 8) | *b as u64;
        }
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.0 = i;
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub type IdentityBuildHasher = BuildHasherDefault<IdentityHasher>;
