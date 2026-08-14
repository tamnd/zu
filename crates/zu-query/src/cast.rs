//! `CAST(expr AS type)`, the checking half of the type lattice.
//!
//! The tower of integer widths is nineteen ISO features and one piece
//! of code, because a width is data. `INT8` and `UINT64` differ in two
//! numbers, the low and high bound, and the whole of GV01 to GV19 is
//! computing that pair and refusing what falls outside it with 22003.
//!
//! Two rules are worth stating because they are easy to get wrong and
//! silent when wrong. A declared precision is a check and not a width:
//! `INT(3)` is sixteen bits and still refuses 1000, so the bound is the
//! tighter of the two. And a target without `NOT NULL` is nullable, so
//! a null casts to a null; only a `NOT NULL` target turns a null into
//! 22004, which is what makes `CAST` usable on optional properties.

use zu_common::gqlstatus::codes;
use zu_common::{FloatBits, IntBits, LogicalType, Result, ZuError};

use crate::exec::Value;

/// Casts `v` to `ty`, or names the condition that stops it.
pub fn cast(v: Value, ty: &LogicalType) -> Result<Value> {
    if matches!(v, Value::Null) {
        if ty.is_nullable() {
            return Ok(Value::Null);
        }
        return Err(ZuError::gql(
            codes::C22004,
            format!("null cast to '{}', which is NOT NULL", ty.base()),
        ));
    }
    match ty.base() {
        LogicalType::Bool => to_bool(v),
        LogicalType::Int {
            signed,
            bits,
            precision,
        } => to_int(v, *signed, *bits, *precision),
        LogicalType::Float { bits, .. } => to_float(v, *bits),
        LogicalType::Decimal { precision, scale } => to_decimal(v, *precision, *scale),
        LogicalType::Str { min, max, .. } => to_str(v, *min, *max),
        LogicalType::List { elem, max } => to_list(v, elem, *max),
        other => Err(ZuError::gql(
            codes::C22G03,
            format!("casting to '{other}' is not implemented"),
        )),
    }
}

/// A cast to a list type is the elementwise cast, GV50.
///
/// The two conditions ISO names for it are the two ways it fails, and
/// they are different failures worth telling apart: `22G0B` is the
/// list being longer than the target's maximum, which is a fact about
/// the list, and `22G0C` is one element not casting, which is a fact
/// about that element and carries the element's own condition in its
/// message. Only a list casts to a list; nothing here wraps a single
/// value into a list of one, because that would make a typo into a
/// silent success.
fn to_list(v: Value, elem: &LogicalType, max: Option<u32>) -> Result<Value> {
    let Value::List(items) = v else {
        return Err(not_castable(&v, "a list"));
    };
    if let Some(limit) = max
        && items.len() > limit as usize
    {
        return Err(ZuError::gql(
            codes::C22G0B,
            format!("a list of {} does not fit a list of {limit}", items.len()),
        ));
    }
    let mut out = Vec::with_capacity(items.len());
    for (at, item) in items.into_iter().enumerate() {
        let cast = cast(item, elem)
            .map_err(|e| ZuError::gql(codes::C22G0C, format!("element {at} of the list: {e}")))?;
        out.push(cast);
    }
    Ok(Value::List(out))
}

/// `22018 invalid character value for cast`, the condition for a value
/// whose spelling the target type does not accept.
fn not_castable(v: &Value, target: &str) -> ZuError {
    ZuError::gql(
        codes::C22018,
        format!("{} cannot be read as {target}", show(v)),
    )
}

/// `22003 numeric value out of range`.
fn out_of_range(shown: String, ty: &str) -> ZuError {
    ZuError::gql(codes::C22003, format!("{shown} does not fit '{ty}'"))
}

fn show(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Str(s) => format!("'{s}'"),
        other => format!("{other:?}"),
    }
}

