//! `expr IS TYPED type`, ISO's GA06.
//!
//! The predicate asks whether a value belongs to a type, which is not
//! the question a cast asks. `'42'` casts to `INT` and is not one, so
//! nothing here may be written in terms of [`crate::cast`]: a cast is a
//! conversion that may fail and this is a membership test that always
//! answers.
//!
//! Two answers are worth stating because they are the ones an engine
//! gets wrong quietly. The predicate never returns null, not even for a
//! null value: asking whether the null value is of a nullable type has
//! an answer and the answer is yes, and a type written `NOT NULL` is
//! how a query asks the other question. And the empty type is empty,
//! so `NULL IS TYPED NOTHING` is false even though every other type
//! written without `NOT NULL` admits a null.
//!
//! What is missing here is missing from the value model rather than
//! from this module. zu has no byte string value and no record value,
//! so a byte string type and a record type are inhabited by nothing at
//! runtime, and the predicate says false rather than pretending. Those
//! answers change when the values arrive, and the corpus cases that ask
//! them are written over integers so they stay honest either way.

use zu_common::{IntBits, LogicalType};

use crate::exec::Value;

/// Whether `v` is a value of type `ty`.
pub fn is_of(v: &Value, ty: &LogicalType) -> bool {
    if matches!(v, Value::Null) {
        return null_is_of(ty);
    }
    material(v, ty)
}

/// The null value belongs to every nullable type, to the null type
/// itself, and to no other. The wrapper is the whole test, which is why
/// nullability is a wrapper: a flag on each variant would be a rule
/// each variant could forget.
fn null_is_of(ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Nullable(inner) => !matches!(**inner, LogicalType::Nothing),
        LogicalType::Null | LogicalType::Any | LogicalType::AnyProperty => true,
        LogicalType::Union(members) => members.iter().any(null_is_of),
        _ => false,
    }
}

/// Whether a value that is not null belongs to `ty`.
fn material(v: &Value, ty: &LogicalType) -> bool {
    match ty {
        LogicalType::Nullable(inner) => material(v, inner),
        LogicalType::Union(members) => members.iter().any(|m| material(v, m)),
        LogicalType::Any => true,
        // GV68. A property holds a scalar, and the reference and
        // constructed types are the ones it cannot hold.
        LogicalType::AnyProperty => matches!(
            v,
            Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Str(_)
        ),
        // GV71 and GV72. The null type has one value and this is not
        // it, and the empty type has none.
        LogicalType::Null | LogicalType::Nothing => false,

        LogicalType::Bool => matches!(v, Value::Bool(_)),
        LogicalType::Int {
            signed,
            bits,
            precision,
        } => match v {
            Value::Int(i) => in_range(i128::from(*i), *signed, *bits, *precision),
            _ => false,
        },
        // A decimal's declared digits bound the value the same way an
        // integer's do, and an integer is an exact number with a scale
        // of zero, so it belongs to a decimal type wide enough for it.
        LogicalType::Decimal { precision, scale } => match v {
            Value::Int(i) => fits_digits(i128::from(*i), precision.saturating_sub(*scale)),
            _ => false,
        },
        LogicalType::Float { .. } => matches!(v, Value::Float(_)),
        LogicalType::Str { min, max, .. } => match v {
            Value::Str(s) => within(s.chars().count(), *min, *max),
            _ => false,
        },
        LogicalType::Bytes { .. } => false,

        LogicalType::Date
        | LogicalType::LocalTime
        | LogicalType::LocalDatetime
        | LogicalType::ZonedTime
        | LogicalType::ZonedDatetime
        | LogicalType::Duration(_) => false,

        LogicalType::Node(_) => matches!(v, Value::Node { .. }),
        LogicalType::Edge(_) => matches!(v, Value::Rel { .. }),
        LogicalType::Graph(_) | LogicalType::BindingTable(_) => false,

        LogicalType::Path(_) => matches!(v, Value::Path(_)),
        // A list belongs to a list type when every element belongs to
        // the element type, which makes the empty list a member of
        // every list type and is the answer ISO wants.
        LogicalType::List { elem, max } => match v {
            Value::List(items) => {
                max.is_none_or(|m| items.len() <= m as usize)
                    && items.iter().all(|item| is_of(item, elem))
            }
            _ => false,
        },
        LogicalType::Record(_) => false,
    }
}

