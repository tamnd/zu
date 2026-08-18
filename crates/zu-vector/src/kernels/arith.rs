//! Binary arithmetic kernels.
//!
//! Integer ops wrap; the old executor's overflow behavior is release-mode
//! wrapping too, and the differential suite referees exact semantics when
//! the new executor takes over. Division and modulo by zero clear the
//! output row's validity instead of trapping, so the kernel stays total
//! and branch cost lands only on the zero rows.

use zu_common::{Result, ZuError};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::vector::{PhysType, ValueVector, VecEncoding};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[inline(always)]
fn apply_i64(op: BinOp, a: i64, b: i64) -> i64 {
    match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Mul => a.wrapping_mul(b),
        // Divisor zero is remapped to 1 here and the row's validity is
        // cleared by the caller; the arithmetic result never escapes.
        BinOp::Div => a.wrapping_div(if b == 0 { 1 } else { b }),
        BinOp::Mod => a.wrapping_rem(if b == 0 { 1 } else { b }),
    }
}

#[inline(always)]
fn apply_f64(op: BinOp, a: f64, b: f64) -> f64 {
    match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        BinOp::Mod => a % b,
    }
}

/// Evaluate `l op r` into a new flat vector. The full vector is computed
/// regardless of selection: the loop is branch-free and cheaper than
/// gathering, and selections keep their meaning positionally.
pub fn binary(
    arena: &mut MorselArena,
    op: BinOp,
    l: &ValueVector,
    r: &ValueVector,
) -> Result<ValueVector> {
    debug_assert_eq!(l.len, r.len);
    if l.phys != r.phys {
        return Err(ZuError::InvalidArgument(format!(
            "no arithmetic kernel for {:?} vs {:?}",
            l.phys, r.phys
        )));
    }
    let len = l.len as usize;
    // Two constants make a constant, and it is worth spotting because
    // it is not a corner case: a value pinned on a level below arrives
    // here broadcast, so `outer.x * $k` is this shape on every vector
    // the level above produces. One apply and one word beats a full
    // pass, and everything downstream reads a constant the same way.
    if let (VecEncoding::Constant, VecEncoding::Constant) = (l.encoding, r.encoding)
        && matches!(
            l.phys,
            PhysType::Int64 | PhysType::Interval | PhysType::Float64
        )
    {
        let mut out = match l.phys {
            PhysType::Float64 => {
                let v = apply_f64(op, l.constant_value::<f64>(), r.constant_value::<f64>());
                ValueVector::constant(arena, l.phys, v, len)
            }
            _ => {
                let v = apply_i64(op, l.constant_value::<i64>(), r.constant_value::<i64>());
                ValueVector::constant(arena, l.phys, v, len)
            }
        };
        out.validity = merged_validity(arena, l, r, len);
        if matches!(l.phys, PhysType::Int64 | PhysType::Interval)
            && matches!(op, BinOp::Div | BinOp::Mod)
        {
            clear_zero_divisor_rows(arena, &mut out, r, len);
        }
        return Ok(out);
    }
    let mut out = ValueVector::flat_uninit(arena, l.phys, len);
    match l.phys {
        PhysType::Int64 | PhysType::Interval => {
            {
                let dst = out.values_mut::<i64>();
                match (l.encoding, r.encoding) {
                    (VecEncoding::Flat, VecEncoding::Flat) => {
                        let (a, b) = (l.values::<i64>(), r.values::<i64>());
                        for i in 0..len {
                            dst[i] = apply_i64(op, a[i], b[i]);
                        }
                    }
                    (VecEncoding::Flat, VecEncoding::Constant) => {
                        let a = l.values::<i64>();
                        let c = r.constant_value::<i64>();
                        for i in 0..len {
                            dst[i] = apply_i64(op, a[i], c);
                        }
                    }
                    (VecEncoding::Constant, VecEncoding::Flat) => {
                        let c = l.constant_value::<i64>();
                        let b = r.values::<i64>();
                        for i in 0..len {
                            dst[i] = apply_i64(op, c, b[i]);
                        }
                    }
                    _ => {
                        return Err(ZuError::InvalidArgument(
                            "arithmetic on dict vectors: materialize first".into(),
                        ));
                    }
                }
            }
            out.validity = merged_validity(arena, l, r, len);
            if matches!(op, BinOp::Div | BinOp::Mod) {
                clear_zero_divisor_rows(arena, &mut out, r, len);
            }
        }
        PhysType::Float64 => {
            {
                let dst = out.values_mut::<f64>();
                match (l.encoding, r.encoding) {
                    (VecEncoding::Flat, VecEncoding::Flat) => {
                        let (a, b) = (l.values::<f64>(), r.values::<f64>());
                        for i in 0..len {
                            dst[i] = apply_f64(op, a[i], b[i]);
                        }
                    }
                    (VecEncoding::Flat, VecEncoding::Constant) => {
                        let a = l.values::<f64>();
                        let c = r.constant_value::<f64>();
                        for i in 0..len {
                            dst[i] = apply_f64(op, a[i], c);
                        }
                    }
                    (VecEncoding::Constant, VecEncoding::Flat) => {
                        let c = l.constant_value::<f64>();
                        let b = r.values::<f64>();
                        for i in 0..len {
                            dst[i] = apply_f64(op, c, b[i]);
                        }
                    }
                    _ => {
                        return Err(ZuError::InvalidArgument(
                            "arithmetic on dict vectors: materialize first".into(),
                        ));
                    }
                }
            }
            out.validity = merged_validity(arena, l, r, len);
        }
        _ => {
            return Err(ZuError::InvalidArgument(format!(
                "no arithmetic kernel for {:?}",
                l.phys
            )));
        }
    }
    Ok(out)
}

