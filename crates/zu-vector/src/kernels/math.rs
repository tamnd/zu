//! The numeric functions of one argument, GF01 to GF03.
//!
//! They come in two halves and the halves differ in what they answer.
//! ABS, CEIL, FLOOR, ROUND and SIGN keep an exact argument exact, which
//! is the rule the row engine follows: the answer of every one of them
//! over a whole number is a whole number, and widening it to a float
//! would lose a digit above two to the fifty third for nothing the
//! statement asked for. SIGN is the exception in the other direction,
//! since minus one, nought and one are whole whatever arrived, so its
//! answer is an integer even for a float argument. The rest, the root,
//! the exponential, the logarithms and the angles, answer a float
//! whatever arrived, because the answer of a root or a logarithm is
//! irrational for all but a handful of arguments and a type that
//! changed with the value would be a type nothing could be planned
//! against.
//!
//! The conditions are the row engine's, in the row engine's words, and
//! finding them is the whole of what makes this more than a loop. They
//! are found the way the arithmetic kernels find theirs, by one cheap
//! fold over the argument that a chunk of ordinary numbers passes, so
//! the loop that computes the answers stays branch free and the walk
//! that builds a condition runs only where the fold could not rule one
//! out. The fold the exact half wants is how many bits the widest value
//! takes, and the fold the approximate half wants is the lowest and
//! highest value the column holds, since every one of those conditions
//! is about where the argument sits: below nought for a root, at or
//! below it for a logarithm, outside minus one to one for an inverse
//! sine, and far enough out for an exponential or a reading in degrees
//! to leave the range a double holds. The cotangent is the one that no
//! fold rules out, its condition being about the sine of the argument
//! rather than the argument, so it is walked whatever the column holds.

use zu_common::{Result, ZuError, gqlstatus::codes};

use crate::arena::MorselArena;
use crate::bitmap::Bitmap;
use crate::sel::SelVector;
use crate::vector::{PhysType, ValueVector, VecEncoding};

