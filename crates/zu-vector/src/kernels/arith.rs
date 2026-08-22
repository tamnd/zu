//! Binary arithmetic kernels.
//!
//! The answers are the row engine's answers, conditions included: an
//! integer answer that does not fit raises `22003`, and a divisor of
//! nought raises `22012` whatever the numeric type, rather than
//! answering the infinity the hardware would. A kernel that quietly answered the
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

use crate::arena::{MorselArena, Pod};
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

/// The answer for one pair, which is what a chunk of two constants
/// needs. The loops below do not go through here: they settle the
/// operation before they start, for the reason `fill` gives.
#[inline(always)]
fn apply_i64(op: BinOp, a: i64, b: i64) -> i64 {
    match op {
        BinOp::Add => a.wrapping_add(b),
        BinOp::Sub => a.wrapping_sub(b),
        BinOp::Mul => a.wrapping_mul(b),
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

/// The compute loop over one of the three shapes a pair of vectors
/// comes in, with the operation already chosen.
///
/// `f` arrives monomorphic, so the compiler stamps one loop per
/// operation and each one holds a single instruction per row wherever
/// the hardware has one. Which operation it is has to be settled
/// outside, the way the compare kernel has always settled it: a match
/// left standing in the loop is a branch the vectorizer will not lift,
/// and it costs about seven times the throughput of the loop it is
/// standing in.
#[inline(always)]
fn fill<T: Pod, F: Fn(T, T) -> T>(
    dst: &mut [T],
    l: &ValueVector,
    r: &ValueVector,
    len: usize,
    f: F,
) -> Result<()> {
    match (l.encoding, r.encoding) {
        (VecEncoding::Flat, VecEncoding::Flat) => {
            let (a, b) = (l.values::<T>(), r.values::<T>());
            for i in 0..len {
                dst[i] = f(a[i], b[i]);
            }
        }
        (VecEncoding::Flat, VecEncoding::Constant) => {
            let a = l.values::<T>();
            let c = r.constant_value::<T>();
            for i in 0..len {
                dst[i] = f(a[i], c);
            }
        }
        (VecEncoding::Constant, VecEncoding::Flat) => {
            let c = l.constant_value::<T>();
            let b = r.values::<T>();
            for i in 0..len {
                dst[i] = f(c, b[i]);
            }
        }
        _ => {
            return Err(ZuError::InvalidArgument(
                "arithmetic on dict vectors: materialize first".into(),
            ));
        }
    }
    Ok(())
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
                match op {
                    BinOp::Add => fill(dst, l, r, len, i64::wrapping_add)?,
                    BinOp::Sub => fill(dst, l, r, len, i64::wrapping_sub)?,
                    BinOp::Mul => fill(dst, l, r, len, i64::wrapping_mul)?,
                    // A divisor of nought is remapped to one so the
                    // hardware cannot trap on a row nobody selected. A
                    // selected row never gets here with one, since the
                    // check ahead of the loop raised on it, so the
                    // number these two answer is never read.
                    BinOp::Div => fill(dst, l, r, len, |a: i64, b: i64| {
                        a.wrapping_div(if b == 0 { 1 } else { b })
                    })?,
                    BinOp::Mod => fill(dst, l, r, len, |a: i64, b: i64| {
                        a.wrapping_rem(if b == 0 { 1 } else { b })
                    })?,
                }
            }
            out.validity = merged_validity(arena, l, r, len);
        }
        PhysType::Float64 => {
            {
                let dst = out.values_mut::<f64>();
                match op {
                    BinOp::Add => fill(dst, l, r, len, |a: f64, b: f64| a + b)?,
                    BinOp::Sub => fill(dst, l, r, len, |a: f64, b: f64| a - b)?,
                    BinOp::Mul => fill(dst, l, r, len, |a: f64, b: f64| a * b)?,
                    BinOp::Div => fill(dst, l, r, len, |a: f64, b: f64| a / b)?,
                    BinOp::Mod => fill(dst, l, r, len, |a: f64, b: f64| a % b)?,
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

/// `22003 data exception, numeric value out of range`, for the other
/// thing arithmetic can fail at: an answer that does not fit the type
/// holding it.
///
/// The float half of the library has always raised this for an answer
/// that left the range a double holds, and an integer answer that left
/// the range an i64 holds is the same sentence about a different width.
/// A statement that got a bare refusal instead was told the engine
/// would not answer without being told what the standard calls it.
fn overflow() -> ZuError {
    ZuError::gql(codes::C22003, "integer overflow".to_string())
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
        // Nought and minus nought are one value to a comparison, which
        // is what the fold is asking, so both are found here.
        VecEncoding::Flat => v.values::<f64>()[..len].contains(&0.0),
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

    /// An answer too wide for the type is `22003 numeric value out of
    /// range`, the same condition the row engine raises for it, and the
    /// fold that finds it looks at the operands rather than at the
    /// answer, so a chunk of ordinary numbers never reaches the walk
    /// that builds the condition.
    #[test]
    fn an_integer_answer_that_does_not_fit_raises() {
        let mut arena = MorselArena::new();
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, i64::MAX]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, 1i64, 2);
        let out = binary(&mut arena, BinOp::Add, &l, &r, None);
        assert_eq!(raised(out), "22003: integer overflow");

        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[1i64, 1 << 40]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, 1i64 << 40, 2);
        let out = binary(&mut arena, BinOp::Mul, &l, &r, None);
        assert_eq!(raised(out), "22003: integer overflow");

        // The one dividend a divisor of minus one has no answer for.
        let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &[i64::MIN]);
        let r = ValueVector::constant(&mut arena, PhysType::Int64, -1i64, 1);
        let out = binary(&mut arena, BinOp::Div, &l, &r, None);
        assert_eq!(raised(out), "22003: integer overflow");
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

    /// The kernel settles the operation outside its loop, so the answer
    /// a chunk gets is written in five places where it used to be
    /// written in one. This walks every operation over every shape a
    /// pair of vectors comes in and compares what the kernel wrote
    /// against what one pair at a time answers, which is the check that
    /// keeps the two from drifting apart.
    #[test]
    fn every_operation_answers_what_one_pair_answers() {
        const OPS: [BinOp; 5] = [
            BinOp::Add,
            BinOp::Sub,
            BinOp::Mul,
            BinOp::Div,
            BinOp::Mod,
        ];
        let whole: [i64; 4] = [7, -30, 1, 5];
        let real: [f64; 4] = [7.5, -30.25, 1.0, 5.0];
        let mut arena = MorselArena::new();
        for op in OPS {
            let l = ValueVector::flat_from(&mut arena, PhysType::Int64, &whole);
            let r = ValueVector::flat_from(&mut arena, PhysType::Int64, &[2i64, 3, 4, 5]);
            let c = ValueVector::constant(&mut arena, PhysType::Int64, 3i64, 4);
            for (name, l, r) in [("flat flat", &l, &r), ("flat const", &l, &c)] {
                let out = binary(&mut arena, op, l, r, None).unwrap();
                for i in 0..4 {
                    let want = apply_i64(op, at_i64(l, i), at_i64(r, i));
                    assert_eq!(out.values::<i64>()[i], want, "{op:?} {name} row {i}");
                }
            }
            let out = binary(&mut arena, op, &c, &r, None).unwrap();
            for i in 0..4 {
                let want = apply_i64(op, 3, at_i64(&r, i));
                assert_eq!(out.values::<i64>()[i], want, "{op:?} const flat row {i}");
            }

            let l = ValueVector::flat_from(&mut arena, PhysType::Float64, &real);
            let r = ValueVector::flat_from(&mut arena, PhysType::Float64, &[2.0f64, 3.0, 4.0, 0.5]);
            let out = binary(&mut arena, op, &l, &r, None).unwrap();
            for (i, x) in real.iter().enumerate() {
                let want = apply_f64(op, *x, at_f64(&r, i));
                assert_eq!(out.values::<f64>()[i], want, "{op:?} float row {i}");
            }
        }
    }
}
