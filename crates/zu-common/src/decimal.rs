//! Exact decimal numbers, GV17.
//!
//! A decimal is an integer and a count of digits to move the point by,
//! and that pair is the whole of it. `1.20` is a hundred and twenty
//! hundredths, `1.2` is twelve tenths, and the two are the same number
//! written with different care about how precisely it is known. Both
//! facts matter here: they compare equal, and each prints back the way
//! it was written.
//!
//! The reason for a type of its own rather than binary floating point
//! is that a tenth is not a binary fraction. Adding `0.1` to `0.2` in
//! binary64 gives a number that is not `0.3`, and no amount of care at
//! the edges fixes it, because the value the machine holds was never
//! three tenths. A ledger, a price and a tax rate are all counted in
//! exact units of a tenth or a hundredth, so they are counted here as
//! integers of those units and the scale says which unit.
//!
//! The unscaled integer is an `i128`, which holds thirty eight digits.
//! That is the largest precision `DECIMAL(p, s)` may be declared with,
//! so every declarable decimal fits one and the carrier never has to
//! refuse a value the type admits.

use std::cmp::Ordering;
use std::fmt;

/// The most digits a decimal may have, which is what an `i128` holds
/// with room to spare for the sign.
pub const MAX_DIGITS: u16 = 38;

/// An exact decimal: `unscaled` units, each one ten to the minus
/// `scale`.
///
/// The scale rides on the value rather than only on the declared type,
/// because a decimal exists outside any column. `CAST('1.20' AS
/// DECIMAL(5,2))` in a `RETURN` has no column to ask, and it still has
/// to print two places.
#[derive(Debug, Clone, Copy)]
pub struct Decimal {
    unscaled: i128,
    scale: u16,
}

/// Which way a digit that will not fit is allowed to go.
#[derive(Debug, Clone, Copy)]
enum Round {
    /// Half away from zero, which is the rule everywhere else in this
    /// file and the one `ROUND` is expected to follow.
    Half,
    /// Toward positive infinity, for `CEIL`.
    Up,
    /// Toward negative infinity, for `FLOOR`.
    Down,
}

impl Decimal {
    /// A decimal of `unscaled` units of ten to the minus `scale`.
    ///
    /// Nothing is normalised. The caller wrote a scale and it is kept,
    /// so a value cast to `DECIMAL(5,2)` prints two places even when
    /// the second one is a nought.
    pub fn new(unscaled: i128, scale: u16) -> Decimal {
        Decimal { unscaled, scale }
    }

    /// The integer this is counted in units of.
    pub fn unscaled(&self) -> i128 {
        self.unscaled
    }

    /// How many of the digits are after the point.
    pub fn scale(&self) -> u16 {
        self.scale
    }

    /// Whether this is exactly zero, which the sign of the unscaled
    /// integer does not answer on its own since zero has none.
    pub fn is_zero(&self) -> bool {
        self.unscaled == 0
    }

    /// How many digits the unscaled integer is written with, which is
    /// the precision a value needs rather than the one it was declared
    /// with. Zero is one digit.
    pub fn digits(&self) -> u16 {
        let mut left = self.unscaled.unsigned_abs();
        let mut digits = 1;
        while left >= 10 {
            left /= 10;
            digits += 1;
        }
        digits
    }

    /// This number written at `scale` instead, or `None` when the
    /// change would not be exact or would not fit.
    ///
    /// Going to a larger scale multiplies and can overflow; going to a
    /// smaller one divides and can drop a digit that is not a nought.
    /// Both are refused rather than rounded, because the caller that
    /// wants rounding says so and the caller that does not is asking
    /// whether this number is one of that type at all.
    pub fn rescale(&self, scale: u16) -> Option<Decimal> {
        match scale.cmp(&self.scale) {
            Ordering::Equal => Some(*self),
            Ordering::Greater => {
                let by = 10i128.checked_pow(u32::from(scale - self.scale))?;
                Some(Decimal {
                    unscaled: self.unscaled.checked_mul(by)?,
                    scale,
                })
            }
            Ordering::Less => {
                let by = 10i128.checked_pow(u32::from(self.scale - scale))?;
                match self.unscaled % by {
                    0 => Some(Decimal {
                        unscaled: self.unscaled / by,
                        scale,
                    }),
                    _ => None,
                }
            }
        }
    }

