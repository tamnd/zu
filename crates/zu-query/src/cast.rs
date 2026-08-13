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
        LogicalType::Str { max, .. } => to_str(v, *max),
        other => Err(ZuError::gql(
            codes::C22G03,
            format!("casting to '{other}' is not implemented"),
        )),
    }
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

fn to_str(v: Value, max: Option<u32>) -> Result<Value> {
    let s = match &v {
        Value::Str(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        other => return Err(not_castable(other, "a string")),
    };
    if let Some(max) = max
        && s.chars().count() > max as usize
    {
        return Err(ZuError::gql(
            codes::C22001,
            format!("'{s}' is longer than STRING({max})"),
        ));
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
    use crate::value_type::{by_name, by_name_with_precision};

    fn ty(name: &str) -> LogicalType {
        by_name(name).expect("a known type")
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
        let t = by_name_with_precision("INT", 3).expect("INT(3)");
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