/// Clear validity on rows whose divisor is zero. The scan re-reads the
/// divisor vector rather than tracking rows during the compute loop, so
/// the arithmetic loop stays branch-free and nothing is allocated unless
/// a zero actually occurs.
fn clear_zero_divisor_rows(
    arena: &mut MorselArena,
    out: &mut ValueVector,
    r: &ValueVector,
    len: usize,
) {
    let any_zero = match r.encoding {
        VecEncoding::Flat => r.values::<i64>().contains(&0),
        VecEncoding::Constant => r.constant_value::<i64>() == 0,
        VecEncoding::Dict { .. } => false,
    };
    if !any_zero {
        return;
    }
    let v = out
        .validity
        .get_or_insert_with(|| Bitmap::new_in(arena, len, true));
    match r.encoding {
        VecEncoding::Flat => {
            for (i, &x) in r.values::<i64>().iter().enumerate() {
                if x == 0 {
                    v.clear(i);
                }
            }
        }
        _ => {
            v.words_mut().fill(0);
        }
    }
}

/// NULL in, NULL out: the output validity is the AND of the inputs',
/// allocated only when either side actually carries one.
fn merged_validity(
    arena: &mut MorselArena,
    l: &ValueVector,
    r: &ValueVector,
    len: usize,
) -> Option<Bitmap> {
    match (&l.validity, &r.validity) {
        (None, None) => None,
        (a, b) => {
            let mut v = Bitmap::new_in(arena, len, true);
            if let Some(a) = a {
                v.and_with(a);
            }
            if let Some(b) = b {
                v.and_with(b);
            }
            Some(v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_flat_const() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 2, 3]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, 10i64, 3);
        let out = binary(&mut arena, BinOp::Add, &l, &r).unwrap();
        assert_eq!(out.values::<i64>(), &[11, 12, 13]);
        assert!(out.validity.is_none());
    }

    #[test]
    fn div_by_zero_nulls_the_row() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[10i64, 20, 30]);
        let r = ValueVector::flat_from(&mut arena, PhysType::Int64, &[2i64, 0, 3]);
        let out = binary(&mut arena, BinOp::Div, &l, &r).unwrap();
        assert_eq!(out.values::<i64>()[0], 5);
        assert_eq!(out.values::<i64>()[2], 10);
        assert!(out.is_valid(0));
        assert!(!out.is_valid(1));
        assert!(out.is_valid(2));
    }
}
