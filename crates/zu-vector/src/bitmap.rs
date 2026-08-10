//! Validity and predicate bitmap (perf/02 section 1.3).
//!
//! One bit per row, u64 words, arena-backed. Null propagation for
//! arithmetic is a word-wise AND, never a per-row branch. The invariant
//! throughout: bits at and above `len` are zero, so popcounts and the
//! bitmap-to-selection kernel never mask the tail.

use crate::arena::{MorselArena, RawBuf};

pub struct Bitmap {
    words: RawBuf,
    len: u32,
}

impl Bitmap {
    /// Allocate a bitmap of `len` bits, all set or all clear.
    pub fn new_in(arena: &mut MorselArena, len: usize, set: bool) -> Self {
        let words = len.div_ceil(64);
        let mut buf = arena.alloc_of::<u64>(words);
        if set {
            let w = buf.as_mut_slice::<u64>();
            w.fill(!0u64);
            let tail = len % 64;
            if tail != 0 {
                w[words - 1] = (1u64 << tail) - 1;
            }
        } else {
            // Arena blocks are recycled dirty, so clear is explicit.
            buf.as_mut_slice::<u64>().fill(0);
        }
        Self {
            words: buf,
            len: len as u32,
        }
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, i: usize) -> bool {
        debug_assert!(i < self.len as usize);
        self.words.as_slice::<u64>()[i / 64] >> (i % 64) & 1 == 1
    }

    #[inline]
    pub fn set(&mut self, i: usize) {
        debug_assert!(i < self.len as usize);
        self.words.as_mut_slice::<u64>()[i / 64] |= 1 << (i % 64);
    }

    #[inline]
    pub fn clear(&mut self, i: usize) {
        debug_assert!(i < self.len as usize);
        self.words.as_mut_slice::<u64>()[i / 64] &= !(1 << (i % 64));
    }

    pub fn words(&self) -> &[u64] {
        self.words.as_slice()
    }

    pub fn words_mut(&mut self) -> &mut [u64] {
        self.words.as_mut_slice()
    }

    /// Zero the tail bits above `len`. Kernels that write whole words
    /// call this once at the end instead of masking per word.
    pub fn mask_tail(&mut self) {
        let len = self.len as usize;
        let tail = len % 64;
        if tail != 0 {
            self.words.as_mut_slice::<u64>()[len / 64] &= (1u64 << tail) - 1;
        }
    }

    pub fn and_with(&mut self, other: &Bitmap) {
        debug_assert_eq!(self.len, other.len);
        for (w, o) in self
            .words
            .as_mut_slice::<u64>()
            .iter_mut()
            .zip(other.words())
        {
            *w &= o;
        }
    }

    pub fn or_with(&mut self, other: &Bitmap) {
        debug_assert_eq!(self.len, other.len);
        for (w, o) in self
            .words
            .as_mut_slice::<u64>()
            .iter_mut()
            .zip(other.words())
        {
            *w |= o;
        }
    }

    pub fn count_ones(&self) -> usize {
        self.words
            .as_slice::<u64>()
            .iter()
            .map(|w| w.count_ones() as usize)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_stays_masked() {
        let mut arena = MorselArena::new();
        let all = Bitmap::new_in(&mut arena, 70, true);
        assert_eq!(all.count_ones(), 70);
        let mut none = Bitmap::new_in(&mut arena, 70, false);
        none.set(69);
        assert_eq!(none.count_ones(), 1);
        assert!(none.get(69));
        none.clear(69);
        assert_eq!(none.count_ones(), 0);
    }

    #[test]
    fn word_ops() {
        let mut arena = MorselArena::new();
        let mut a = Bitmap::new_in(&mut arena, 130, false);
        let mut b = Bitmap::new_in(&mut arena, 130, false);
        a.set(0);
        a.set(129);
        b.set(129);
        a.and_with(&b);
        assert_eq!(a.count_ones(), 1);
        assert!(a.get(129));
        a.or_with(&b);
        assert_eq!(a.count_ones(), 1);
    }
}
