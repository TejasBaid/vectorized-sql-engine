//! Packed validity bitmap: bit `i` set means row `i` is NOT NULL.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitmap {
    words: Vec<u64>,
    len: usize,
}

impl Bitmap {
    pub fn new_set(len: usize) -> Self {
        let mut words = vec![u64::MAX; len.div_ceil(64)];
        if !len.is_multiple_of(64)
            && let Some(last) = words.last_mut()
        {
            *last = (1u64 << (len % 64)) - 1;
        }
        Bitmap { words, len }
    }

    pub fn new_unset(len: usize) -> Self {
        Bitmap {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    pub fn from_bools(bits: &[bool]) -> Self {
        let mut words = vec![0u64; bits.len().div_ceil(64)];
        for (i, &b) in bits.iter().enumerate() {
            words[i / 64] |= (b as u64) << (i % 64);
        }
        Bitmap { words, len: bits.len() }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        debug_assert!(i < self.len);
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    #[inline]
    pub fn set(&mut self, i: usize, v: bool) {
        let mask = 1u64 << (i % 64);
        if v {
            self.words[i / 64] |= mask;
        } else {
            self.words[i / 64] &= !mask;
        }
    }

    pub fn push(&mut self, v: bool) {
        if self.len.is_multiple_of(64) {
            self.words.push(0);
        }
        self.len += 1;
        self.set(self.len - 1, v);
    }

    pub fn count_set(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    /// Bitwise AND of two bitmaps of equal length.
    pub fn and(&self, other: &Bitmap) -> Bitmap {
        debug_assert_eq!(self.len, other.len);
        Bitmap {
            words: self.words.iter().zip(&other.words).map(|(a, b)| a & b).collect(),
            len: self.len,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = bool> + '_ {
        (0..self.len).map(move |i| self.get(i))
    }
}

/// Combine two optional validity bitmaps (None = all valid).
pub fn and_validity(a: Option<&Bitmap>, b: Option<&Bitmap>) -> Option<Bitmap> {
    match (a, b) {
        (None, None) => None,
        (Some(a), None) => Some(a.clone()),
        (None, Some(b)) => Some(b.clone()),
        (Some(a), Some(b)) => Some(a.and(b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_count() {
        let mut b = Bitmap::new_unset(130);
        b.set(0, true);
        b.set(64, true);
        b.set(129, true);
        assert!(b.get(64) && !b.get(65));
        assert_eq!(b.count_set(), 3);
        assert_eq!(Bitmap::new_set(130).count_set(), 130);
        let mut p = Bitmap::new_unset(0);
        for i in 0..100 {
            p.push(i % 3 == 0);
        }
        assert_eq!(p.count_set(), 34);
    }
}
