//! GF01, the numeric functions that keep an exact argument exact: ABS,
//! CEIL, FLOOR, ROUND and SIGN.
//!
//! An integer in gives an integer out, which is the rule the row engine
//! follows and the reason these five are a kernel of their own: the
//! answer of every one of them over a whole number is a whole number,
//! and widening it to a float would lose a digit above two to the fifty
//! third for nothing the statement asked for. SIGN is the exception in
//! the other direction, since minus one, nought and one are whole
//! whatever arrived, so its answer is an integer even for a float
//! argument.
//!
//! The conditions are the row engine's, in the row engine's words. Only
//! two of the shapes here have any: the distance of the bottom integer
//! from nought is one past the top of one, and rounding to a place left
//! of the point can carry a number past the top the same way. Both are
//! found the way the arithmetic kernels find theirs, by one cheap fold
//! over the argument that a chunk of ordinary numbers passes, so the
//! loop that computes the answers stays branch free and the walk that
//! builds a condition runs only where the fold could not rule one out.

use zu_common::{Result, ZuError, gqlstatus::codes};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::sel::SelVector;
use crate::vector::{PhysType, ValueVector, VecEncoding};

/// One of the five, with the digit count ROUND was written with. A
/// second argument that is not a constant is not this op: the compiler
/// leaves that call to the row engine rather than reading a column per
/// row inside a loop that is here to avoid exactly that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MathOp {
    Abs,
    Ceil,
    Floor,
    Sign,
    Round(i64),
}

impl MathOp {
    /// The name the row engine builds its conditions with, so a message
    /// says which function had no answer whichever engine raised it.
    fn name(self) -> &'static str {
        match self {
            MathOp::Abs => "abs",
            MathOp::Ceil => "ceil",
            MathOp::Floor => "floor",
            MathOp::Sign => "sign",
            MathOp::Round(_) => "round",
        }
    }

    /// Whether an argument could leave this function with no answer.
    /// The compiler reads it to decide what may sit behind a guard: a
    /// function that cannot raise is safe anywhere, and one that can is
    /// only safe where the query did not write an order for it.
    pub fn may_raise(self) -> bool {
        match self {
            MathOp::Abs => true,
            MathOp::Round(digits) => digits != 0,
            MathOp::Ceil | MathOp::Floor | MathOp::Sign => false,
        }
    }

    /// The type the answers land in, given the type the arguments have.
    pub fn answer_type(self, arg: PhysType) -> Option<PhysType> {
        match (self, arg) {
            (MathOp::Sign, PhysType::Int64 | PhysType::Float64) => Some(PhysType::Int64),
            (_, PhysType::Int64) => Some(PhysType::Int64),
            (_, PhysType::Float64) => Some(PhysType::Float64),
            _ => None,
        }
    }
}

/// Evaluate `op(v)` into a new flat vector.
///
/// The whole vector is computed whatever the selection holds, for the
/// reason the arithmetic kernels do it: the loop is branch free and
/// cheaper than gathering, and a selection keeps its meaning by
/// position. `sel` says which rows are the query's, which is what the
/// conditions are raised over and the only thing it is read for.
pub fn unary(
    arena: &mut MorselArena,
    op: MathOp,
    v: &ValueVector,
    sel: Option<&SelVector>,
) -> Result<ValueVector> {
    let len = v.len as usize;
    if matches!(v.encoding, VecEncoding::Dict { .. }) {
        return Err(ZuError::InvalidArgument(
            "numeric functions on dict vectors: materialize first".into(),
        ));
    }
    let Some(answer) = op.answer_type(v.phys) else {
        return Err(ZuError::InvalidArgument(format!(
            "no {}() kernel for {:?}",
            op.name(),
            v.phys
        )));
    };
    check(op, v, sel, len)?;
    let mut out = ValueVector::flat_uninit(arena, answer, len);
    match (v.phys, answer) {
        (PhysType::Int64, PhysType::Int64) => {
            let dst = out.values_mut::<i64>();
            match v.encoding {
                VecEncoding::Constant => {
                    let c = exact(op, v.constant_value::<i64>());
                    dst[..len].fill(c);
                }
                _ => {
                    let src = v.values::<i64>();
                    for i in 0..len {
                        dst[i] = exact(op, src[i]);
                    }
                }
            }
        }
        (PhysType::Float64, PhysType::Int64) => {
            let dst = out.values_mut::<i64>();
            match v.encoding {
                VecEncoding::Constant => {
                    let c = sign_f64(v.constant_value::<f64>());
                    dst[..len].fill(c);
                }
                _ => {
                    let src = v.values::<f64>();
                    for i in 0..len {
                        dst[i] = sign_f64(src[i]);
                    }
                }
            }
        }
        (PhysType::Float64, PhysType::Float64) => {
            let dst = out.values_mut::<f64>();
            match v.encoding {
                VecEncoding::Constant => {
                    let c = real(op, v.constant_value::<f64>());
                    dst[..len].fill(c);
                }
                _ => {
                    let src = v.values::<f64>();
                    for i in 0..len {
                        dst[i] = real(op, src[i]);
                    }
                }
            }
        }
        _ => unreachable!("answer_type answers for these three shapes only"),
    }
    // NULL in, NULL out: the answer is null exactly where the argument
    // was, and the bitmap is copied only where the argument carries one.
    out.validity = v.validity.as_ref().map(|valid| {
        let mut copy = Bitmap::new_in(arena, len, true);
        copy.and_with(valid);
        copy
    });
    Ok(out)
}