/// One of the numeric functions of one argument, with the digit count
/// ROUND was written with. A second argument that is not a constant is
/// not this op: the compiler leaves that call to the row engine rather
/// than reading a column per row inside a loop that is here to avoid
/// exactly that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MathOp {
    Abs,
    Ceil,
    Floor,
    Sign,
    Round(i64),
    Sqrt,
    Exp,
    Ln,
    Log10,
    Sin,
    Cos,
    Tan,
    Cot,
    Asin,
    Acos,
    Atan,
    Degrees,
    Radians,
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
            MathOp::Sqrt => "sqrt",
            MathOp::Exp => "exp",
            MathOp::Ln => "ln",
            MathOp::Log10 => "log10",
            MathOp::Sin => "sin",
            MathOp::Cos => "cos",
            MathOp::Tan => "tan",
            MathOp::Cot => "cot",
            MathOp::Asin => "asin",
            MathOp::Acos => "acos",
            MathOp::Atan => "atan",
            MathOp::Degrees => "degrees",
            MathOp::Radians => "radians",
        }
    }

    /// Whether the answer is a float whatever the argument was, which
    /// is the half of these whose answer is irrational for all but a
    /// handful of arguments.
    fn is_real(self) -> bool {
        !matches!(
            self,
            MathOp::Abs | MathOp::Ceil | MathOp::Floor | MathOp::Sign | MathOp::Round(_)
        )
    }

    /// Whether an argument could leave this function with no answer.
    /// The compiler reads it to decide what may sit behind a guard: a
    /// function that cannot raise is safe anywhere, and one that can is
    /// only safe where the query did not write an order for it.
    pub fn may_raise(self) -> bool {
        match self {
            // The distance of the bottom integer from nought is one
            // past the top of one, and a carry left of the point can
            // push a number past the top the same way.
            MathOp::Abs => true,
            MathOp::Round(digits) => digits != 0,
            MathOp::Ceil | MathOp::Floor | MathOp::Sign => false,
            // A sine, a cosine and a tangent are answers for every
            // number there is, an inverse tangent is bounded, and
            // reading a number as radians divides it by about fifty
            // seven, so none of these five can be asked for a number
            // nobody has.
            MathOp::Sin | MathOp::Cos | MathOp::Tan | MathOp::Atan | MathOp::Radians => false,
            _ => true,
        }
    }

    /// The type the answers land in, given the type the arguments have.
    pub fn answer_type(self, arg: PhysType) -> Option<PhysType> {
        match (self, arg) {
            (_, PhysType::Int64 | PhysType::Float64) if self.is_real() => Some(PhysType::Float64),
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
        // A whole number through a root or an angle, which is the one
        // shape where the answer is wider than what arrived.
        (PhysType::Int64, PhysType::Float64) => {
            let dst = out.values_mut::<f64>();
            match v.encoding {
                VecEncoding::Constant => {
                    let c = real(op, v.constant_value::<i64>() as f64);
                    dst[..len].fill(c);
                }
                _ => {
                    let src = v.values::<i64>();
                    for i in 0..len {
                        dst[i] = real(op, src[i] as f64);
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
        _ => unreachable!("answer_type answers for these four shapes only"),
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

/// One of the numeric functions over two numbers: POWER, LOG and MOD
/// under its function spelling.
///
/// MOD is here rather than on the arithmetic kernel it shares its
/// answers with because the two spellings do not share their words: the
/// operator reports a remainder that does not fit the way the operator
/// does, and the function reports it as the function. A statement
/// cannot tell which engine answered it, so it must not be able to tell
/// which kernel did either.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MathPair {
    Power,
    Log,
    Mod,
}

impl MathPair {
    fn name(self) -> &'static str {
        match self {
            MathPair::Power => "power",
            MathPair::Log => "log",
            MathPair::Mod => "mod",
        }
    }

    /// The type the answers land in. Both arguments have to be the one
    /// type already, the compiler having moved a written number into
    /// the column's type before it gets here.
    pub fn answer_type(self, l: PhysType, r: PhysType) -> Option<PhysType> {
        if l != r {
            return None;
        }
        match (self, l) {
            // A remainder of two whole numbers is a whole number, which
            // is the operator's rule and so the function's.
            (MathPair::Mod, PhysType::Int64) => Some(PhysType::Int64),
            (_, PhysType::Int64 | PhysType::Float64) => Some(PhysType::Float64),
            _ => None,
        }
    }
}

/// Evaluate `op(l, r)` into a new flat vector.
///
/// These are the expensive ones, a power and a logarithm each costing
/// tens of instructions, so the order here is the other way round from
/// the kernel above: the answers are computed first and the conditions
/// are read off the answers. A power that left the range is an infinity
/// sitting in the output, and finding it is a pass over the numbers the
/// loop just wrote rather than a second power per row.
pub fn pair(
    arena: &mut MorselArena,
    op: MathPair,
    l: &ValueVector,
    r: &ValueVector,
    sel: Option<&SelVector>,
) -> Result<ValueVector> {
    debug_assert_eq!(l.len, r.len);
    if matches!(l.encoding, VecEncoding::Dict { .. })
        || matches!(r.encoding, VecEncoding::Dict { .. })
    {
        return Err(ZuError::InvalidArgument(
            "numeric functions on dict vectors: materialize first".into(),
        ));
    }
    let Some(answer) = op.answer_type(l.phys, r.phys) else {
        return Err(ZuError::InvalidArgument(format!(
            "no {}() kernel for {:?} and {:?}",
            op.name(),
            l.phys,
            r.phys
        )));
    };
    let len = l.len as usize;
    let mut out = ValueVector::flat_uninit(arena, answer, len);
    match answer {
        PhysType::Int64 => {
            let dst = out.values_mut::<i64>();
            for (i, slot) in dst[..len].iter_mut().enumerate() {
                let (x, y) = (at_i64(l, i), at_i64(r, i));
                // A divisor of nought is remapped to one so the
                // hardware cannot trap on a row nobody selected, which
                // is what the arithmetic kernel does and for the same
                // reason.
                *slot = x.wrapping_rem(if y == 0 { 1 } else { y });
            }
        }
        _ => {
            let dst = out.values_mut::<f64>();
            for (i, slot) in dst[..len].iter_mut().enumerate() {
                *slot = apply_pair(op, arg_f64(l, i), arg_f64(r, i));
            }
        }
    }
    verify_pair(op, l, r, &out, sel, len)?;
    // NULL in, NULL out, which for two arguments is null where either
    // of them was.
    out.validity = merged_validity(arena, l, r, len);
    Ok(out)
}

/// The answer over two approximate numbers, computed for every row
/// whatever the conditions say, since nothing here traps.
#[inline(always)]
fn apply_pair(op: MathPair, x: f64, y: f64) -> f64 {
    match op {
        MathPair::Power => x.powf(y),
        // LOG takes the base first and the number second, which is the
        // order ISO 20.22 writes it in.
        MathPair::Log => y.log(x),
        MathPair::Mod => x % y,
    }
}

/// Raises what the row engine raises for the first selected row with no
/// answer, reading the answers the loop already wrote.
fn verify_pair(
    op: MathPair,
    l: &ValueVector,
    r: &ValueVector,
    out: &ValueVector,
    sel: Option<&SelVector>,
    len: usize,
) -> Result<()> {
    if !risky_pair(op, l, r, out, len) {
        return Ok(());
    }
    let visit = |i: usize| -> Result<()> {
        if !l.is_valid(i) || !r.is_valid(i) {
            return Ok(());
        }
        if out.phys == PhysType::Int64 {
            let (x, y) = (at_i64(l, i), at_i64(r, i));
            if y == 0 {
                return Err(ZuError::gql(codes::C22012, "division by zero".to_string()));
            }
            return x.checked_rem(y).map(|_| ()).ok_or_else(|| {
                out_of_range(op.name(), format!("of {x} and {y} does not fit an integer"))
            });
        }
        one_pair(op, arg_f64(l, i), arg_f64(r, i), out.values::<f64>()[i])
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

/// Whether the folds leave any row that could have no answer.
fn risky_pair(
    op: MathPair,
    l: &ValueVector,
    r: &ValueVector,
    out: &ValueVector,
    len: usize,
) -> bool {
    if out.phys == PhysType::Int64 {
        // A remainder is a condition where the divisor is nought, and
        // the one pair of whole numbers whose remainder does not fit is
        // the bottom integer by minus one, so the divisor's own span
        // answers for the chunk.
        let (lo, hi) = span(r, len);
        return (lo <= 0.0 && hi >= 0.0) || (lo <= -1.0 && hi >= -1.0);
    }
    // An answer that left the range is an infinity in the output, and
    // finding one is a pass over what the loop wrote. It is the only
    // fold POWER has, its other conditions being about the pair rather
    // than about either column.
    if !all_finite(out, len) {
        return true;
    }
    let (lo, hi) = span(l, len);
    match op {
        // Nought to a negative power and a negative number to a
        // fraction are the two the standard names, and both need a base
        // that is not above nought.
        MathPair::Power => lo <= 0.0,
        // A base is a base when it is above nought and is not one, so a
        // column entirely above one, or entirely between nought and
        // one, holds no base that is not. The number the logarithm is
        // taken of has to be above nought as well.
        MathPair::Log => {
            let base = lo > 1.0 || (lo > 0.0 && hi < 1.0);
            !base || span(r, len).0 <= 0.0
        }
        MathPair::Mod => {
            let (lo, hi) = span(r, len);
            lo <= 0.0 && hi >= 0.0
        }
    }
}

/// What one row of the two argument functions raises, or `Ok` where it
/// has an answer, in the row engine's words.
fn one_pair(op: MathPair, x: f64, y: f64, answer: f64) -> Result<()> {
    match op {
        MathPair::Mod if y == 0.0 => {
            return Err(ZuError::gql(codes::C22012, "division by zero".to_string()));
        }
        MathPair::Power if x == 0.0 && y < 0.0 => {
            return Err(ZuError::gql(
                codes::C2201F,
                "power() has no answer for nought raised to a negative power".to_string(),
            ));
        }
        MathPair::Power if x < 0.0 && y.fract() != 0.0 && y.is_finite() => {
            return Err(ZuError::gql(
                codes::C2201F,
                format!("power() has no answer for {x} raised to {y}, which is not whole"),
            ));
        }
        MathPair::Log if x <= 0.0 || x == 1.0 => {
            return Err(ZuError::gql(
                codes::C2201E,
                format!("log() has no answer in base {x}"),
            ));
        }
        MathPair::Log if y <= 0.0 => {
            return Err(ZuError::gql(
                codes::C2201E,
                format!("log() has no answer for {y}, which is not above nought"),
            ));
        }
        _ => {}
    }
    if answer.is_finite() || !x.is_finite() || !y.is_finite() {
        Ok(())
    } else {
        Err(out_of_range(
            op.name(),
            format!("of {x} and {y} is outside the range of a float"),
        ))
    }
}

/// Whether every answer in the output is a number a double holds.
fn all_finite(out: &ValueVector, len: usize) -> bool {
    out.values::<f64>()[..len].iter().all(|x| x.is_finite())
}

/// The validity of an answer over two arguments, which is null where
/// either of them was null and carries no bitmap where neither did.
fn merged_validity(
    arena: &mut MorselArena,
    l: &ValueVector,
    r: &ValueVector,
    len: usize,
) -> Option<Bitmap> {
    if l.validity.is_none() && r.validity.is_none() {
        return None;
    }
    let mut copy = Bitmap::new_in(arena, len, true);
    if let Some(valid) = l.validity.as_ref() {
        copy.and_with(valid);
    }
    if let Some(valid) = r.validity.as_ref() {
        copy.and_with(valid);
    }
    Some(copy)
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
        _ => unreachable!("the approximate half answers a float"),
    }
}

/// The answer over an approximate number, which is every one of these
/// but the sign.
///
/// Nothing here branches on the value: a row the check ahead of the
/// loop let through has an answer, and where it did not the hardware
/// hands back an infinity or a NaN that nobody reads.
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
        MathOp::Sqrt => x.sqrt(),
        MathOp::Exp => x.exp(),
        MathOp::Ln => x.ln(),
        MathOp::Log10 => x.log10(),
        MathOp::Sin => x.sin(),
        MathOp::Cos => x.cos(),
        MathOp::Tan => x.tan(),
        // The cotangent is the cosine over the sine, which is what the
        // row engine computes and why a sine of nought is a division by
        // nought there rather than a condition of its own.
        MathOp::Cot => x.cos() / x.sin(),
        MathOp::Asin => x.asin(),
        MathOp::Acos => x.acos(),
        MathOp::Atan => x.atan(),
        MathOp::Degrees => x.to_degrees(),
        MathOp::Radians => x.to_radians(),
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
    if !risky(op, v, len) {
        return Ok(());
    }
    let visit = |i: usize| -> Result<()> {
        if !v.is_valid(i) {
            return Ok(());
        }
        match v.phys {
            PhysType::Int64 if !op.is_real() => {
                let x = at_i64(v, i);
                match op {
                    MathOp::Abs => x.checked_abs().map(|_| ()).ok_or_else(|| {
                        out_of_range(
                            op.name(),
                            format!("of {x} is one past the top of an integer"),
                        )
                    }),
                    MathOp::Round(digits) => rounded_int(x, digits).map(|_| ()).ok_or_else(|| {
                        out_of_range(op.name(), format!("of {x} to {digits} digits does not fit"))
                    }),
                    _ => Ok(()),
                }
            }
            _ => one_real(op, arg_f64(v, i)),
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

/// Whether the fold over the column leaves any row that could have no
/// answer. False here is the ordinary case and means the loop below
/// runs with nothing looked at twice.
fn risky(op: MathOp, v: &ValueVector, len: usize) -> bool {
    if !op.may_raise() {
        return false;
    }
    if op.is_real() {
        let (lo, hi) = span(v, len);
        return match op {
            MathOp::Sqrt => lo < 0.0,
            MathOp::Ln | MathOp::Log10 => lo <= 0.0,
            MathOp::Asin | MathOp::Acos => lo < -1.0 || hi > 1.0,
            // The exponential passes the top of a double a little above
            // seven hundred and nine, and nowhere below it.
            MathOp::Exp => hi > 709.0,
            // Reading a number as degrees multiplies it by about fifty
            // seven, so only a number within that factor of the top can
            // leave the range.
            MathOp::Degrees => lo <= -3.0e306 || hi >= 3.0e306,
            // The cotangent's condition is about the sine of the
            // argument and not the argument, and no fold over a column
            // of numbers says where the sine lands, so this one is
            // walked whatever it holds.
            _ => true,
        };
    }
    match (v.phys, op) {
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
    }
}

/// What one row of the approximate half raises, or `Ok` where it has an
/// answer. These are the row engine's conditions and the row engine's
/// words, so a statement cannot tell which engine answered it.
fn one_real(op: MathOp, x: f64) -> Result<()> {
    match op {
        // ISO 20.22 defines the square root as the power of one half,
        // so a negative argument is the power function's condition
        // rather than a condition of its own.
        MathOp::Sqrt if x < 0.0 => {
            return Err(ZuError::gql(
                codes::C2201F,
                format!("sqrt() has no answer for {x}, which is below nought"),
            ));
        }
        MathOp::Ln | MathOp::Log10 if x <= 0.0 => {
            return Err(ZuError::gql(
                codes::C2201E,
                format!(
                    "{}() has no answer for {x}, which is not above nought",
                    op.name()
                ),
            ));
        }
        MathOp::Cot if x.sin() == 0.0 => {
            return Err(ZuError::gql(
                codes::C22012,
                format!("cot() has no answer for {x}, where the sine is nought"),
            ));
        }
        MathOp::Asin | MathOp::Acos if !(-1.0..=1.0).contains(&x) && x.is_finite() => {
            return Err(out_of_range(
                op.name(),
                format!("has no answer for {x}, which is outside minus one to one"),
            ));
        }
        _ => {}
    }
    let answer = real(op, x);
    if answer.is_finite() || !x.is_finite() {
        Ok(())
    } else {
        Err(out_of_range(
            op.name(),
            format!("of {x} is outside the range of a float"),
        ))
    }
}

/// `22003 data exception, numeric value out of range`, named the way the
/// row engine names it.
fn out_of_range(name: &str, detail: String) -> ZuError {
    ZuError::gql(codes::C22003, format!("{name}() {detail}"))
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

/// The lowest and highest value the column holds, which is the fold
/// every condition of the approximate half is answered by. A NaN is
/// left out of both, since `min` and `max` over a double skip one, and
/// leaving it out is right: a function of a NaN is a NaN and the row
/// engine raises nothing for it.
fn span(v: &ValueVector, len: usize) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    let mut fold = |x: f64| {
        lo = lo.min(x);
        hi = hi.max(x);
    };
    match (v.encoding, v.phys) {
        (VecEncoding::Constant, PhysType::Int64) => fold(v.constant_value::<i64>() as f64),
        (VecEncoding::Constant, _) => fold(v.constant_value::<f64>()),
        (_, PhysType::Int64) => {
            for &x in &v.values::<i64>()[..len] {
                fold(x as f64);
            }
        }
        _ => {
            for &x in &v.values::<f64>()[..len] {
                fold(x);
            }
        }
    }
    (lo, hi)
}

/// One row read as the approximate half reads it, whichever of the two
/// types the column holds.
fn arg_f64(v: &ValueVector, i: usize) -> f64 {
    match v.phys {
        PhysType::Int64 => at_i64(v, i) as f64,
        _ => match v.encoding {
            VecEncoding::Constant => v.constant_value::<f64>(),
            _ => v.values::<f64>()[i],
        },
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

    /// A root, a logarithm or an angle answers a float whatever
    /// arrived, so a column of whole numbers comes back wider than it
    /// went in.
    #[test]
    fn the_approximate_half_answers_a_float_whatever_arrived() {
        let mut arena = MorselArena::new();
        let v = ints(&mut arena, &[0, 1, 4]);
        let out = unary(&mut arena, MathOp::Sqrt, &v, None).unwrap();
        assert_eq!(out.phys, PhysType::Float64);
        assert_eq!(out.values::<f64>(), &[0.0, 1.0, 2.0]);

        let out = unary(&mut arena, MathOp::Cos, &v, None).unwrap();
        assert_eq!(out.values::<f64>()[0], 1.0);

        let v = floats(&mut arena, &[100.0, 1000.0]);
        let out = unary(&mut arena, MathOp::Log10, &v, None).unwrap();
        assert_eq!(out.values::<f64>(), &[2.0, 3.0]);
    }

    /// The five that have an answer for every number there is say so,
    /// which is what lets one of them sit behind a guard.
    #[test]
    fn the_angles_and_the_readings_cannot_raise() {
        for op in [
            MathOp::Sin,
            MathOp::Cos,
            MathOp::Tan,
            MathOp::Atan,
            MathOp::Radians,
            MathOp::Ceil,
            MathOp::Floor,
            MathOp::Sign,
            MathOp::Round(0),
        ] {
            assert!(!op.may_raise(), "{op:?} says it can raise");
        }
        for op in [
            MathOp::Sqrt,
            MathOp::Exp,
            MathOp::Ln,
            MathOp::Log10,
            MathOp::Cot,
            MathOp::Asin,
            MathOp::Acos,
            MathOp::Degrees,
            MathOp::Abs,
            MathOp::Round(1),
        ] {
            assert!(op.may_raise(), "{op:?} says it cannot raise");
        }
    }

    /// A root of a negative number and a logarithm of nought have no
    /// answer at all, which the standard gives conditions of their own
    /// rather than leaving to IEEE arithmetic.
    #[test]
    fn a_root_or_a_logarithm_outside_its_domain_raises() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[4.0, -1.0]);
        assert_eq!(
            raised(unary(&mut arena, MathOp::Sqrt, &v, None)),
            "2201F: sqrt() has no answer for -1, which is below nought"
        );

        let v = floats(&mut arena, &[1.0, 0.0]);
        assert_eq!(
            raised(unary(&mut arena, MathOp::Ln, &v, None)),
            "2201E: ln() has no answer for 0, which is not above nought"
        );
        assert_eq!(
            raised(unary(&mut arena, MathOp::Log10, &v, None)),
            "2201E: log10() has no answer for 0, which is not above nought"
        );

        let v = ints(&mut arena, &[2, -4]);
        assert_eq!(
            raised(unary(&mut arena, MathOp::Sqrt, &v, None)),
            "2201F: sqrt() has no answer for -4, which is below nought"
        );
    }

    /// An inverse sine is an angle only for an argument between minus
    /// one and one, and outside that the standard says the value is out
    /// of range rather than letting a NaN travel.
    #[test]
    fn an_inverse_angle_outside_minus_one_to_one_raises() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[0.5, 2.0]);
        assert_eq!(
            raised(unary(&mut arena, MathOp::Asin, &v, None)),
            "22003: asin() has no answer for 2, which is outside minus one to one"
        );
        assert_eq!(
            raised(unary(&mut arena, MathOp::Acos, &v, None)),
            "22003: acos() has no answer for 2, which is outside minus one to one"
        );
    }

    /// The cotangent is the one whose condition no fold over the column
    /// rules out, since it is about the sine of the argument rather
    /// than the argument.
    #[test]
    fn a_cotangent_where_the_sine_is_nought_raises() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[1.0, 0.0]);
        assert_eq!(
            raised(unary(&mut arena, MathOp::Cot, &v, None)),
            "22012: cot() has no answer for 0, where the sine is nought"
        );

        let v = floats(&mut arena, &[1.0, 2.0]);
        let out = unary(&mut arena, MathOp::Cot, &v, None).unwrap();
        assert!(out.values::<f64>()[0].is_finite());
    }

    /// An exponential leaves the range a little above seven hundred and
    /// nine, and a number read as degrees leaves it within about fifty
    /// seven of the top.
    #[test]
    fn an_answer_past_the_top_of_a_float_raises() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[1.0, 710.0]);
        assert_eq!(
            raised(unary(&mut arena, MathOp::Exp, &v, None)),
            "22003: exp() of 710 is outside the range of a float"
        );

        let v = floats(&mut arena, &[f64::MAX]);
        assert!(
            raised(unary(&mut arena, MathOp::Degrees, &v, None))
                .starts_with("22003: degrees() of "),
        );

        // And the fold lets an ordinary column past without a walk.
        let v = floats(&mut arena, &[1.0, 700.0]);
        assert!(
            unary(&mut arena, MathOp::Exp, &v, None)
                .unwrap()
                .values::<f64>()[0]
                > 2.7
        );
    }

    /// A row the selection dropped is not a condition here either, and
    /// neither is a null, which are the two things the approximate half
    /// has to get right for a filter to mean what it says.
    #[test]
    fn the_approximate_half_skips_what_nobody_asked_about() {
        let mut arena = MorselArena::new();
        let v = floats(&mut arena, &[4.0, -1.0, 9.0]);
        let mut sel = SelVector::with_capacity(&mut arena, 2);
        sel.push(0);
        sel.push(2);
        let out = unary(&mut arena, MathOp::Sqrt, &v, Some(&sel)).unwrap();
        assert_eq!(out.values::<f64>()[2], 3.0);

        let mut v = floats(&mut arena, &[4.0, -1.0, 9.0]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        v.validity = Some(valid);
        let out = unary(&mut arena, MathOp::Sqrt, &v, None).unwrap();
        assert!(!out.is_valid(1));
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

    /// A remainder of two whole numbers is a whole number, and a power
    /// or a logarithm of them is not, which is the one place the two
    /// argument functions disagree about the type of an answer.
    #[test]
    fn a_remainder_stays_exact_and_the_other_two_do_not() {
        let mut arena = MorselArena::new();
        let l = ints(&mut arena, &[7, -7, 8]);
        let r = ints(&mut arena, &[3, 3, 2]);
        let out = pair(&mut arena, MathPair::Mod, &l, &r, None).unwrap();
        assert_eq!(out.phys, PhysType::Int64);
        assert_eq!(out.values::<i64>(), &[1, -1, 0]);

        let out = pair(&mut arena, MathPair::Power, &l, &r, None).unwrap();
        assert_eq!(out.phys, PhysType::Float64);
        assert_eq!(out.values::<f64>(), &[343.0, -343.0, 64.0]);

        let base = ints(&mut arena, &[2, 2, 2]);
        let of = ints(&mut arena, &[8, 4, 1]);
        let out = pair(&mut arena, MathPair::Log, &base, &of, None).unwrap();
        assert_eq!(out.values::<f64>(), &[3.0, 2.0, 0.0]);
    }

    /// The remainder of an approximate number keeps the sign of the
    /// dividend, which is what the operator answers and so what the
    /// function has to.
    #[test]
    fn an_approximate_remainder_keeps_the_sign_of_the_dividend() {
        let mut arena = MorselArena::new();
        let l = floats(&mut arena, &[7.5, -7.5]);
        let r = floats(&mut arena, &[2.0, 2.0]);
        let out = pair(&mut arena, MathPair::Mod, &l, &r, None).unwrap();
        assert_eq!(out.values::<f64>(), &[1.5, -1.5]);
    }

    /// A divisor of nought has no remainder, whichever type the two
    /// sides hold, and the one pair of whole numbers whose remainder
    /// does not fit says so as the function rather than as the
    /// operator.
    #[test]
    fn a_remainder_with_no_answer_raises() {
        let mut arena = MorselArena::new();
        let l = ints(&mut arena, &[7, 1]);
        let r = ints(&mut arena, &[3, 0]);
        assert_eq!(
            raised(pair(&mut arena, MathPair::Mod, &l, &r, None)),
            "22012: division by zero"
        );

        let l = floats(&mut arena, &[7.0, 1.0]);
        let r = floats(&mut arena, &[3.0, 0.0]);
        assert_eq!(
            raised(pair(&mut arena, MathPair::Mod, &l, &r, None)),
            "22012: division by zero"
        );

        let l = ints(&mut arena, &[i64::MIN]);
        let r = ints(&mut arena, &[-1]);
        assert_eq!(
            raised(pair(&mut arena, MathPair::Mod, &l, &r, None)),
            "22003: mod() of -9223372036854775808 and -1 does not fit an integer"
        );
    }

    /// Nought to a negative power and a negative number to a fraction
    /// are the two the standard names, and both of them are conditions
    /// about the pair rather than about either column.
    #[test]
    fn a_power_with_no_answer_raises() {
        let mut arena = MorselArena::new();
        let l = floats(&mut arena, &[2.0, 0.0]);
        let r = floats(&mut arena, &[3.0, -1.0]);
        assert_eq!(
            raised(pair(&mut arena, MathPair::Power, &l, &r, None)),
            "2201F: power() has no answer for nought raised to a negative power"
        );

        let l = floats(&mut arena, &[2.0, -2.0]);
        let r = floats(&mut arena, &[3.0, 0.5]);
        assert_eq!(
            raised(pair(&mut arena, MathPair::Power, &l, &r, None)),
            "2201F: power() has no answer for -2 raised to 0.5, which is not whole"
        );

        // A negative number to a whole power has an answer, so the
        // fold that let the walk happen must not be the answer itself.
        let l = floats(&mut arena, &[-2.0]);
        let r = floats(&mut arena, &[3.0]);
        let out = pair(&mut arena, MathPair::Power, &l, &r, None).unwrap();
        assert_eq!(out.values::<f64>(), &[-8.0]);
    }

    /// A base is a base when it is above nought and is not one, and the
    /// number a logarithm is taken of has to be above nought as well.
    #[test]
    fn a_logarithm_outside_its_domain_raises() {
        let mut arena = MorselArena::new();
        let base = floats(&mut arena, &[2.0, 1.0]);
        let of = floats(&mut arena, &[8.0, 8.0]);
        assert_eq!(
            raised(pair(&mut arena, MathPair::Log, &base, &of, None)),
            "2201E: log() has no answer in base 1"
        );

        let base = floats(&mut arena, &[2.0, 2.0]);
        let of = floats(&mut arena, &[8.0, 0.0]);
        assert_eq!(
            raised(pair(&mut arena, MathPair::Log, &base, &of, None)),
            "2201E: log() has no answer for 0, which is not above nought"
        );
    }

    /// An answer that left the range of a float is an infinity sitting
    /// in the output, which is the fold the power has and the reason
    /// this kernel computes first and reads the conditions after.
    #[test]
    fn a_power_past_the_top_of_a_float_raises() {
        let mut arena = MorselArena::new();
        let l = floats(&mut arena, &[1.0e300]);
        let r = floats(&mut arena, &[2.0]);
        assert!(
            raised(pair(&mut arena, MathPair::Power, &l, &r, None))
                .starts_with("22003: power() of "),
        );
    }

    /// A row the selection dropped and a row holding a null are not
    /// conditions, which is what lets one of these stand in a filter.
    #[test]
    fn a_pair_skips_what_nobody_asked_about() {
        let mut arena = MorselArena::new();
        let l = ints(&mut arena, &[7, 1, 9]);
        let r = ints(&mut arena, &[3, 0, 2]);
        let mut sel = SelVector::with_capacity(&mut arena, 2);
        sel.push(0);
        sel.push(2);
        let out = pair(&mut arena, MathPair::Mod, &l, &r, Some(&sel)).unwrap();
        assert_eq!(out.values::<i64>()[0], 1);
        assert_eq!(out.values::<i64>()[2], 1);

        let mut r = ints(&mut arena, &[3, 0, 2]);
        let mut valid = Bitmap::new_in(&mut arena, 3, true);
        valid.clear(1);
        r.validity = Some(valid);
        let out = pair(&mut arena, MathPair::Mod, &l, &r, None).unwrap();
        assert!(!out.is_valid(1), "null in, null out");
        assert!(out.is_valid(0) && out.is_valid(2));
    }

    /// Two arguments of two types are not this kernel's, the compiler
    /// having moved a written number into the column's type before it
    /// gets here.
    #[test]
    fn the_two_arguments_hold_the_one_type() {
        assert_eq!(
            MathPair::Mod.answer_type(PhysType::Int64, PhysType::Int64),
            Some(PhysType::Int64)
        );
        assert_eq!(
            MathPair::Power.answer_type(PhysType::Int64, PhysType::Int64),
            Some(PhysType::Float64)
        );
        assert_eq!(
            MathPair::Mod.answer_type(PhysType::Int64, PhysType::Float64),
            None
        );
        assert_eq!(
            MathPair::Log.answer_type(PhysType::Str, PhysType::Str),
            None
        );
    }
}