/// Whether `n` is inside the width and inside the declared digit count,
/// which are two bounds and the tighter one wins.
fn in_range(n: i128, signed: bool, bits: IntBits, precision: Option<u16>) -> bool {
    let within_width = if bits >= IntBits::B128 {
        true
    } else {
        let width = u32::from(bits.bits());
        if signed {
            n >= -(1i128 << (width - 1)) && n < (1i128 << (width - 1))
        } else {
            n >= 0 && n < (1i128 << width)
        }
    };
    within_width && precision.is_none_or(|digits| fits_digits(n, digits))
}

/// Whether `n` is written in `digits` decimal digits or fewer.
fn fits_digits(n: i128, digits: u16) -> bool {
    match 10i128.checked_pow(u32::from(digits)) {
        Some(limit) => n > -limit && n < limit,
        // Past the carrier's own range every value fits, because no
        // value is that wide.
        None => true,
    }
}

/// Whether a length is inside a pair of optional bounds.
fn within(len: usize, min: Option<u32>, max: Option<u32>) -> bool {
    min.is_none_or(|m| len >= m as usize) && max.is_none_or(|m| len <= m as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zu_common::RecordType;

    fn nullable(ty: LogicalType) -> LogicalType {
        LogicalType::Nullable(Box::new(ty))
    }

    fn int() -> LogicalType {
        LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: None,
        }
    }

    #[test]
    fn the_null_value_belongs_to_a_nullable_type_and_to_the_null_type() {
        assert!(is_of(&Value::Null, &nullable(int())));
        assert!(!is_of(&Value::Null, &int()));
        assert!(is_of(&Value::Null, &nullable(LogicalType::Null)));
        assert!(!is_of(&Value::Int(1), &nullable(LogicalType::Null)));
    }

    /// The empty type is the one type a null does not belong to, which
    /// is the whole of GV72 and the case a nullability wrapper would
    /// get wrong if it were checked first.
    #[test]
    fn no_value_at_all_belongs_to_the_empty_type() {
        assert!(!is_of(&Value::Null, &nullable(LogicalType::Nothing)));
        assert!(!is_of(&Value::Int(1), &nullable(LogicalType::Nothing)));
    }

    #[test]
    fn a_width_and_a_declared_precision_both_bound_the_membership() {
        let int8 = LogicalType::Int {
            signed: true,
            bits: IntBits::B8,
            precision: None,
        };
        assert!(is_of(&Value::Int(127), &int8));
        assert!(!is_of(&Value::Int(128), &int8));
        let declared = LogicalType::Int {
            signed: true,
            bits: IntBits::B32,
            precision: Some(3),
        };
        assert!(is_of(&Value::Int(999), &declared));
        assert!(!is_of(&Value::Int(1000), &declared));
    }

    /// A cast reads a string as a number and the predicate does not,
    /// which is the difference between converting and belonging.
    #[test]
    fn a_string_that_casts_to_an_integer_is_still_not_one() {
        assert!(!is_of(&Value::Str("42".into()), &nullable(int())));
        assert!(is_of(
            &Value::Str("42".into()),
            &nullable(LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            })
        ));
    }

    #[test]
    fn a_union_admits_what_any_of_its_members_admits() {
        let either = LogicalType::Union(vec![
            int(),
            LogicalType::Str {
                min: None,
                max: None,
                fixed: false,
            },
        ]);
        assert!(is_of(&Value::Int(1), &either));
        assert!(is_of(&Value::Str("x".into()), &either));
        assert!(!is_of(&Value::Bool(true), &either));
        assert!(!is_of(&Value::Null, &either));
        assert!(is_of(&Value::Null, &nullable(either)));
    }

    #[test]
    fn the_open_union_admits_everything_and_a_record_type_admits_nothing_yet() {
        assert!(is_of(&Value::Int(1), &LogicalType::Any));
        assert!(is_of(&Value::Null, &LogicalType::Any));
        assert!(is_of(&Value::Int(1), &LogicalType::AnyProperty));
        assert!(!is_of(
            &Value::Int(1),
            &nullable(LogicalType::Record(RecordType::open(Vec::new())))
        ));
    }
}