fn to_bool(v: Value) -> Result<Value> {
    Ok(Value::Bool(match &v {
        Value::Bool(b) => *b,
        Value::Int(0) => false,
        Value::Int(1) => true,
        Value::Str(s) if s.eq_ignore_ascii_case("true") => true,
        Value::Str(s) if s.eq_ignore_ascii_case("false") => false,
        other => return Err(not_castable(other, "a boolean")),
    }))
}

/// The closed interval a width holds, as the widest integer this crate
/// carries. Above 64 bits the interval is wider than any value can be
/// and the bound is the carrier's own, which is checked separately when
/// the result is narrowed back to a runtime value.
fn range(signed: bool, bits: IntBits) -> (i128, i128) {
    if bits >= IntBits::B128 {
        return (i128::MIN, i128::MAX);
    }
    let width = u32::from(bits.bits());
    if signed {
        (-(1i128 << (width - 1)), (1i128 << (width - 1)) - 1)
    } else {
        (0, (1i128 << width) - 1)
    }
}

fn to_int(v: Value, signed: bool, bits: IntBits, precision: Option<u16>) -> Result<Value> {
    let n: i128 = match &v {
        Value::Int(i) => i128::from(*i),
        Value::Bool(b) => i128::from(*b),
        // A float rounds toward zero, which is the standard's
        // truncation and not the nearest integer: 1.9 is 1.
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(out_of_range(show(&v), &name(signed, bits)));
            }
            let t = f.trunc();
            if t < -(2f64.powi(127)) || t >= 2f64.powi(127) {
                return Err(out_of_range(show(&v), &name(signed, bits)));
            }
            t as i128
        }
        Value::Str(s) => s
            .trim()
            .parse::<i128>()
            .map_err(|_| not_castable(&v, "an integer"))?,
        other => return Err(not_castable(other, "an integer")),
    };

    let (lo, hi) = range(signed, bits);
    if n < lo || n > hi {
        return Err(out_of_range(n.to_string(), &name(signed, bits)));
    }
    // The declared digit count is the tighter bound whenever it is
    // narrower than the width, which is the whole point of writing it:
    // INT(3) is a sixteen bit type that also refuses 999 + 1.
    if let Some(digits) = precision
        && let Some(limit) = 10i128.checked_pow(u32::from(digits))
        && (n >= limit || n <= -limit)
    {
        return Err(out_of_range(
            n.to_string(),
            &format!("{}({digits})", name(signed, bits)),
        ));
    }
    let out = i64::try_from(n)
        .map_err(|_| out_of_range(n.to_string(), "a value this executor can carry"))?;
    Ok(Value::Int(out))
}

fn to_float(v: Value, bits: FloatBits) -> Result<Value> {
    let f: f64 = match &v {
        Value::Float(f) => *f,
        Value::Int(i) => *i as f64,
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Str(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| not_castable(&v, "a number"))?,
        other => return Err(not_castable(other, "a number")),
    };
    // Narrowing loses digits by design and that is not an error; going
    // past the width's largest finite value is, because the answer
    // would be an infinity nobody asked for.
    let narrowed = match bits {
        FloatBits::B16 | FloatBits::B32 => f64::from(f as f32),
        _ => f,
    };
    if f.is_finite() && !narrowed.is_finite() {
        return Err(out_of_range(show(&v), &format!("FLOAT{}", bits.bits())));
    }
    Ok(Value::Float(narrowed))
}