/// The answer over a whole number, which is a whole number.
///
/// The two shapes that can have no answer wrap here rather than
/// branching, the way the arithmetic loop wraps: a row the check ahead
/// of the loop let through has an answer, and a row it did not is a row
/// nobody reads.
#[inline(always)]
fn exact(op: MathOp, x: i64) -> i64 {
    match op {
        MathOp::Abs => x.wrapping_abs(),
        MathOp::Sign => x.signum(),
        // A whole number is already at its own ceiling and its own
        // floor, and rounding one to a digit inside the fraction leaves
        // it where it is, so these answer what they were handed rather
        // than going through a float that could not hold it.
        MathOp::Ceil | MathOp::Floor => x,
        MathOp::Round(digits) if digits >= 0 => x,
        MathOp::Round(digits) => rounded_int(x, digits).unwrap_or(0),
    }
}

/// The answer over an approximate number, for the four that answer one.
#[inline(always)]
fn real(op: MathOp, x: f64) -> f64 {
    match op {
        MathOp::Abs => x.abs(),
        MathOp::Ceil => x.ceil(),
        MathOp::Floor => x.floor(),
        MathOp::Round(0) => x.round(),
        MathOp::Round(digits) => {
            let scale = 10f64.powi(digits.clamp(-308, 308) as i32);
            (x * scale).round() / scale
        }
        MathOp::Sign => unreachable!("sign answers an integer"),
    }
}

#[inline(always)]
fn sign_f64(x: f64) -> i64 {
    if x > 0.0 {
        1
    } else if x < 0.0 {
        -1
    } else {
        0
    }
}

/// An integer rounded to a place left of the decimal point, kept in the
/// integers the whole way, so a number wider than a double holds is
/// rounded exactly. This is the row engine's `rounded_int` and answers
/// what it answers.
fn rounded_int(value: i64, digits: i64) -> Option<i64> {
    let places = u32::try_from(-digits).ok()?;
    // Past nineteen digits every integer rounds to nought, and the power
    // below would overflow rather than say so.
    if places > 19 {
        return Some(0);
    }
    let factor = 10i128.checked_pow(places)?;
    let value = value as i128;
    let half = factor / 2;
    let carried = if value >= 0 {
        (value + half) / factor
    } else {
        (value - half) / factor
    };
    i64::try_from(carried.checked_mul(factor)?).ok()
}