    /// This number without its sign, at the scale it was written with.
    ///
    /// `None` only for the one number an `i128` cannot negate.
    pub fn abs(&self) -> Option<Decimal> {
        Some(Decimal {
            unscaled: self.unscaled.checked_abs()?,
            scale: self.scale,
        })
    }

    /// One, nought or minus one, after the sign of this number.
    pub fn signum(&self) -> i64 {
        self.unscaled.signum() as i64
    }

    /// This number rounded to `digits` places after the point, half
    /// away from zero, which is what `ROUND` means over exact numbers.
    ///
    /// `digits` may be negative, which rounds to a place left of the
    /// point: a hundred and fifty at minus two digits is two hundred.
    /// The answer's scale is `digits` where that is a scale, and nought
    /// where it is not, because a number rounded to hundreds has no
    /// digits after the point to keep.
    pub fn round(&self, digits: i64) -> Option<Decimal> {
        self.quantize(digits, Round::Half)
    }

    /// This number taken up to the next `digits` place, toward positive
    /// infinity, which is what `CEIL` means: `-1.5` ceils to `-1`.
    pub fn ceil(&self, digits: i64) -> Option<Decimal> {
        self.quantize(digits, Round::Up)
    }

    /// This number taken down to the previous `digits` place, toward
    /// negative infinity: `-1.5` floors to `-2`.
    pub fn floor(&self, digits: i64) -> Option<Decimal> {
        self.quantize(digits, Round::Down)
    }

    /// The shared body of the three: the number written at scale
    /// `digits` under `mode`, then brought back to a scale that exists,
    /// which is `digits` itself where it is not negative and nought
    /// where it is.
    fn quantize(&self, digits: i64, mode: Round) -> Option<Decimal> {
        let scale = u16::try_from(digits.max(0)).ok()?.min(MAX_DIGITS);
        let drop = i64::from(self.scale) - digits;
        if drop <= 0 {
            return self.rescale(scale);
        }
        let by = 10i128.checked_pow(u32::try_from(drop).ok()?)?;
        let (whole, rest) = (self.unscaled / by, self.unscaled % by);
        let carried = match mode {
            Round::Half if rest.checked_mul(2)?.abs() >= by => match self.unscaled.is_negative() {
                true => whole.checked_sub(1)?,
                false => whole.checked_add(1)?,
            },
            Round::Up if rest > 0 => whole.checked_add(1)?,
            Round::Down if rest < 0 => whole.checked_sub(1)?,
            _ => whole,
        };
        // Where the rounding went left of the point, the noughts it
        // stood on go back: two hundreds is two hundred.
        let back = 10i128.checked_pow(u32::try_from(i64::from(scale) - digits).ok()?)?;
        Some(Decimal {
            unscaled: carried.checked_mul(back)?,
            scale,
        })
    }

    /// The whole part of this number, with the digits past the point
    /// dropped rather than rounded, so `1.9` is 1 and `-1.9` is -1.
    ///
    /// Truncation toward zero and not rounding, because that is what
    /// the standard's cast to an exact integer does and the caller that
    /// wants the nearest integer asks for it.
    pub fn truncate(&self) -> Option<i128> {
        let by = 10i128.checked_pow(u32::from(self.scale))?;
        Some(self.unscaled / by)
    }

    /// This number as binary floating point, which is a narrowing and
    /// is why it is spelled out rather than done wherever a float is
    /// wanted.
    ///
    /// Every decimal has a nearest binary64 and most have no exact one,
    /// so this is the edge where exactness ends. Callers reach it when
    /// a decimal meets a float in arithmetic, where the answer is
    /// inexact whatever is done, and when a function is defined on
    /// binary floating point and on nothing else.
    pub fn to_f64(&self) -> f64 {
        self.unscaled as f64 / 10f64.powi(i32::from(self.scale))
    }

