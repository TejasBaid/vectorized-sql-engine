//! Variable-length UTF-8 string storage: one contiguous byte buffer plus offsets.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringData {
    /// `offsets[i]..offsets[i+1]` is the byte range of string `i`.
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl Default for StringData {
    fn default() -> Self {
        Self::new()
    }
}

impl StringData {
    pub fn new() -> Self {
        StringData {
            offsets: vec![0],
            bytes: Vec::new(),
        }
    }

    pub fn with_capacity(rows: usize, bytes: usize) -> Self {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        StringData {
            offsets,
            bytes: Vec::with_capacity(bytes),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn total_bytes(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn push(&mut self, s: &str) {
        self.bytes.extend_from_slice(s.as_bytes());
        self.offsets.push(self.bytes.len() as u32);
    }

    #[inline]
    pub fn get(&self, i: usize) -> &str {
        let start = self.offsets[i] as usize;
        let end = self.offsets[i + 1] as usize;
        // SAFETY: bytes are only ever appended from `&str` values, and offsets
        // always fall on the boundaries between those appended strings.
        unsafe { std::str::from_utf8_unchecked(&self.bytes[start..end]) }
    }

    #[inline]
    pub fn get_bytes(&self, i: usize) -> &[u8] {
        &self.bytes[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }

    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        (0..self.len()).map(move |i| self.get(i))
    }
}

impl<'a> FromIterator<&'a str> for StringData {
    fn from_iter<T: IntoIterator<Item = &'a str>>(iter: T) -> Self {
        let mut s = StringData::new();
        for v in iter {
            s.push(v);
        }
        s
    }
}
