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
use zu_common::temporal::{NANOS_PER_DAY, NANOS_PER_MINUTE};
use zu_common::{FloatBits, IntBits, LogicalType, Result, Temporal, ZuError};

use crate::exec::Value;

/// Casts `v` to `ty`, or names the condition that stops it.
///
/// Every pair of a value kind and a target type is one cell of the cast
/// matrix and every cell has an answer: the value, or a condition that
/// says which pair it was. A cell nobody wrote a rule for is refused by
/// [`forbidden`] with both type names in the diagnostic record, so a
/// hole in the matrix reads as a refusal and never as a wrong value.
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
        LogicalType::Record(rt) => to_record(v, rt),
        target @ (LogicalType::Date
        | LogicalType::LocalTime
        | LogicalType::ZonedTime
        | LogicalType::LocalDatetime
        | LogicalType::ZonedDatetime
        | LogicalType::Duration(_)) => to_temporal(v, target),
        // A reference is a handle the engine hands out and never a
        // value the language builds, so the only cast to one that can
        // mean anything is the one that already holds it.
        target @ (LogicalType::Node(_) | LogicalType::Edge(_) | LogicalType::Path(_)) => {
            keep(v, target)
        }
        // GV65 and GV68. The open unions accept what they are open to,
        // which for a property union is everything a property may hold.
        LogicalType::Any => Ok(v),
        LogicalType::AnyProperty => {
            if property_value(&v) {
                Ok(v)
            } else {
                Err(forbidden(&v, &LogicalType::AnyProperty))
            }
        }
        // GV67. A closed union is its members tried in the order they
        // were written, which makes ANY<INT|STRING> of '7' the integer
        // and not the string, and the whole cast fails only when no
        // member takes the value.
        LogicalType::Union(members) => {
            for member in members {
                if let Ok(out) = cast(v.clone(), member) {
                    return Ok(out);
                }
            }
            Err(forbidden(&v, ty.base()))
        }
        // GV71 and GV72, the two immaterial types. The null is the only
        // value of `NULL` and the top of this function has already
        // answered it, and no value at all has type `NOTHING`.
        LogicalType::Null | LogicalType::Nothing => Err(forbidden(&v, ty.base())),
        // GV35 to GV38. There is no byte string value in this executor
        // yet and no syntax to write one with, and a cast that picked an
        // encoding for a character string on the user's behalf would be
        // a wrong answer rather than a missing one, so the whole column
        // refuses until the value exists.
        LogicalType::Bytes { .. } => Err(forbidden(&v, ty.base())),
        // GV60 and GV61. A graph and a binding table are catalog handles
        // the engine hands out, and nothing an expression builds is one,
        // so the same rule as the references applies with none of them
        // ever holding.
        LogicalType::Graph(_) | LogicalType::BindingTable(_) => Err(forbidden(&v, ty.base())),
        // `base()` above has already removed the wrapper, and matching
        // on it here rather than on a catch-all is what makes a new
        // member of the lattice a compile error in this function.
        LogicalType::Nullable(_) => unreachable!("base() strips the nullability wrapper"),
    }
}

/// Whether this value is one a property may hold, which is GV68's
/// membership test.
///
/// A property holds the values the standard calls property values: the
/// scalars, the temporals, and a list of them. The two exclusions are
/// the references and the constructed types, and they are excluded for
/// the same reason, that a property is stored and neither a handle nor
/// a record is a thing to store. A list is asked about its elements
/// rather than waved through, because a list of nodes is no more
/// storable than a node is.
fn property_value(v: &Value) -> bool {
    match v {
        Value::Node { .. } | Value::Rel { .. } | Value::Path(_) | Value::Chain(_) => false,
        Value::Record(_) => false,
        Value::List(items) => items.iter().all(property_value),
        _ => true,
    }
}