    /// The text read as a decimal at `scale`, rounded half away from
    /// zero past the scale. `None` is text that is not a number, or a
    /// number too large for the carrier.
    ///
    /// Half away from zero is what a person writing money means by
    /// rounding, and it is what the standard's `CAST` to an exact
    /// numeric permits an implementation to choose. An exponent is
    /// accepted because `1E3` is a number a query may write, and it is
    /// applied to the scale rather than to a float, so nothing goes
    /// through binary on the way.
    pub fn parse(text: &str, scale: u16) -> Option<Decimal> {
        let text = text.trim();
        let (negative, rest) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text.strip_prefix('+').unwrap_or(text)),
        };
        let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
            Some((m, e)) => (m, e.parse::<i32>().ok()?),
            None => (rest, 0),
        };
        let (whole, fraction) = match mantissa.split_once('.') {
            Some((w, f)) => (w, f),
            None => (mantissa, ""),
        };
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        if !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
        {
            return None;
        }
        // The digits are read as one integer and the point is then a
        // number, which is the scale the text was written at, less
        // whatever the exponent moved it by.
        let mut digits: i128 = 0;
        for byte in whole.bytes().chain(fraction.bytes()) {
            digits = digits
                .checked_mul(10)?
                .checked_add(i128::from(byte - b'0'))?;
        }
        let written = i64::try_from(fraction.len())
            .ok()?
            .checked_sub(i64::from(exponent))?;
        let unscaled = shift(digits, written, i64::from(scale))?;
        Some(Decimal {
            unscaled: match negative {
                true => -unscaled,
                false => unscaled,
            },
            scale,
        })
    }

    /// The sum of two decimals, at the larger of the two scales, or
    /// `None` on overflow.
    ///
    /// The wider scale is the exact one: a hundredth plus a tenth is a
    /// number of hundredths, and answering in tenths would have to
    /// throw a digit away.
    pub fn add(&self, other: &Decimal) -> Option<Decimal> {
        let (a, b) = align(self, other)?;
        Some(Decimal {
            unscaled: a.unscaled.checked_add(b.unscaled)?,
            scale: a.scale,
        })
    }

    /// The difference of two decimals, at the larger of the two scales.
    pub fn sub(&self, other: &Decimal) -> Option<Decimal> {
        let (a, b) = align(self, other)?;
        Some(Decimal {
            unscaled: a.unscaled.checked_sub(b.unscaled)?,
            scale: a.scale,
        })
    }

    /// The product of two decimals, whose scale is the sum of the two
    /// scales, since that is what multiplying the units does.
    pub fn mul(&self, other: &Decimal) -> Option<Decimal> {
        let scale = self.scale.checked_add(other.scale)?;
        if scale > MAX_DIGITS {
            return None;
        }
        Some(Decimal {
            unscaled: self.unscaled.checked_mul(other.unscaled)?,
            scale,
        })
    }

    /// The quotient of two decimals, or `None` for a division by zero
    /// or an overflow.
    ///
    /// A quotient is the one operation exact arithmetic cannot always
    /// answer exactly: a third has no decimal spelling at any scale. So
    /// the answer is carried to a scale that keeps what the operands
    /// knew and no further, and the last digit is rounded half away
    /// from zero. The scale chosen is the larger operand scale plus six
    /// guard digits, capped at the carrier, which is enough that money
    /// divided by a count still says what a person would write.
    pub fn div(&self, other: &Decimal) -> Option<Decimal> {
        if other.unscaled == 0 {
            return None;
        }
        let scale = self
            .scale
            .max(other.scale)
            .saturating_add(6)
            .min(MAX_DIGITS);
        // Scale the numerator up by the answer's scale and by the
        // divisor's own, since dividing by a number of hundredths
        // multiplies by a hundred.
        let up = i64::from(scale) + i64::from(other.scale) - i64::from(self.scale);
        let numerator = shift(i128::try_from(self.unscaled.unsigned_abs()).ok()?, 0, up)?;
        let divisor = i128::try_from(other.unscaled.unsigned_abs()).ok()?;
        let quotient = numerator / divisor;
        // Half away from zero: the remainder doubled reaching the
        // divisor is a half or more.
        let unscaled = match (numerator % divisor).checked_mul(2) {
            Some(twice) if twice >= divisor => quotient.checked_add(1)?,
            _ => quotient,
        };
        Some(Decimal {
            unscaled: match self.unscaled.is_negative() != other.unscaled.is_negative() {
                true => -unscaled,
                false => unscaled,
            },
            scale,
        })
    }

    /// The remainder of a division, at the larger of the two scales,
    /// with the sign of the left operand as every other remainder in
    /// this engine has.
    pub fn rem(&self, other: &Decimal) -> Option<Decimal> {
        let (a, b) = align(self, other)?;
        if b.unscaled == 0 {
            return None;
        }
        Some(Decimal {
            unscaled: a.unscaled.checked_rem(b.unscaled)?,
            scale: a.scale,
        })
    }

    /// This number with its sign turned around.
    pub fn negate(&self) -> Option<Decimal> {
        Some(Decimal {
            unscaled: self.unscaled.checked_neg()?,
            scale: self.scale,
        })
    }
}