/// An exact decimal with `precision` digits in all and `scale` of them
/// after the point, GV17.
///
/// The check is exact and the carrier is not, which is worth stating
/// plainly. The digits are counted on an unscaled integer, so a number
/// with too many of them is refused rather than quietly rounded away,
/// and the result is then handed back as binary floating point because
/// that is the only number a row carries today. An exact carrier
/// arrives with the physical decimal column, and the check written here
/// is the one it will use.
fn to_decimal(v: Value, precision: u16, scale: u16) -> Result<Value> {
    let text = match &v {
        Value::Str(s) => s.trim().to_string(),
        Value::Int(i) => i.to_string(),
        // A float is already inexact, so it is scaled and rounded the
        // same way a written number is, and the digit count is then
        // checked on the result.
        Value::Float(f) if f.is_finite() => format!("{f}"),
        other => return Err(not_castable(other, "an exact number")),
    };
    let spelled = format!("DECIMAL({precision}, {scale})");
    let unscaled = unscaled(&text, scale).ok_or_else(|| not_castable(&v, "an exact number"))?;
    let limit = 10i128
        .checked_pow(u32::from(precision))
        .ok_or_else(|| out_of_range(text.clone(), &spelled))?;
    if unscaled >= limit || unscaled <= -limit {
        return Err(out_of_range(text, &spelled));
    }
    Ok(Value::Float(unscaled as f64 / 10f64.powi(i32::from(scale))))
}

