//! Gather and compaction kernels.
//!
//! Gather is the late-materialization primitive: fetch rows named by an
//! index vector out of a decoded column. Compaction turns a selected
//! vector flat at pipeline breaks when selectivity has dropped far
//! enough that downstream full-vector loops would mostly touch dead
//! rows (perf/02 puts the crossover at 1/8).

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::sel::SelVector;
use crate::str::StrView;
use crate::vector::{ValueVector, VecEncoding};

/// out[i] = src[idx[i]]. The loop is unrolled by four so the loads
/// overlap; indices are u32 because gather sources (decoded groups) are
/// bounded by group size, not vector size.
pub fn gather_u64(src: &[u64], idx: &[u32], out: &mut [u64]) {
    debug_assert_eq!(idx.len(), out.len());
    // Fours as arrays rather than as slices, which is the same unroll
    // with the length in the type, so the four stores need no bounds
    // check to prove.
    let (fours, tail) = idx.as_chunks::<4>();
    let (outs, out_tail) = out.as_chunks_mut::<4>();
    for (i4, o4) in fours.iter().zip(outs) {
        o4[0] = src[i4[0] as usize];
        o4[1] = src[i4[1] as usize];
        o4[2] = src[i4[2] as usize];
        o4[3] = src[i4[3] as usize];
    }
    for (o, &i) in out_tail.iter_mut().zip(tail) {
        *o = src[i as usize];
    }
}

/// Compact a selected vector to a flat unselected one. Word-addressed
/// types move 8 or 16 bytes per surviving row; validity compacts with
/// the rows. Constant vectors just shrink their logical length.
pub fn compact(arena: &mut MorselArena, v: &ValueVector, sel: &SelVector) -> ValueVector {
    let n = sel.len();
    match v.encoding {
        VecEncoding::Constant => {
            let mut out = ValueVector::flat_uninit(arena, v.phys, 0);
            out.encoding = VecEncoding::Constant;
            out.data = {
                let mut d = arena.alloc(v.phys.width().expect("word type"), 8);
                d.as_mut_slice::<u8>()
                    .copy_from_slice(v.data.as_slice::<u8>());
                d
            };
            out.len = n as u32;
            out
        }
        VecEncoding::Flat => {
            let mut out = ValueVector::flat_uninit(arena, v.phys, n);
            match v.phys.width() {
                Some(8) => {
                    let src = v.data.as_slice::<u64>();
                    let dst = out.values_mut::<u64>();
                    for (o, &row) in dst.iter_mut().zip(sel.as_slice()) {
                        *o = src[row as usize];
                    }
                }
                Some(16) => {
                    let src = v.data.as_slice::<StrView>();
                    let dst = out.values_mut::<StrView>();
                    for (o, &row) in dst.iter_mut().zip(sel.as_slice()) {
                        *o = src[row as usize];
                    }
                }
                _ => unreachable!("compact on {:?}", v.phys),
            }
            if let Some(valid) = &v.validity {
                let mut vout = Bitmap::new_in(arena, n, true);
                for (i, &row) in sel.as_slice().iter().enumerate() {
                    if !valid.get(row as usize) {
                        vout.clear(i);
                    }
                }
                out.validity = Some(vout);
            }
            out.aux = clone_aux(&v.aux);
            out
        }
        VecEncoding::Dict { codes_width: 2 } => {
            let src = v.codes_u16();
            let mut data = arena.alloc_of::<u16>(n);
            {
                let dst = data.as_mut_slice::<u16>();
                for (o, &row) in dst.iter_mut().zip(sel.as_slice()) {
                    *o = src[row as usize];
                }
            }
            let mut out = ValueVector {
                phys: v.phys,
                encoding: v.encoding,
                data,
                validity: None,
                aux: clone_aux(&v.aux),
                len: n as u32,
            };
            if let Some(valid) = &v.validity {
                let mut vout = Bitmap::new_in(arena, n, true);
                for (i, &row) in sel.as_slice().iter().enumerate() {
                    if !valid.get(row as usize) {
                        vout.clear(i);
                    }
                }
                out.validity = Some(vout);
            }
            out
        }
        VecEncoding::Dict { .. } => unreachable!("only u16 codes exist today"),
    }
}

/// Aux data is shared, not copied: buffers and dictionaries are Arcs.
fn clone_aux(aux: &crate::vector::Aux) -> crate::vector::Aux {
    use crate::vector::Aux;
    match aux {
        Aux::None => Aux::None,
        Aux::Str(b) => Aux::Str(std::sync::Arc::clone(b)),
        Aux::Dict(d) => Aux::Dict(std::sync::Arc::clone(d)),
        Aux::List(_) => unimplemented!("list compaction lands with the list operators"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::PhysType;

    #[test]
    fn gather_matches_indexing() {
        let src: Vec<u64> = (0..100).map(|i| i * 7).collect();
        let idx = [0u32, 99, 50, 1, 2, 98, 3];
        let mut out = [0u64; 7];
        gather_u64(&src, &idx, &mut out);
        for (o, &i) in out.iter().zip(&idx) {
            assert_eq!(*o, src[i as usize]);
        }
    }

    #[test]
    fn compact_flat_i64() {
        let mut arena = MorselArena::new();
        let mut v = ValueVector::flat_from(&mut arena, PhysType::Int64, &[10i64, 20, 30, 40]);
        let mut valid = Bitmap::new_in(&mut arena, 4, true);
        valid.clear(3);
        v.validity = Some(valid);
        let mut sel = SelVector::with_capacity(&mut arena, 2);
        sel.push(1);
        sel.push(3);
        let out = compact(&mut arena, &v, &sel);
        assert_eq!(out.values::<i64>(), &[20, 40]);
        assert!(out.is_valid(0));
        assert!(!out.is_valid(1));
    }
}