/// `digits`, written at scale `from`, read at scale `to`, rounding half
/// away from zero where that loses a digit. `None` on overflow.
fn shift<T>(digits: T, from: i64, to: i64) -> Option<i128>
where
    i128: TryFrom<T>,
{
    let digits = i128::try_from(digits).ok()?;
    match to.checked_sub(from)? {
        0 => Some(digits),
        by if by > 0 => digits.checked_mul(10i128.checked_pow(u32::try_from(by).ok()?)?),
        by => {
            let down = 10i128.checked_pow(u32::try_from(-by).ok()?)?;
            let whole = digits / down;
            match (digits % down).checked_mul(2) {
                Some(twice) if twice.abs() >= down => match digits.is_negative() {
                    true => whole.checked_sub(1),
                    false => whole.checked_add(1),
                },
                _ => Some(whole),
            }
        }
    }
}

/// The two numbers written at one scale, which is the larger of theirs,
/// so that the pair can be added, subtracted or compared as integers.
fn align(a: &Decimal, b: &Decimal) -> Option<(Decimal, Decimal)> {
    let scale = a.scale.max(b.scale);
    Some((a.rescale(scale)?, b.rescale(scale)?))
}

/// Two decimals are equal when they are the same number, whatever scale
/// each was written at, so `1.20` equals `1.2`.
///
/// The scale is how precisely a number is known and not part of which
/// number it is. A query grouping prices would otherwise put `1.5` and
/// `1.50` in two groups, which is a distinction no user of a ledger
/// means to draw.
impl PartialEq for Decimal {
    fn eq(&self, other: &Decimal) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Decimal {}

/// A total order, which two exact numbers always have.
///
/// Where the scales cannot be aligned without overflowing, the numbers
/// are compared by sign and then by how many digits they have before
/// the point, which orders them correctly because a number with more
/// whole digits is the larger one.
impl Ord for Decimal {
    fn cmp(&self, other: &Decimal) -> Ordering {
        if let Some((a, b)) = align(self, other) {
            return a.unscaled.cmp(&b.unscaled);
        }
        let sign = self.unscaled.signum().cmp(&other.unscaled.signum());
        if sign != Ordering::Equal {
            return sign;
        }
        let whole = |d: &Decimal| i32::from(d.digits()) - i32::from(d.scale);
        let by_size = whole(self).cmp(&whole(other));
        match self.unscaled.is_negative() {
            true => by_size.reverse(),
            false => by_size,
        }
    }
}

impl PartialOrd for Decimal {
    fn partial_cmp(&self, other: &Decimal) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::hash::Hash for Decimal {
    /// Hashed on the number and not on the pair, so that two values
    /// which compare equal hash alike and a decimal can key a group.
    ///
    /// The normal form is the unscaled integer with its trailing
    /// noughts taken off, which every spelling of one number reduces
    /// to.
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let (mut unscaled, mut scale) = (self.unscaled, i64::from(self.scale));
        while unscaled != 0 && unscaled % 10 == 0 {
            unscaled /= 10;
            scale -= 1;
        }
        unscaled.hash(state);
        match unscaled {
            0 => 0i64.hash(state),
            _ => scale.hash(state),
        }
    }
}

/// Written the way it was declared: the digits with a point put in at
/// the scale, and no exponent, since an exact number is written out.
impl fmt::Display for Decimal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.scale == 0 {
            return write!(f, "{}", self.unscaled);
        }
        let digits = self.unscaled.unsigned_abs().to_string();
        let places = usize::from(self.scale);
        let sign = match self.unscaled.is_negative() {
            true => "-",
            false => "",
        };
        // A number smaller than one has no whole digits of its own, so
        // the nought before the point and the noughts after it are put
        // there rather than taken from the digits.
        if digits.len() <= places {
            let pad = "0".repeat(places - digits.len());
            return write!(f, "{sign}0.{pad}{digits}");
        }
        let (whole, fraction) = digits.split_at(digits.len() - places);
        write!(f, "{sign}{whole}.{fraction}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decimal_prints_at_the_scale_it_was_written_with() {
        assert_eq!(Decimal::new(120, 2).to_string(), "1.20");
        assert_eq!(Decimal::new(12, 1).to_string(), "1.2");
        assert_eq!(Decimal::new(12, 0).to_string(), "12");
        assert_eq!(Decimal::new(-120, 2).to_string(), "-1.20");
        // Fewer digits than places, so the nought and the padding are
        // written rather than taken out of the digits.
        assert_eq!(Decimal::new(5, 3).to_string(), "0.005");
        assert_eq!(Decimal::new(-5, 3).to_string(), "-0.005");
        assert_eq!(Decimal::new(0, 2).to_string(), "0.00");
    }