/// A cast to a reference type, which succeeds only where the value is
/// already of that kind.
///
/// The named forms, `NODE nodetype` and the rest, are a check against
/// the catalog rather than against the value, so they are refused here
/// until the catalog is a thing a cast can read, G2. Refusing is the
/// safe half of that: an unchecked cast to a named type would claim a
/// label the node may not wear.
fn keep(v: Value, target: &LogicalType) -> Result<Value> {
    let held = match (&v, target) {
        (Value::Node { .. }, LogicalType::Node(None))
        | (Value::Rel { .. }, LogicalType::Edge(None))
        | (Value::Path(_), LogicalType::Path(None)) => true,
        (Value::Node { .. }, LogicalType::Node(Some(_)))
        | (Value::Rel { .. }, LogicalType::Edge(Some(_)))
        | (Value::Path(_), LogicalType::Path(Some(_))) => {
            return Err(ZuError::gql(
                codes::C22G0W,
                format!("'{target}' names a type this cast cannot check yet"),
            ));
        }
        _ => false,
    };
    if held {
        Ok(v)
    } else {
        // 22G0V is the reference cast's own condition: the value is
        // not of the base type the reference type names.
        Err(ZuError::gql(
            codes::C22G0V,
            format!("{} is not a value of type '{target}'", show(&v)),
        ))
    }
}

/// `22G03 invalid value type`, the condition every forbidden cell of
/// the matrix raises, with both type names in the record.
fn forbidden(v: &Value, target: &LogicalType) -> ZuError {
    forbidden_named(v, &target.to_string())
}

/// [`forbidden`] where the target is spelled rather than built, for the
/// conversions that are handed a width or a length instead of the type
/// the user wrote.
fn forbidden_named(v: &Value, target: &str) -> ZuError {
    ZuError::gql(
        codes::C22G03,
        format!("'{}' does not cast to '{target}'", value_type(v)),
    )
}

/// The name of the type a value has, for the two type names a forbidden
/// cell owes its diagnostic record. [`crate::row`] raises the same
/// condition when a caller reads a column as the wrong Rust type, and
/// spells the type it found the same way, because a caller comparing
/// the two messages is looking at one question.
pub(crate) fn value_type(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(_) => "BOOL".into(),
        Value::Int(_) => "INT64".into(),
        Value::Float(_) => "FLOAT64".into(),
        Value::Str(_) => "STRING".into(),
        Value::Node { .. } => "NODE".into(),
        Value::Rel { .. } => "EDGE".into(),
        Value::List(_) => "LIST".into(),
        Value::Record(_) => "RECORD".into(),
        Value::Temporal(t) => t.logical_type().to_string(),
        Value::Path(_) | Value::Chain(_) => "PATH".into(),
    }
}

/// A cast to one of the six temporal types, GV39 to GV42.
///
/// Two sources reach them. A string is read the way a literal of the
/// target type is read, so `CAST('2024-01-15' AS DATE)` and `DATE
/// '2024-01-15'` are the same value by construction. A temporal value
/// converts by ISO's own rules, which are in [`convert`]; nothing else
/// converts at all, because a number is not an instant until something
/// says which epoch and which unit it counts, and ISO does not.
///
/// A duration written out is the one place the text decides the type
/// rather than the target: `P1Y` is a year-month duration and `P1D` is
/// a day-time one, whichever of the two the target spells, because that
/// is what the duration literal does and there is no second spelling to
/// ask for the year-month kind with. A duration value already has its
/// kind, and the two kinds do not convert into each other.
fn to_temporal(v: Value, target: &LogicalType) -> Result<Value> {
    match v {
        Value::Str(ref s) => match Temporal::parse(target, s) {
            Some(t) => Ok(Value::Temporal(t)),
            None if matches!(target, LogicalType::Duration(_)) => Err(ZuError::gql(
                codes::C22G0H,
                format!("'{s}' is not a duration"),
            )),
            None => Err(ZuError::gql(
                codes::C22007,
                format!("'{s}' is not a value of type '{target}'"),
            )),
        },
        Value::Temporal(from) => match convert(from, target) {
            Some(t) => Ok(Value::Temporal(t)),
            None => Err(forbidden(&Value::Temporal(from), target)),
        },
        other => Err(forbidden(&other, target)),
    }
}