/// `text` read as a decimal and multiplied by ten to the `scale`, with
/// anything past the scale rounded half away from zero. `None` when the
/// text is not a number at all.
fn unscaled(text: &str, scale: u16) -> Option<i128> {
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1i128, rest),
        None => (1i128, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = match digits.split_once('.') {
        Some((w, f)) => (w, f),
        None => (digits, ""),
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
    let mut n: i128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let scale = scale as usize;
    for i in 0..scale {
        n = n.checked_mul(10)?;
        n += i128::from(fraction.as_bytes().get(i).map_or(0, |b| b - b'0'));
    }
    // The first digit the scale drops decides the rounding, half away
    // from zero, which is what a written decimal means by rounding.
    if fraction.as_bytes().get(scale).is_some_and(|b| *b >= b'5') {
        n = n.checked_add(1)?;
    }
    Some(sign * n)
}

/// A character string, GV30 to GV32.
///
/// The minimum length and the fixed length are one rule and not two: a
/// value shorter than the minimum is padded with spaces, and a fixed
/// length type is one whose minimum equals its maximum, so `CHAR(3)`
/// pads without needing a case of its own. Past the maximum is 22001,
/// because there is nowhere for the characters to go.
fn to_str(v: Value, min: Option<u32>, max: Option<u32>) -> Result<Value> {
    let mut s = match &v {
        Value::Str(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        other => return Err(not_castable(other, "a string")),
    };
    let len = s.chars().count();
    if let Some(max) = max
        && len > max as usize
    {
        return Err(ZuError::gql(
            codes::C22001,
            format!("'{s}' is {len} characters and the target holds {max}"),
        ));
    }
    if let Some(min) = min
        && len < min as usize
    {
        s.extend(std::iter::repeat_n(' ', min as usize - len));
    }
    Ok(Value::Str(s))
}

/// The canonical spelling of a width, for messages.
fn name(signed: bool, bits: IntBits) -> String {
    format!("{}INT{}", if signed { "" } else { "U" }, bits.bits())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value_type::spelled;

    fn ty(name: &str) -> LogicalType {
        spelled(name, &[]).expect("a known type")
    }

    fn status(e: &ZuError) -> String {
        match e {
            ZuError::Gql(record) => record.status.code().to_string(),
            other => panic!("expected a gqlstatus, got {other}"),
        }
    }

    #[test]
    fn a_width_holds_its_own_edges_and_refuses_one_past_them() {
        for (spelling, edge) in [
            ("INT8", 127i64),
            ("INT16", 32767),
            ("INT32", 2147483647),
            ("INT64", i64::MAX),
            ("UINT8", 255),
            ("UINT16", 65535),
            ("UINT32", 4294967295),
        ] {
            let t = ty(spelling);
            assert_eq!(cast(Value::Int(edge), &t).unwrap(), Value::Int(edge));
            if spelling != "INT64" {
                let over = cast(Value::Int(edge + 1), &t).unwrap_err();
                assert_eq!(status(&over), "22003", "{spelling} accepted {}", edge + 1);
            }
        }
        assert_eq!(
            status(&cast(Value::Int(-1), &ty("UINT8")).unwrap_err()),
            "22003"
        );
    }

    /// The digit count is the tighter of the two bounds. Three digits
    /// take sixteen bits, which reach 32767, so only the declared
    /// precision can refuse a number the width would have taken.
    #[test]
    fn a_declared_precision_refuses_what_the_width_would_have_taken() {
        let t = spelled("INT", &[3]).expect("INT(3)");
        assert_eq!(cast(Value::Int(100), &t).unwrap(), Value::Int(100));
        assert_eq!(status(&cast(Value::Int(1000), &t).unwrap_err()), "22003");
    }

    #[test]
    fn a_null_passes_through_unless_the_target_says_not_null() {
        let nullable = LogicalType::Nullable(Box::new(ty("INT")));
        assert_eq!(cast(Value::Null, &nullable).unwrap(), Value::Null);
        assert_eq!(status(&cast(Value::Null, &ty("INT")).unwrap_err()), "22004");
    }

    #[test]
    fn the_two_directions_the_corpus_asks_for_round_trip() {
        assert_eq!(
            cast(Value::Str("42".into()), &ty("INT")).unwrap(),
            Value::Int(42)
        );
        assert_eq!(
            cast(Value::Int(42), &ty("STRING")).unwrap(),
            Value::Str("42".into())
        );
        assert_eq!(
            status(&cast(Value::Str("forty two".into()), &ty("INT")).unwrap_err()),
            "22018"
        );
    }

    /// A minimum length pads and a maximum length refuses, and a fixed
    /// length is the case where the two are the same number, which is
    /// why `CHAR` needs no rule of its own.
    #[test]
    fn a_length_pads_at_the_bottom_and_refuses_at_the_top() {
        let fixed = spelled("CHAR", &[3]).expect("CHAR(3)");
        assert_eq!(
            cast(Value::Str("a".into()), &fixed).unwrap(),
            Value::Str("a  ".into())
        );
        assert_eq!(
            status(&cast(Value::Str("abcd".into()), &fixed).unwrap_err()),
            "22001"
        );
        let ranged = spelled("STRING", &[2, 10]).expect("STRING(2, 10)");
        assert_eq!(
            cast(Value::Str("abc".into()), &ranged).unwrap(),
            Value::Str("abc".into())
        );
        assert_eq!(
            cast(Value::Str("a".into()), &ranged).unwrap(),
            Value::Str("a ".into())
        );
    }

    /// The scale says how many digits survive the point and the
    /// precision says how many there are in all, so a number with too
    /// many is refused rather than rounded into range.
    #[test]
    fn a_decimal_rounds_to_its_scale_and_refuses_past_its_precision() {
        let t = spelled("DECIMAL", &[5, 2]).expect("DECIMAL(5, 2)");
        assert_eq!(
            cast(Value::Str("1.20".into()), &t).unwrap(),
            Value::Float(1.2)
        );
        assert_eq!(
            cast(Value::Str("1.235".into()), &t).unwrap(),
            Value::Float(1.24)
        );
        assert_eq!(
            cast(Value::Str("-1.235".into()), &t).unwrap(),
            Value::Float(-1.24)
        );
        // 1000.00 is seven digits unscaled and the type holds five.
        assert_eq!(
            status(&cast(Value::Str("1000.00".into()), &t).unwrap_err()),
            "22003"
        );
        assert_eq!(
            status(&cast(Value::Str("one".into()), &t).unwrap_err()),
            "22018"
        );
    }

    #[test]
    fn a_float_truncates_toward_zero_and_a_narrow_one_refuses_an_infinity() {
        assert_eq!(cast(Value::Float(1.9), &ty("INT")).unwrap(), Value::Int(1));
        assert_eq!(
            cast(Value::Float(-1.9), &ty("INT")).unwrap(),
            Value::Int(-1)
        );
        assert_eq!(
            status(&cast(Value::Float(1e300), &ty("REAL")).unwrap_err()),
            "22003"
        );
        assert_eq!(
            cast(Value::Float(0.5), &ty("REAL")).unwrap(),
            Value::Float(0.5)
        );
    }
}