    #[test]
    fn two_spellings_of_one_number_are_one_value() {
        let (loose, tight) = (Decimal::new(12, 1), Decimal::new(120, 2));
        assert_eq!(loose, tight);
        assert_eq!(loose.cmp(&tight), Ordering::Equal);
        // And they hash alike, which is what lets one key a group.
        let hash = |d: &Decimal| {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            d.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(&loose), hash(&tight));
        assert_eq!(hash(&Decimal::new(0, 0)), hash(&Decimal::new(0, 7)));
        // They print differently all the same, because the scale is
        // how precisely the number is known.
        assert_ne!(loose.to_string(), tight.to_string());
    }

    #[test]
    fn a_tenth_and_two_tenths_are_three_tenths() {
        let (a, b) = (
            Decimal::parse("0.1", 1).unwrap(),
            Decimal::parse("0.2", 1).unwrap(),
        );
        assert_eq!(a.add(&b).unwrap().to_string(), "0.3");
        // Which is the whole point: the same two numbers taken as
        // binary64 do not add up to three tenths, and no arrangement of
        // the sum makes them.
        assert_ne!(a.to_f64() + b.to_f64(), 0.3f64);
    }

    #[test]
    fn arithmetic_answers_at_the_scale_the_units_give() {
        let (a, b) = (Decimal::new(150, 2), Decimal::new(3, 1));
        // A hundredth plus a tenth is a number of hundredths.
        assert_eq!(a.add(&b).unwrap().to_string(), "1.80");
        assert_eq!(a.sub(&b).unwrap().to_string(), "1.20");
        // Multiplying the units multiplies the scale.
        assert_eq!(a.mul(&b).unwrap().to_string(), "0.450");
        assert_eq!(a.rem(&b).unwrap().to_string(), "0.00");
        assert_eq!(a.negate().unwrap().to_string(), "-1.50");
    }

    #[test]
    fn a_quotient_is_carried_as_far_as_the_operands_knew() {
        let third = Decimal::new(1, 0).div(&Decimal::new(3, 0)).unwrap();
        assert_eq!(third.to_string(), "0.333333");
        // Rounded half away from zero at the last digit kept.
        assert_eq!(
            Decimal::new(2, 0)
                .div(&Decimal::new(3, 0))
                .unwrap()
                .to_string(),
            "0.666667"
        );
        assert_eq!(
            Decimal::new(-2, 0)
                .div(&Decimal::new(3, 0))
                .unwrap()
                .to_string(),
            "-0.666667"
        );
        // A price split three ways still says what a person would.
        let each = Decimal::new(1000, 2).div(&Decimal::new(4, 0)).unwrap();
        assert_eq!(each.rescale(2).unwrap().to_string(), "2.50");
        assert_eq!(Decimal::new(1, 0).div(&Decimal::new(0, 0)), None);
    }

    #[test]
    fn text_is_read_at_the_scale_it_is_wanted_at() {
        assert_eq!(Decimal::parse("1.20", 2).unwrap(), Decimal::new(120, 2));
        assert_eq!(
            Decimal::parse("  -3.5 ", 3).unwrap(),
            Decimal::new(-3500, 3)
        );
        assert_eq!(Decimal::parse("12", 2).unwrap(), Decimal::new(1200, 2));
        assert_eq!(Decimal::parse(".5", 1).unwrap(), Decimal::new(5, 1));
        assert_eq!(Decimal::parse("5.", 1).unwrap(), Decimal::new(50, 1));
        // An exponent moves the point rather than going through a float.
        assert_eq!(Decimal::parse("1E3", 0).unwrap(), Decimal::new(1000, 0));
        assert_eq!(Decimal::parse("15e-2", 2).unwrap(), Decimal::new(15, 2));
        // Past the scale it rounds half away from zero.
        assert_eq!(Decimal::parse("1.005", 2).unwrap(), Decimal::new(101, 2));
        assert_eq!(Decimal::parse("-1.005", 2).unwrap(), Decimal::new(-101, 2));
        assert_eq!(Decimal::parse("1.004", 2).unwrap(), Decimal::new(100, 2));
        for not_a_number in ["", ".", "x", "1.2.3", "1,000", "1e", "0x10"] {
            assert_eq!(Decimal::parse(not_a_number, 2), None, "{not_a_number}");
        }
    }