/// One temporal value read as another temporal type, `None` where the
/// pair has no conversion at all.
///
/// The rules are ISO's and each one is a different kind of loss. A
/// datetime truncates to its date and to its time of day, and a date
/// zero fills to midnight, so the pair is not a round trip and is not
/// meant to be. A zoned value cast to a local one **drops** the offset
/// and keeps the reading a clock in that zone showed, rather than
/// normalising to UTC, which is the rule engines most often get
/// backwards. A local value cast to a zoned one takes UTC, which is
/// zu's implementation defined default zone.
///
/// The two duration kinds never convert into each other. Months and
/// nanoseconds have no ratio: a month is 28, 29, 30 or 31 days and
/// which one depends on a date the duration does not carry.
fn convert(from: Temporal, target: &LogicalType) -> Option<Temporal> {
    use LogicalType as L;
    // The local reading of a value and the offset it was written with,
    // which is what every conversion below is stated in.
    let local_datetime = |nanos: i64, offset: i16| nanos + i64::from(offset) * NANOS_PER_MINUTE;
    Some(match (from, target) {
        (t, target) if t.logical_type() == *target => t,

        (Temporal::Date(days), L::LocalDatetime) => {
            Temporal::LocalDatetime(i64::from(days) * NANOS_PER_DAY)
        }
        (Temporal::Date(days), L::ZonedDatetime) => Temporal::ZonedDatetime {
            nanos: i64::from(days) * NANOS_PER_DAY,
            offset: 0,
        },

        (Temporal::LocalDatetime(nanos), L::Date) => Temporal::Date(day_of(nanos)),
        (Temporal::LocalDatetime(nanos), L::LocalTime) => Temporal::LocalTime(time_of(nanos)),
        (Temporal::LocalDatetime(nanos), L::ZonedTime) => Temporal::ZonedTime {
            nanos: time_of(nanos),
            offset: 0,
        },
        (Temporal::LocalDatetime(nanos), L::ZonedDatetime) => {
            Temporal::ZonedDatetime { nanos, offset: 0 }
        }

        (Temporal::ZonedDatetime { nanos, offset }, L::Date) => {
            Temporal::Date(day_of(local_datetime(nanos, offset)))
        }
        (Temporal::ZonedDatetime { nanos, offset }, L::LocalTime) => {
            Temporal::LocalTime(time_of(local_datetime(nanos, offset)))
        }
        (Temporal::ZonedDatetime { nanos, offset }, L::LocalDatetime) => {
            Temporal::LocalDatetime(local_datetime(nanos, offset))
        }
        (Temporal::ZonedDatetime { nanos, offset }, L::ZonedTime) => Temporal::ZonedTime {
            nanos: time_of(local_datetime(nanos, offset)),
            offset,
        },

        (Temporal::LocalTime(nanos), L::ZonedTime) => Temporal::ZonedTime { nanos, offset: 0 },
        (Temporal::ZonedTime { nanos, .. }, L::LocalTime) => Temporal::LocalTime(nanos),

        _ => return None,
    })
}

/// The day a datetime falls on, counting a day that starts before the
/// epoch as the day it started rather than the one it ends in.
fn day_of(nanos: i64) -> i32 {
    (nanos.div_euclid(NANOS_PER_DAY)) as i32
}

