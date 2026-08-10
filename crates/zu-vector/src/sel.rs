//! Selection vectors (perf/02 section 1.3).
//!
//! Filters never move data: they produce or refine a selection, a sorted
//! list of u16 row indices into the vectors of a chunk. `None` at the
//! chunk level means identity. Compaction to flat happens only at
//! pipeline breaks when selectivity drops below 1/8.

use crate::VECTOR_SIZE;
use crate::arena::{MorselArena, RawBuf};
use crate::bitmap::Bitmap;

pub struct SelVector {
    idx: RawBuf,
    len: u32,
}

impl SelVector {
    /// Allocate an empty selection with room for `cap` indices.
    pub fn with_capacity(arena: &mut MorselArena, cap: usize) -> Self {
        debug_assert!(cap <= VECTOR_SIZE);
        Self {
            idx: arena.alloc_of::<u16>(cap),
            len: 0,
        }
    }

    /// Build a selection from the set bits of a predicate bitmap. This is
    /// the bitmap_to_sel kernel: one trailing_zeros loop per word, no
    /// per-bit branch on the clear bits. Output offsets are popcounted up
    /// front so the extraction chains of different words carry no
    /// dependence on each other and overlap in the out-of-order window;
    /// that roughly halves the latency at dense selectivities.
    pub fn from_bitmap(arena: &mut MorselArena, bits: &Bitmap) -> Self {
        let mut sel = Self::with_capacity(arena, bits.len());
        let out = sel.idx.as_mut_slice::<u16>();
        let words = bits.words();
        debug_assert!(words.len() <= VECTOR_SIZE / 64);
        let mut offsets = [0u32; VECTOR_SIZE / 64];
        let mut n = 0u32;
        for (o, &w) in offsets.iter_mut().zip(words) {
            *o = n;
            n += w.count_ones();
        }
        for (wi, &word) in words.iter().enumerate() {
            let base = (wi * 64) as u16;
            let mut w = word;
            let mut at = offsets[wi] as usize;
            while w != 0 {
                out[at] = base + w.trailing_zeros() as u16;
                at += 1;
                w &= w - 1;
            }
        }
        sel.len = n;
        sel
    }

    /// Refine an existing selection by a bitmap evaluated over the same
    /// chunk positions: keep index i when `bits` holds at sel[i]'s row.
    pub fn refine(arena: &mut MorselArena, sel: &SelVector, bits: &Bitmap) -> Self {
        let mut out = Self::with_capacity(arena, sel.len());
        let dst = out.idx.as_mut_slice::<u16>();
        let mut n = 0usize;
        for &row in sel.as_slice() {
            // Branch-free append: write always, advance on keep.
            dst[n] = row;
            n += usize::from(bits.get(row as usize));
        }
        out.len = n as u32;
        out
    }

    pub fn push(&mut self, row: u16) {
        let n = self.len as usize;
        self.idx.as_mut_slice::<u16>()[n] = row;
        self.len += 1;
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn as_slice(&self) -> &[u16] {
        &self.idx.as_slice::<u16>()[..self.len as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_bitmap_matches_naive() {
        let mut arena = MorselArena::new();
        let mut bits = Bitmap::new_in(&mut arena, 200, false);
        let rows = [0usize, 1, 63, 64, 65, 127, 128, 199];
        for &r in &rows {
            bits.set(r);
        }
        let sel = SelVector::from_bitmap(&mut arena, &bits);
        let got: Vec<usize> = sel.as_slice().iter().map(|&i| i as usize).collect();
        assert_eq!(got, rows);
    }

    #[test]
    fn refine_keeps_hits_in_order() {
        let mut arena = MorselArena::new();
        let mut bits = Bitmap::new_in(&mut arena, 100, false);
        bits.set(10);
        bits.set(30);
        let mut sel = SelVector::with_capacity(&mut arena, 3);
        sel.push(5);
        sel.push(10);
        sel.push(30);
        let refined = SelVector::refine(&mut arena, &sel, &bits);
        assert_eq!(refined.as_slice(), &[10, 30]);
    }
}
