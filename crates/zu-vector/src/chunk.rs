//! Data chunks and factorized chunk sets (perf/02 section 1.4).
//!
//! The factorized model is exactly today's: a flat prefix chunk plus
//! unflat list-level chunks, logical multiplicity the product of the
//! level sizes, flattening lazy. Only the cell representation changed.

use crate::sel::SelVector;
use crate::vector::ValueVector;

pub struct DataChunk {
    pub vecs: Vec<ValueVector>,
    /// One selection shared by every vector in the chunk. None is
    /// identity.
    pub sel: Option<SelVector>,
    /// Row count before selection.
    pub count: u32,
    /// Some(i) pins the chunk flat at row i, the vectorized form of the
    /// old executor's cur.
    pub cur: Option<u32>,
}

impl DataChunk {
    pub fn new(vecs: Vec<ValueVector>, count: u32) -> Self {
        Self {
            vecs,
            sel: None,
            count,
            cur: None,
        }
    }

    /// Rows visible after selection and pinning.
    pub fn active_count(&self) -> usize {
        if self.cur.is_some() {
            return 1;
        }
        match &self.sel {
            Some(sel) => sel.len(),
            None => self.count as usize,
        }
    }
}

/// A factorized tuple group: chunks[0] is the flat prefix, chunks[1..]
/// are unflat list levels.
pub struct ChunkSet {
    pub chunks: Vec<DataChunk>,
}

impl ChunkSet {
    pub fn new(chunks: Vec<DataChunk>) -> Self {
        Self { chunks }
    }

    /// Logical row multiplicity: the product of the active counts of the
    /// unpinned levels.
    pub fn multiplicity(&self) -> u64 {
        self.chunks
            .iter()
            .map(|c| c.active_count() as u64)
            .product()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::MorselArena;
    use crate::vector::{PhysType, ValueVector};

    #[test]
    fn multiplicity_is_product_of_levels() {
        let mut arena = MorselArena::new();
        let flat = DataChunk::new(
            vec![ValueVector::flat_from(
                &mut arena,
                PhysType::Int64,
                &[1i64, 2],
            )],
            2,
        );
        let unflat = DataChunk::new(
            vec![ValueVector::flat_from(
                &mut arena,
                PhysType::Int64,
                &[10i64, 20, 30],
            )],
            3,
        );
        let set = ChunkSet::new(vec![flat, unflat]);
        assert_eq!(set.multiplicity(), 6);
        let mut pinned = set;
        pinned.chunks[1].cur = Some(1);
        assert_eq!(pinned.multiplicity(), 2);
    }
}