/// The time of day a datetime shows, which is always at or after
/// midnight even when the instant is before the epoch.
fn time_of(nanos: i64) -> i64 {
    nanos.rem_euclid(NANOS_PER_DAY)
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
        return Err(forbidden_named(&v, &format!("LIST<{elem}>")));
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

/// A cast to a record type is the fieldwise cast, GV46.
///
/// ISO separates the two ways it fails and the separation is the point.
/// `22G0Y` is a field the type declares and the record does not carry,
/// which is a fact about the record's shape, and `22G0X` is a field
/// that is there and will not go into its declared type, which is a
/// fact about that field's value and carries its own condition in the
/// message. A field the type does not name is dropped by a closed
/// record type, because a closed type says what the record has, and
/// kept by an open one, because an open type says only what it has at
/// least.
fn to_record(v: Value, rt: &zu_common::RecordType) -> Result<Value> {
    let Value::Record(fields) = v else {
        return Err(forbidden_named(&v, "RECORD"));
    };
    let mut out = Vec::with_capacity(fields.len());
    for declared in &rt.fields {
        let Some(value) = fields
            .iter()
            .find(|(name, _)| *name == declared.name)
            .map(|(_, value)| value.clone())
        else {
            return Err(ZuError::gql(
                codes::C22G0Y,
                format!("the record has no field '{}'", declared.name),
            ));
        };
        let cast = cast(value, &declared.ty).map_err(|e| {
            ZuError::gql(
                codes::C22G0X,
                format!("field '{}' of the record: {e}", declared.name),
            )
        })?;
        out.push((declared.name.clone(), cast));
    }
    if rt.open {
        for (name, value) in fields {
            if rt.field(&name).is_none() {
                out.push((name, value));
            }
        }
    }
    Ok(Value::record(out))
}

/// `22018 invalid character value for cast`, the condition for a
/// character string whose spelling the target type does not accept.
///
/// The line between this and [`forbidden`] is the line the two ISO
/// conditions draw. 22018 is a fact about the characters, so a string
/// is the only value that raises it, and a value of a kind the target
/// does not take at all is 22G03 whatever its spelling would have been.
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

/// A truth value, GV27.
///
/// An exact number of zero or one is the two truth values written the
/// way most call sites write them, and any other exact number is the
/// right kind of value carrying the wrong one, which is what 22003
/// says. An approximate number is not accepted at all: 0.9999999999 is
/// not one, no rounding rule in the standard makes it one, and a cast
/// that rounded it would answer a question nobody asked.
fn to_bool(v: Value) -> Result<Value> {
    Ok(Value::Bool(match &v {
        Value::Bool(b) => *b,
        Value::Int(0) => false,
        Value::Int(1) => true,
        Value::Int(n) => return Err(out_of_range(n.to_string(), "BOOL")),
        Value::Str(s) if s.eq_ignore_ascii_case("true") => true,
        Value::Str(s) if s.eq_ignore_ascii_case("false") => false,
        Value::Str(_) => return Err(not_castable(&v, "a boolean")),
        other => return Err(forbidden_named(other, "BOOL")),
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
        other => return Err(forbidden_named(other, &name(signed, bits))),
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
        other => return Err(forbidden_named(other, &format!("FLOAT{}", bits.bits()))),
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
    let spelled = format!("DECIMAL({precision}, {scale})");
    let text = match &v {
        Value::Str(s) => s.trim().to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => u8::from(*b).to_string(),
        // A float is already inexact, so it is scaled and rounded the
        // same way a written number is, and the digit count is then
        // checked on the result.
        Value::Float(f) if f.is_finite() => format!("{f}"),
        // An infinity has no exact spelling to round, which makes it
        // the range's own condition rather than a wrong kind of value.
        Value::Float(_) => return Err(out_of_range(show(&v), &spelled)),
        other => return Err(forbidden_named(other, &spelled)),
    };
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
        // A temporal value prints the way it was written and the way a
        // result row prints it, which is the one formatter, so a cast
        // to a string and the cell in the answer can never disagree.
        Value::Temporal(t) => t.to_string(),
        other => return Err(forbidden_named(other, "STRING")),
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
    use zu_common::{DurationKind, Field, RecordType};

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

    /// The cast matrix, one row per kind of value and one column per
    /// member of the type lattice, in the order [`sources`] and
    /// [`targets`] list them:
    ///
    /// ```text
    ///  1 NULL       10 LOCAL TIME       19 BINDING TABLE
    ///  2 NOTHING    11 ZONED TIME       20 PATH
    ///  3 BOOL       12 LOCAL DATETIME   21 LIST<INT64>
    ///  4 INT64      13 ZONED DATETIME   22 RECORD { a :: INT64 }
    ///  5 DECIMAL    14 DURATION         23 ANY
    ///  6 FLOAT64    15 NODE             24 ANY PROPERTY VALUE
    ///  7 STRING     16 NODE Person      25 ANY<INT64|STRING>
    ///  8 BYTES      17 EDGE             26 INT64 nullable
    ///  9 DATE       18 GRAPH
    /// ```
    ///
    /// A cell is `.` where the cast succeeds and the condition it
    /// raises where it does not. Writing the whole matrix out is the
    /// point rather than a chore: a rule reads plausibly one pair at a
    /// time, and it is the column read top to bottom that shows the
    /// pair nobody thought about.
    const MATRIX: &[(&str, &str)] = &[
        (
            "null",
            ". 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 22004 .",
        ),
        (
            "bool",
            "22G03 22G03 . . . . . 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . .",
        ),
        (
            "int",
            "22G03 22G03 . . . . . 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . .",
        ),
        (
            "float",
            "22G03 22G03 22G03 . . . . 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . .",
        ),
        (
            "'1'",
            "22G03 22G03 22018 . . . . 22G03 22007 22007 22007 22007 22007 22G0H 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . .",
        ),
        (
            "'true'",
            "22G03 22G03 . 22018 22018 22018 . 22G03 22007 22007 22007 22007 22007 22G0H 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22018",
        ),
        (
            "'2024-01-15'",
            "22G03 22G03 22018 22018 22018 22018 . 22G03 . 22007 22007 . . 22G0H 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22018",
        ),
        (
            "list",
            "22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V . 22G03 . . 22G03 22G03",
        ),
        (
            "record",
            "22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 . . 22G03 22G03 22G03",
        ),
        (
            "node",
            "22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 . 22G0W 22G0V 22G03 22G03 22G0V 22G03 22G03 . 22G03 22G03 22G03",
        ),
        (
            "edge",
            "22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G0V 22G0V . 22G03 22G03 22G0V 22G03 22G03 . 22G03 22G03 22G03",
        ),
        (
            "path",
            "22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 . 22G03 22G03 . 22G03 22G03 22G03",
        ),
        (
            "date",
            "22G03 22G03 22G03 22G03 22G03 22G03 . 22G03 . 22G03 22G03 . . 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22G03",
        ),
        (
            "time",
            "22G03 22G03 22G03 22G03 22G03 22G03 . 22G03 22G03 . . 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22G03",
        ),
        (
            "ztime",
            "22G03 22G03 22G03 22G03 22G03 22G03 . 22G03 22G03 . . 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22G03",
        ),
        (
            "datetime",
            "22G03 22G03 22G03 22G03 22G03 22G03 . 22G03 . . . . . 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22G03",
        ),
        (
            "zdatetime",
            "22G03 22G03 22G03 22G03 22G03 22G03 . 22G03 . . . . . 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22G03",
        ),
        (
            "duration",
            "22G03 22G03 22G03 22G03 22G03 22G03 . 22G03 22G03 22G03 22G03 22G03 22G03 . 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22G03",
        ),
        (
            "months",
            "22G03 22G03 22G03 22G03 22G03 22G03 . 22G03 22G03 22G03 22G03 22G03 22G03 22G03 22G0V 22G0V 22G0V 22G03 22G03 22G0V 22G03 22G03 . . . 22G03",
        ),
    ];

    /// One value of every kind the executor carries, in the row order
    /// of [`MATRIX`].
    ///
    /// A character string appears three times because the target type
    /// decides only half of a string's cell: what the characters spell
    /// decides the other half, and the three rows are the three answers
    /// that gives.
    fn sources() -> Vec<Value> {
        vec![
            Value::Null,
            Value::Bool(true),
            Value::Int(1),
            Value::Float(1.5),
            Value::Str("1".into()),
            Value::Str("true".into()),
            Value::Str("2024-01-15".into()),
            Value::List(vec![Value::Int(1)]),
            Value::record(vec![("a".into(), Value::Int(1))]),
            Value::Node {
                table: 0,
                offset: 0,
            },
            Value::Rel {
                table: 0,
                src: 0,
                dst: 1,
            },
            Value::Path(vec![Value::Node {
                table: 0,
                offset: 0,
            }]),
            Value::Temporal(Temporal::Date(19738)),
            Value::Temporal(Temporal::LocalTime(3_600_000_000_000)),
            Value::Temporal(Temporal::ZonedTime {
                nanos: 3_600_000_000_000,
                offset: 60,
            }),
            Value::Temporal(Temporal::LocalDatetime(1_705_276_800_000_000_000)),
            Value::Temporal(Temporal::ZonedDatetime {
                nanos: 1_705_276_800_000_000_000,
                offset: 60,
            }),
            Value::Temporal(Temporal::Duration(DurationKind::DayTime, NANOS_PER_DAY)),
            Value::Temporal(Temporal::Duration(DurationKind::YearMonth, 14)),
        ]
    }

    /// One type of every member of the lattice, in the column order of
    /// [`MATRIX`].
    fn targets() -> Vec<LogicalType> {
        vec![
            LogicalType::Null,
            LogicalType::Nothing,
            LogicalType::Bool,
            ty("INT64"),
            LogicalType::Decimal {
                precision: 10,
                scale: 2,
            },
            ty("FLOAT64"),
            ty("STRING"),
            LogicalType::Bytes {
                min: None,
                max: None,
                fixed: false,
            },
            LogicalType::Date,
            LogicalType::LocalTime,
            LogicalType::ZonedTime,
            LogicalType::LocalDatetime,
            LogicalType::ZonedDatetime,
            LogicalType::Duration(DurationKind::DayTime),
            LogicalType::Node(None),
            LogicalType::Node(Some("Person".into())),
            LogicalType::Edge(None),
            LogicalType::Graph(None),
            LogicalType::BindingTable(None),
            LogicalType::Path(None),
            LogicalType::List {
                elem: Box::new(ty("INT64")),
                max: None,
            },
            LogicalType::Record(RecordType::closed(vec![Field {
                name: "a".into(),
                ty: ty("INT64"),
            }])),
            LogicalType::Any,
            LogicalType::AnyProperty,
            LogicalType::Union(vec![ty("INT64"), ty("STRING")]),
            LogicalType::Nullable(Box::new(ty("INT64"))),
        ]
    }

    /// The column name of a target type.
    ///
    /// The match is exhaustive on purpose and so is the one in [`cast`]
    /// itself. A new member of the lattice stops both compiling, and
    /// the only way to clear it is to say which column the member is
    /// and then fill that column in for every row, which is the fifteen
    /// cells nobody would have thought about on their own.
    fn column(ty: &LogicalType) -> &'static str {
        match ty {
            LogicalType::Null => "NULL",
            LogicalType::Nothing => "NOTHING",
            LogicalType::Bool => "BOOL",
            LogicalType::Int { .. } => "INT",
            LogicalType::Decimal { .. } => "DECIMAL",
            LogicalType::Float { .. } => "FLOAT",
            LogicalType::Str { .. } => "STRING",
            LogicalType::Bytes { .. } => "BYTES",
            LogicalType::Date => "DATE",
            LogicalType::LocalTime => "LOCAL TIME",
            LogicalType::ZonedTime => "ZONED TIME",
            LogicalType::LocalDatetime => "LOCAL DATETIME",
            LogicalType::ZonedDatetime => "ZONED DATETIME",
            LogicalType::Duration(_) => "DURATION",
            LogicalType::Node(None) => "NODE",
            LogicalType::Node(Some(_)) => "NODE of a named type",
            LogicalType::Edge(None) => "EDGE",
            LogicalType::Edge(Some(_)) => "EDGE of a named type",
            LogicalType::Graph(_) => "GRAPH",
            LogicalType::BindingTable(_) => "BINDING TABLE",
            LogicalType::Path(None) => "PATH",
            LogicalType::Path(Some(_)) => "PATH of a named type",
            LogicalType::List { .. } => "LIST",
            LogicalType::Record(_) => "RECORD",
            LogicalType::Any => "ANY",
            LogicalType::AnyProperty => "ANY PROPERTY VALUE",
            LogicalType::Union(_) => "a closed union",
            LogicalType::Nullable(_) => "a nullable type",
        }
    }

    /// The row name of a value, exhaustive for the same reason
    /// [`column`] is. `Chain` has no row because it is the pipeline's
    /// internal form of a path and is settled into one before any value
    /// reaches an expression.
    fn row_label(v: &Value) -> &'static str {
        match v {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "a string",
            Value::List(_) => "list",
            Value::Record(_) => "record",
            Value::Node { .. } => "node",
            Value::Rel { .. } => "edge",
            Value::Path(_) => "path",
            Value::Temporal(_) => "a temporal",
            Value::Chain(_) => "chain",
        }
    }

    /// Every cell of the matrix, checked against the row it is written
    /// on.
    #[test]
    fn every_pair_of_a_value_and_a_type_has_the_answer_the_matrix_states() {
        let sources = sources();
        let targets = targets();
        assert_eq!(MATRIX.len(), sources.len(), "a row per value");
        for (row, (label, cells)) in MATRIX.iter().enumerate() {
            let value = &sources[row];
            let cells: Vec<&str> = cells.split_whitespace().collect();
            assert_eq!(cells.len(), targets.len(), "row '{label}' is short");
            for (at, want) in cells.iter().enumerate() {
                let target = &targets[at];
                let seen = format!("{label} to {}", column(target));
                match (*want, cast(value.clone(), target)) {
                    (".", Ok(_)) => {}
                    (".", Err(e)) => panic!("{seen} should cast, raised {}", status(&e)),
                    (code, Ok(out)) => panic!("{seen} should raise {code}, gave {out:?}"),
                    (code, Err(e)) => assert_eq!(status(&e), code, "{seen}"),
                }
            }
        }
    }

    /// The grid and the values it describes cannot drift apart, and no
    /// column is written twice, which would leave the one it displaced
    /// unchecked.
    #[test]
    fn the_matrix_names_its_own_rows_and_columns() {
        for ((label, _), value) in MATRIX.iter().zip(sources()) {
            let named = row_label(&value);
            assert!(
                label.starts_with(named) || named == "a string" || named == "a temporal",
                "row '{label}' holds a {named}"
            );
        }
        let mut names: Vec<&str> = targets().iter().map(column).collect();
        let written = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), written, "a column is written twice");
    }
}