    #[test]
    fn a_rescale_that_would_not_be_exact_is_refused() {
        let value = Decimal::new(125, 2);
        assert_eq!(value.rescale(4).unwrap(), Decimal::new(12500, 4));
        assert_eq!(value.rescale(2).unwrap(), value);
        // Two places down to one would throw away a five.
        assert_eq!(value.rescale(1), None);
        assert_eq!(
            Decimal::new(120, 2).rescale(1).unwrap(),
            Decimal::new(12, 1)
        );
    }

    #[test]
    fn rounding_stops_at_the_place_it_was_asked_for() {
        let value = Decimal::parse("1.005", 3).unwrap();
        // The float this reads as is a shade under a thousand and five
        // thousandths, so a float would round it down. An exact number
        // has the five and rounds up.
        assert_eq!(value.round(2).unwrap().to_string(), "1.01");
        assert_eq!(value.round(0).unwrap().to_string(), "1");
        assert_eq!(value.round(5).unwrap().to_string(), "1.00500");
    }

    #[test]
    fn halves_go_away_from_nought_on_both_sides_of_it() {
        assert_eq!(Decimal::new(25, 1).round(0).unwrap().to_string(), "3");
        assert_eq!(Decimal::new(-25, 1).round(0).unwrap().to_string(), "-3");
        assert_eq!(Decimal::new(35, 1).round(0).unwrap().to_string(), "4");
    }

    #[test]
    fn a_negative_digit_count_rounds_to_the_left_of_the_point() {
        // Minus one is the tens, so this drops the half and the five
        // under it stays where it is.
        assert_eq!(Decimal::new(1505, 1).round(-1).unwrap().to_string(), "150");
        // Minus two is the hundreds, and now the fifty is the half.
        assert_eq!(Decimal::new(150, 0).round(-2).unwrap().to_string(), "200");
        assert_eq!(Decimal::new(-150, 0).round(-2).unwrap().to_string(), "-200");
        assert_eq!(Decimal::new(149, 0).round(-2).unwrap().to_string(), "100");
    }

    #[test]
    fn a_ceiling_and_a_floor_lean_the_same_way_for_both_signs() {
        let up = Decimal::new(15, 1);
        let down = Decimal::new(-15, 1);
        assert_eq!(up.ceil(0).unwrap().to_string(), "2");
        assert_eq!(up.floor(0).unwrap().to_string(), "1");
        // Toward positive infinity is toward nought here, which is what
        // separates a ceiling from rounding away from nought.
        assert_eq!(down.ceil(0).unwrap().to_string(), "-1");
        assert_eq!(down.floor(0).unwrap().to_string(), "-2");
        // Nothing to drop, so nothing moves.
        assert_eq!(Decimal::new(2, 0).ceil(0).unwrap().to_string(), "2");
    }

    #[test]
    fn the_digit_count_is_of_the_number_and_not_of_the_declaration() {
        assert_eq!(Decimal::new(0, 4).digits(), 1);
        assert_eq!(Decimal::new(9, 0).digits(), 1);
        assert_eq!(Decimal::new(10, 0).digits(), 2);
        assert_eq!(Decimal::new(-999, 2).digits(), 3);
        assert_eq!(Decimal::new(i128::MAX, 0).digits(), 39);
    }

    #[test]
    fn ordering_holds_where_the_scales_cannot_be_aligned() {
        // Aligning these would overflow, so the order comes from the
        // sign and the count of whole digits.
        let huge = Decimal::new(i128::MAX, 0);
        let small = Decimal::new(1, MAX_DIGITS);
        assert_eq!(huge.cmp(&small), Ordering::Greater);
        assert_eq!(small.cmp(&huge), Ordering::Less);
        assert_eq!(
            huge.negate().unwrap().cmp(&small.negate().unwrap()),
            Ordering::Less
        );
        assert_eq!(Decimal::new(0, 0).cmp(&small), Ordering::Less);
    }
}
