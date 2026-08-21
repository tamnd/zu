//! Validity bitmaps, one bit per row, stored separately from values.
//!
//! Layout: `count: u32 LE`, `set_count: u32 LE`, then ceil(count / 64)
//! little-endian u64 words. All-valid and all-null segments are expected to
//! be elided by the caller via `SegmentMeta.null_count`; this container is
//! for the mixed case.

use zu_common::{Result, ZuError};

/// A fixed-length bitmap. Bit i set means row i is valid.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Validity {
    len: usize,
    words: Vec<u64>,
}

impl Validity {
    pub fn new_all_valid(len: usize) -> Self {
        let mut v = Self {
            len,
            words: vec![u64::MAX; len.div_ceil(64)],
        };
        v.clear_tail();
        v
    }

    pub fn new_all_null(len: usize) -> Self {
        Self {
            len,
            words: vec![0; len.div_ceil(64)],
        }
    }

    pub fn from_bools(bits: &[bool]) -> Self {
        let mut v = Self::new_all_null(bits.len());
        for (i, &b) in bits.iter().enumerate() {
            if b {
                v.set(i);
            }
        }
        v
    }

    fn clear_tail(&mut self) {
        let tail = self.len % 64;
        if tail != 0
            && let Some(last) = self.words.last_mut()
        {
            *last &= (1u64 << tail) - 1;
        }
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
    pub fn set(&mut self, i: usize) {
        debug_assert!(i < self.len);
        self.words[i / 64] |= 1 << (i % 64);
    }

    #[inline]
    pub fn unset(&mut self, i: usize) {
        debug_assert!(i < self.len);
        self.words[i / 64] &= !(1 << (i % 64));
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        debug_assert!(i < self.len);
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    pub fn count_valid(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }

    pub fn encode(&self, out: &mut Vec<u8>) -> usize {
        let start = out.len();
        out.extend_from_slice(&(self.len as u32).to_le_bytes());
        out.extend_from_slice(&(self.count_valid() as u32).to_le_bytes());
        for w in &self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out.len() - start
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let corrupt = |detail: &str| ZuError::Corrupt {
            what: "validity",
            detail: detail.to_string(),
        };
        let header = bytes.get(..8).ok_or_else(|| corrupt("truncated header"))?;
        let len = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let set_count = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let n_words = len.div_ceil(64);
        let body = bytes
            .get(8..8 + n_words * 8)
            .ok_or_else(|| corrupt("truncated body"))?;
        let words: Vec<u64> = body
            .as_chunks::<8>()
            .0
            .iter()
            .map(|c| u64::from_le_bytes(*c))
            .collect();
        let v = Self { len, words };
        if v.count_valid() != set_count {
            return Err(corrupt("set_count mismatch"));
        }
        if !v.len.is_multiple_of(64)
            && let Some(&last) = v.words.last()
            && last & !((1u64 << (v.len % 64)) - 1) != 0
        {
            return Err(corrupt("nonzero tail bits"));
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_mixed() {
        let bits: Vec<bool> = (0..1000).map(|i| i % 3 != 0).collect();
        let v = Validity::from_bools(&bits);
        assert_eq!(v.count_valid(), bits.iter().filter(|&&b| b).count());
        let mut buf = Vec::new();
        v.encode(&mut buf);
        let back = Validity::decode(&buf).unwrap();
        assert_eq!(v, back);
        for (i, &b) in bits.iter().enumerate() {
            assert_eq!(back.get(i), b);
        }
    }

    #[test]
    fn set_unset() {
        let mut v = Validity::new_all_valid(130);
        assert_eq!(v.count_valid(), 130);
        v.unset(129);
        v.unset(0);
        assert_eq!(v.count_valid(), 128);
        v.set(0);
        assert!(v.get(0) && !v.get(129));
    }

    #[test]
    fn corruption_detected() {
        let v = Validity::from_bools(&[true, false, true]);
        let mut buf = Vec::new();
        v.encode(&mut buf);
        assert!(Validity::decode(&buf[..buf.len() - 1]).is_err());
        let mut bad = buf.clone();
        bad[4] ^= 1;
        assert!(Validity::decode(&bad).is_err());
        let mut tail = buf;
        *tail.last_mut().unwrap() |= 0x80;
        assert!(Validity::decode(&tail).is_err());
    }
}
