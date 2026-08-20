//! Binary arithmetic kernels.
//!
//! The answers are the row engine's answers, conditions included: an
//! integer answer that does not fit raises, and a divisor of nought
//! raises `22012` whatever the numeric type, rather than answering the
//! infinity the hardware would. A kernel that quietly answered the
//! wrapped number or a null would be giving a wrong answer where the
//! standard asks for a condition, and which of the two a query got
//! would depend on which engine took its plan.
//!
//! What that costs is arranged so the rows that have an answer pay
//! almost nothing for the rows that do not. The compute loop stays
//! branch free and wraps, and whether any row could have raised is
//! answered by one pass over the operands that folds them into a single
//! magnitude, which is cheap and vectorizes. Only when that pass says a
//! row might have gone over does the kernel walk the selection with
//! checked arithmetic to find which one, and that walk is the only
//! place a condition is built.
//!
//! The rows outside the chunk's selection are computed and never
//! looked at, which is what lets the compute loop stay branch free, and
//! they are not looked at here either: a row the selection dropped is a
//! row the row engine never evaluated, so it has no condition to raise.

use zu_common::{Result, ZuError, gqlstatus::codes};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::sel::SelVector;
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
        // A divisor of nought is remapped to one so the hardware
        // cannot trap on a row nobody selected. A selected row never
        // gets here with one, since the check ahead of the loop raised
        // on it, so the number this answers is never read.
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
///
/// `sel` is the chunk's selection, which says which rows are the
/// query's. It is what the conditions are raised over and nothing else
/// reads it, so a caller with every row in hand passes `None`.
pub fn binary(
    arena: &mut MorselArena,
    op: BinOp,
    l: &ValueVector,
    r: &ValueVector,
    sel: Option<&SelVector>,
) -> Result<ValueVector> {
    debug_assert_eq!(l.len, r.len);
    if l.phys != r.phys {
        return Err(ZuError::InvalidArgument(format!(
            "no arithmetic kernel for {:?} vs {:?}",
            l.phys, r.phys
        )));
    }
    let len = l.len as usize;
    check(op, l, r, sel, len)?;
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

/// `22012 data exception, division by zero`, for both `/` and `%`, in
/// the words the row engine raises it with. The standard's name says
/// division, and the modulus of a zero divisor is undefined for exactly
/// the same reason.
fn divide_by_zero(op: BinOp) -> ZuError {
    let what = if matches!(op, BinOp::Mod) {
        "modulus"
    } else {
        "division"
    };
    ZuError::gql(codes::C22012, format!("{what} by zero"))
}

/// An integer answer that does not fit in an integer, which is the
/// other thing arithmetic can fail at and is not a GQL condition: the
/// standard's numeric conditions are about the value the statement
/// wrote, and this is about the width of the type holding it.
fn overflow() -> ZuError {
    ZuError::InvalidArgument("integer overflow".into())
}

/// Raises what the row engine raises for the first row of the selection
/// that has no answer, and answers `Ok` when every selected row has
/// one.
///
/// A row whose operands are not both valid is skipped, since a null
/// operand answers null rather than raising, and a row the selection
/// dropped is skipped because the row engine never evaluated it.
fn check(
    op: BinOp,
    l: &ValueVector,
    r: &ValueVector,
    sel: Option<&SelVector>,
    len: usize,
) -> Result<()> {
    let risky = match l.phys {
        PhysType::Int64 | PhysType::Interval => match op {
            // Two numbers below two to the sixty second cannot add or
            // subtract past the top, and two below two to the thirty
            // first cannot multiply past it, so one fold over the
            // operands answers for the whole chunk. It answers
            // conservatively, which is the right way round: a chunk of
            // large numbers pays for a walk that finds nothing.
            BinOp::Add | BinOp::Sub => (magnitude(l, len) | magnitude(r, len)) >= 1u64 << 62,
            BinOp::Mul => (magnitude(l, len) | magnitude(r, len)) >= 1u64 << 31,
            // Nought has no answer at all, and minus one has none for
            // the one dividend whose negation does not fit.
            BinOp::Div | BinOp::Mod => awkward_divisor(r, len),
        },
        // The approximate numbers overflow to an infinity, which is
        // what the row engine answers too, so a divisor of nought is
        // the whole of what can go wrong.
        PhysType::Float64 => matches!(op, BinOp::Div | BinOp::Mod) && zero_divisor_f64(r, len),
        _ => false,
    };
    if !risky {
        return Ok(());
    }
    let float = l.phys == PhysType::Float64;
    let visit = |i: usize| -> Result<()> {
        if !l.is_valid(i) || !r.is_valid(i) {
            return Ok(());
        }
        if float {
            if at_f64(r, i) == 0.0 {
                return Err(divide_by_zero(op));
            }
            return Ok(());
        }
        let (a, b) = (at_i64(l, i), at_i64(r, i));
        let fits = match op {
            BinOp::Add => a.checked_add(b).is_some(),
            BinOp::Sub => a.checked_sub(b).is_some(),
            BinOp::Mul => a.checked_mul(b).is_some(),
            BinOp::Div | BinOp::Mod => {
                if b == 0 {
                    return Err(divide_by_zero(op));
                }
                a.checked_div(b).is_some()
            }
        };
        if fits { Ok(()) } else { Err(overflow()) }
    };
    match sel {
        Some(sel) => {
            for &row in sel.as_slice() {
                visit(row as usize)?;
            }
        }
        None => {
            for i in 0..len {
                visit(i)?;
            }
        }
    }
    Ok(())
}

/// The bits every value in the vector fits inside, as the OR of what
/// each one takes without its sign. A dict vector answers with every
/// bit set, since the arithmetic loops refuse one anyway and this is
/// read before them.
fn magnitude(v: &ValueVector, len: usize) -> u64 {
    match v.encoding {
        VecEncoding::Flat => v.values::<i64>()[..len]
            .iter()
            .fold(0u64, |acc, x| acc | x.unsigned_abs()),
        VecEncoding::Constant => v.constant_value::<i64>().unsigned_abs(),
        VecEncoding::Dict { .. } => u64::MAX,
    }
}

/// Whether the divisor holds a value the division has no answer for.
fn awkward_divisor(v: &ValueVector, len: usize) -> bool {
    match v.encoding {
        VecEncoding::Flat => v.values::<i64>()[..len].iter().any(|&x| x == 0 || x == -1),
        VecEncoding::Constant => matches!(v.constant_value::<i64>(), 0 | -1),
        VecEncoding::Dict { .. } => true,
    }
}

/// The same question over the approximate numbers, where minus one is
/// an ordinary divisor and nought is not.
fn zero_divisor_f64(v: &ValueVector, len: usize) -> bool {
    match v.encoding {
        VecEncoding::Flat => v.values::<f64>()[..len].iter().any(|&x| x == 0.0),
        VecEncoding::Constant => v.constant_value::<f64>() == 0.0,
        VecEncoding::Dict { .. } => true,
    }
}

fn at_i64(v: &ValueVector, i: usize) -> i64 {
    match v.encoding {
        VecEncoding::Constant => v.constant_value::<i64>(),
        _ => v.values::<i64>()[i],
    }
}

fn at_f64(v: &ValueVector, i: usize) -> f64 {
    match v.encoding {
        VecEncoding::Constant => v.constant_value::<f64>(),
        _ => v.values::<f64>()[i],
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

    use crate::sel::SelVector;

    /// The condition a kernel raised, as the words a caller reads.
    /// `ValueVector` says nothing about itself, so the answer arm names
    /// what happened rather than printing what came back.
    fn raised(out: Result<ValueVector>) -> String {
        match out {
            Ok(_) => panic!("the kernel answered where it has no answer"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn add_flat_const() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 2, 3]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, 10i64, 3);
        let out = binary(&mut arena, BinOp::Add, &l, &r, None).unwrap();
        assert_eq!(out.values::<i64>(), &[11, 12, 13]);
        assert!(out.validity.is_none());
    }

    /// The condition the standard names for it, in the words the row
    /// engine uses, since a query cannot tell which engine answered it.
    #[test]
    fn div_by_zero_raises() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[10i64, 20, 30]);
        let r = ValueVector::flat_from(&mut arena, PhysType::Int64, &[2i64, 0, 3]);
        let out = binary(&mut arena, BinOp::Div, &l, &r, None);
        assert_eq!(raised(out), "22012: division by zero");
        let out = binary(&mut arena, BinOp::Mod, &l, &r, None);
        assert_eq!(raised(out), "22012: modulus by zero");
    }

    /// A row the selection dropped is a row the query never asked
    /// about, so the divisor it holds is not a condition.
    #[test]
    fn a_row_outside_the_selection_raises_nothing() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[10i64, 20, 30]);
        let r = ValueVector::flat_from(&mut arena, PhysType::Int64, &[2i64, 0, 3]);
        let mut sel = SelVector::with_capacity(&mut arena, 2);
        sel.push(0);
        sel.push(2);
        let out = binary(&mut arena, BinOp::Div, &l, &r, Some(&sel)).unwrap();
        assert_eq!(out.values::<i64>()[0], 5);
        assert_eq!(out.values::<i64>()[2], 10);
    }

    /// A null operand answers null, which is not a condition either, so
    /// the divisor under it is never read.
    #[test]
    fn a_null_operand_is_not_a_condition() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[10i64, 20, 30]);
        let mut r = ValueVector::flat_from(&mut arena, PhysType::Int64, &[2i64, 0, 3]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        r.validity = Some(valid);
        let out = binary(&mut arena, BinOp::Div, &l, &r, None).unwrap();
        assert!(!out.is_valid(1), "null in, null out");
    }

    /// An answer too wide for the type is the row engine's `integer
    /// overflow`, and the fold that finds it looks at the operands
    /// rather than at the answer, so a chunk of ordinary numbers never
    /// reaches the walk that builds the condition.
    #[test]
    fn an_integer_answer_that_does_not_fit_raises() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, i64::MAX]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, 1i64, 2);
        let out = binary(&mut arena, BinOp::Add, &l, &r, None);
        assert_eq!(raised(out), "invalid argument: integer overflow");

        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 1 << 40]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, 1i64 << 40, 2);
        let out = binary(&mut arena, BinOp::Mul, &l, &r, None);
        assert_eq!(raised(out), "invalid argument: integer overflow");

        // The one dividend a divisor of minus one has no answer for.
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[i64::MIN]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, -1i64, 1);
        let out = binary(&mut arena, BinOp::Div, &l, &r, None);
        assert_eq!(raised(out), "invalid argument: integer overflow");
    }

    /// The approximate numbers raise on a divisor of nought too, which
    /// is where a kernel that answered what the hardware answers would
    /// hand back an infinity the statement never asked for.
    #[test]
    fn a_float_divisor_of_nought_raises() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Float64, &[1.0f64, 2.0]);
        let r = ValueVector::flat_from(&mut arena, PhysType::Float64, &[2.0f64, 0.0]);
        let out = binary(&mut arena, BinOp::Div, &l, &r, None);
        assert_eq!(raised(out), "22012: division by zero");
    }

    /// Two numbers wide enough to worry the fold but whose answer fits.
    #[test]
    fn a_wide_answer_that_fits_is_an_answer() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[i64::MAX, i64::MIN]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, 0i64, 2);
        let out = binary(&mut arena, BinOp::Add, &l, &r, None).unwrap();
        assert_eq!(out.values::<i64>(), &[i64::MAX, i64::MIN]);
    }
}