/// Raises what the row engine raises for the first selected row that has
/// no answer, and answers `Ok` when every one of them has one.
///
/// A row whose argument is null is skipped, since a null answers null
/// rather than raising, and so is a row the selection dropped, since the
/// row engine never evaluated it.
fn check(op: MathOp, v: &ValueVector, sel: Option<&SelVector>, len: usize) -> Result<()> {
    let risky = match (v.phys, op) {
        // Every integer but the bottom one has a distance from nought
        // that fits, and the bottom one is the only value whose size
        // without its sign reaches two to the sixty third, so one fold
        // answers for the whole chunk.
        (PhysType::Int64, MathOp::Abs) => magnitude(v, len) >= 1u64 << 63,
        // Carrying a number left of the point can push it past the top,
        // and how far depends on the digit count, so the fold here is
        // the digit count itself: a chunk asked to round inside the
        // fraction cannot carry at all.
        (PhysType::Int64, MathOp::Round(digits)) => digits < 0,
        // The approximate answers are infinities where they leave the
        // range, which is what the row engine hands back for all but
        // one of these. Rounding is the one that checks, because
        // multiplying by the scale is where a finite number can leave.
        (PhysType::Float64, MathOp::Round(digits)) => digits != 0,
        _ => false,
    };
    if !risky {
        return Ok(());
    }
    let visit = |i: usize| -> Result<()> {
        if !v.is_valid(i) {
            return Ok(());
        }
        match v.phys {
            PhysType::Int64 => {
                let x = at_i64(v, i);
                match op {
                    MathOp::Abs => x.checked_abs().map(|_| ()).ok_or_else(|| {
                        out_of_range(op, format!("of {x} is one past the top of an integer"))
                    }),
                    MathOp::Round(digits) => rounded_int(x, digits).map(|_| ()).ok_or_else(|| {
                        out_of_range(op, format!("of {x} to {digits} digits does not fit"))
                    }),
                    _ => Ok(()),
                }
            }
            _ => {
                let x = at_f64(v, i);
                let answer = real(op, x);
                if answer.is_finite() || !x.is_finite() {
                    Ok(())
                } else {
                    Err(out_of_range(
                        op,
                        format!("of {x} is outside the range of a float"),
                    ))
                }
            }
        }
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

/// `22003 data exception, numeric value out of range`, named the way the
/// row engine names it.
fn out_of_range(op: MathOp, detail: String) -> ZuError {
    ZuError::gql(codes::C22003, format!("{}() {detail}", op.name()))
}

/// The bits every value in the vector fits inside, as the OR of what
/// each one takes without its sign.
fn magnitude(v: &ValueVector, len: usize) -> u64 {
    match v.encoding {
        VecEncoding::Constant => v.constant_value::<i64>().unsigned_abs(),
        _ => v.values::<i64>()[..len]
            .iter()
            .fold(0u64, |acc, x| acc | x.unsigned_abs()),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The condition a kernel raised, as the words a caller reads.
    fn raised(out: Result<ValueVector>) -> String {
        match out {
            Ok(_) => panic!("the kernel answered where it has no answer"),
            Err(err) => err.to_string(),
        }
    }

    fn ints(arena: &mut MorselArena, vals: &[i64]) -> ValueVector {
        ValueVector::flat_from(arena, PhysType::Int64, vals)
    }

    fn floats(arena: &mut MorselArena, vals: &[f64]) -> ValueVector {
        ValueVector::flat_from(arena, PhysType::Float64, vals)
    }

    /// An integer argument keeps its type through all five, which is
    /// what stops a number above two to the fifty third losing a digit
    /// to a float nobody asked for.
    #[test]
    fn an_exact_argument_stays_exact() {
        let mut arena = MorselArena::new();
        let big = 9_007_199_254_740_993i64;
        let v = ints(&mut arena, &[-3, 0, big]);
        for op in [MathOp::Ceil, MathOp::Floor, MathOp::Round(0)] {
            let out = unary(&mut arena, op, &v, None).unwrap();
            assert_eq!(out.phys, PhysType::Int64);
            assert_eq!(out.values::<i64>(), &[-3, 0, big]);
        }
        let out = unary(&mut arena, MathOp::Abs, &v, None).unwrap();
        assert_eq!(out.values::<i64>(), &[3, 0, big]);
        let out = unary(&mut arena, MathOp::Sign, &v, None).unwrap();
        assert_eq!(out.values::<i64>(), &[-1, 0, 1]);
    }

    /// The sign of an approximate number is an exact one, so this is
    /// the one shape whose answer is not the type of its argument.
    #[test]
    fn the_sign_of_a_float_is_an_integer() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[-0.5, 0.0, 2.5]);
        let out = unary(&mut arena, MathOp::Sign, &v, None).unwrap();
        assert_eq!(out.phys, PhysType::Int64);
        assert_eq!(out.values::<i64>(), &[-1, 0, 1]);
    }

    /// Halves go away from nought, which is the rule SQL rounds by and
    /// the one a reader expects writing it out by hand.
    #[test]
    fn halves_round_away_from_nought() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[2.5, -2.5, 1.4999]);
        let out = unary(&mut arena, MathOp::Round(0), &v, None).unwrap();
        assert_eq!(out.values::<f64>(), &[3.0, -3.0, 1.0]);
    }

    /// A negative digit count rounds tens and hundreds, and over the
    /// integers it never goes through a float, so the answer is exact
    /// however wide the number is.
    #[test]
    fn a_negative_digit_count_rounds_left_of_the_point() {
        let mut arena = MorselArena::new();
        let v = ints(&mut arena, &[150, -150, 149]);
        let out = unary(&mut arena, MathOp::Round(-2), &v, None).unwrap();
        assert_eq!(out.values::<i64>(), &[200, -200, 100]);

        let v = floats(&mut arena, &[1.234, 1.236]);
        let out = unary(&mut arena, MathOp::Round(2), &v, None).unwrap();
        assert_eq!(out.values::<f64>(), &[1.23, 1.24]);
    }

    /// The distance of the bottom integer from nought is one past the
    /// top of one, which is the row engine's condition and its words.
    #[test]
    fn the_bottom_integer_has_no_distance_from_nought() {
        let mut arena = MorselArena::new();
        let v = ints(&mut arena, &[1, i64::MIN]);
        let out = unary(&mut arena, MathOp::Abs, &v, None);
        assert_eq!(
            raised(out),
            "22003: abs() of -9223372036854775808 is one past the top of an integer"
        );
    }

    /// Carrying a number left of the point can push it past the top the
    /// same way.
    #[test]
    fn a_carry_past_the_top_of_an_integer_raises() {
        let mut arena = MorselArena::new();
        let v = ints(&mut arena, &[i64::MAX]);
        let out = unary(&mut arena, MathOp::Round(-1), &v, None);
        assert_eq!(
            raised(out),
            "22003: round() of 9223372036854775807 to -1 digits does not fit"
        );
    }

    /// A row the selection dropped is a row the query never asked
    /// about, so the argument it holds is not a condition.
    #[test]
    fn a_row_outside_the_selection_raises_nothing() {
        let mut arena = MorselArena::new();
        let v = ints(&mut arena, &[7, i64::MIN, 9]);
        let mut sel = SelVector::with_capacity(&mut arena, 2);
        sel.push(0);
        sel.push(2);
        let out = unary(&mut arena, MathOp::Abs, &v, Some(&sel)).unwrap();
        assert_eq!(out.values::<i64>()[0], 7);
        assert_eq!(out.values::<i64>()[2], 9);
    }

    /// A null argument answers null rather than raising, so the value
    /// sitting under it is never read.
    #[test]
    fn a_null_argument_is_not_a_condition() {
        let mut arena = MorselArena::new();
        let mut v = ints(&mut arena, &[7, i64::MIN, 9]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        let out = unary(&mut arena, MathOp::Abs, &v, None).unwrap();
        assert!(!out.is_valid(1), "null in, null out");
        assert!(out.is_valid(0) && out.is_valid(2));
    }

    /// An argument that was already infinite is let through, because an
    /// engine that raises there is answering a question about IEEE
    /// arithmetic with a condition the statement did not cause.
    #[test]
    fn an_infinite_argument_is_not_a_condition() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[f64::INFINITY]);
        let out = unary(&mut arena, MathOp::Round(2), &v, None).unwrap();
        assert!(out.values::<f64>()[0].is_infinite());
    }

    /// A finite number the scale pushes out of the range is the one
    /// place rounding an approximate number raises.
    #[test]
    fn a_finite_number_rounded_out_of_range_raises() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[f64::MAX]);
        let out = unary(&mut arena, MathOp::Round(2), &v, None);
        assert!(
            raised(out).starts_with("22003: round() of "),
            "the row engine's condition"
        );
    }
}
