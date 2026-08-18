//! Value type names, the spelling half of the type lattice.
//!
//! ISO gives the integer tower nineteen optional features and about as
//! many spellings, because `SMALLINT` and `INT16` are the same type
//! asked for twice and `INT(9)` is a third way of asking. The lattice
//! in `zu_common` is the answer to all of them, so what lives here is
//! only the mapping from a name to a [`LogicalType`], kept as a table
//! rather than a chain of parser branches.
//!
//! A name is a family and an arity, not a type on its own. `STRING`,
//! `STRING(10)` and `STRING(2, 10)` are three features and one row,
//! and the row says what each count of arguments means, so adding a
//! spelling is adding a row and never a branch.
//!
//! Nullability is the one part that is not a name. A cast target is
//! nullable unless it says `NOT NULL`, which is why the parser wraps
//! what this module returns rather than this module doing it: the
//! wrapper belongs to the syntax that asked for it.

use zu_common::{DurationKind, FloatBits, IntBits, LogicalType};

/// What a name means once its arguments are known.
///
/// The families differ in what an argument counts. An integer's
/// argument is decimal digits, a string's is characters and a byte
/// string's is octets, and a decimal takes two arguments that count
/// different things, so the arity rules cannot be shared.
enum Family {
    /// A name that takes no arguments at all.
    Simple(LogicalType),
    /// `INT`, `UINT8` and the rest of the tower. One argument is the
    /// declared decimal digit count of GV09.
    Int { signed: bool, bits: IntBits },
    /// `FLOAT` and the widths. One argument is GV22's precision.
    Float(FloatBits),
    /// `DECIMAL`, GV17. One argument is the precision, two are the
    /// precision and the scale.
    Decimal,
    /// A character string type, GV30 to GV32. `fixed` is `CHAR`, whose
    /// one argument is both bounds at once.
    Chars { fixed: bool },
    /// A byte string type, GV35 to GV38, counted in octets.
    Octets { fixed: bool },
}

/// The default precision of a bare `DECIMAL`.
///
/// ISO leaves it implementation defined. 38 digits is the widest a
/// 128 bit unscaled integer holds in full, which is the widest carrier
/// the lattice has for one, so it is the largest promise zu can keep.
const DECIMAL_DIGITS: u16 = 38;

/// Every spelling, in the order the features are numbered.
static NAMES: &[(&str, Family)] = &[
    ("INT8", int(true, IntBits::B8)),
    ("INT16", int(true, IntBits::B16)),
    ("INT32", int(true, IntBits::B32)),
    ("INT64", int(true, IntBits::B64)),
    ("INT128", int(true, IntBits::B128)),
    ("INT256", int(true, IntBits::B256)),
    ("UINT8", int(false, IntBits::B8)),
    ("UINT16", int(false, IntBits::B16)),
    ("UINT32", int(false, IntBits::B32)),
    ("UINT64", int(false, IntBits::B64)),
    ("UINT128", int(false, IntBits::B128)),
    ("UINT256", int(false, IntBits::B256)),
    // The word spellings. ISO makes these features of their own, GV05,
    // GV08, GV10, GV18 and GV19, and they are aliases of widths above.
    ("SMALLINT", int(true, IntBits::B16)),
    ("USMALLINT", int(false, IntBits::B16)),
    ("INT", int(true, IntBits::B32)),
    ("INTEGER", int(true, IntBits::B32)),
    ("UINT", int(false, IntBits::B32)),
    ("BIGINT", int(true, IntBits::B64)),
    ("UBIGINT", int(false, IntBits::B64)),
    ("DECIMAL", Family::Decimal),
    ("DEC", Family::Decimal),
    ("NUMERIC", Family::Decimal),
    ("FLOAT16", Family::Float(FloatBits::B16)),
    ("FLOAT32", Family::Float(FloatBits::B32)),
    ("FLOAT64", Family::Float(FloatBits::B64)),
    ("FLOAT128", Family::Float(FloatBits::B128)),
    ("FLOAT256", Family::Float(FloatBits::B256)),
    ("REAL", Family::Float(FloatBits::B32)),
    ("DOUBLE", Family::Float(FloatBits::B64)),
    // A handful of names are two words. The parser joins a pair with a
    // single space and looks the pair up before the first word alone,
    // so DOUBLE PRECISION is one name and a bare DOUBLE is another.
    ("DOUBLE PRECISION", Family::Float(FloatBits::B64)),
    ("FLOAT", Family::Float(FloatBits::B64)),
    ("BOOL", Family::Simple(LogicalType::Bool)),
    ("BOOLEAN", Family::Simple(LogicalType::Bool)),
    ("STRING", Family::Chars { fixed: false }),
    ("VARCHAR", Family::Chars { fixed: false }),
    ("CHAR", Family::Chars { fixed: true }),
    ("BYTES", Family::Octets { fixed: false }),
    ("VARBINARY", Family::Octets { fixed: false }),
    ("BINARY", Family::Octets { fixed: true }),
    // GV39 and GV40. A bare TIME or DATETIME carries a zone, which is
    // GQL's default and the opposite of the one most engines picked,
    // so the local types are the ones that have to say so.
    ("DATE", Family::Simple(LogicalType::Date)),
    ("LOCAL TIME", Family::Simple(LogicalType::LocalTime)),
    ("LOCAL DATETIME", Family::Simple(LogicalType::LocalDatetime)),
    (
        "LOCAL TIMESTAMP",
        Family::Simple(LogicalType::LocalDatetime),
    ),
    ("ZONED TIME", Family::Simple(LogicalType::ZonedTime)),
    ("TIME", Family::Simple(LogicalType::ZonedTime)),
    ("ZONED DATETIME", Family::Simple(LogicalType::ZonedDatetime)),
    ("DATETIME", Family::Simple(LogicalType::ZonedDatetime)),
    ("TIMESTAMP", Family::Simple(LogicalType::ZonedDatetime)),
    // GV41 and GV42. The two duration kinds do not mix, and a bare
    // DURATION is the day time one, which is the reading every engine
    // that spells it DURATION already has.
    (
        "DURATION",
        Family::Simple(LogicalType::Duration(DurationKind::DayTime)),
    ),
    (
        "INTERVAL",
        Family::Simple(LogicalType::Duration(DurationKind::DayTime)),
    ),
    // GV55, GV60, GV61 and the two immaterial types, GV71 and GV72.
    ("PATH", Family::Simple(LogicalType::Path(None))),
    ("GRAPH", Family::Simple(LogicalType::Graph(None))),
    // `PROPERTY GRAPH` is the long spelling of the same type and says
    // nothing the short one does not, which is also how a `USE` clause
    // reads the pair.
    ("PROPERTY GRAPH", Family::Simple(LogicalType::Graph(None))),
    (
        "BINDING TABLE",
        Family::Simple(LogicalType::BindingTable(None)),
    ),
    ("TABLE", Family::Simple(LogicalType::BindingTable(None))),
    ("NULL", Family::Simple(LogicalType::Null)),
    ("NOTHING", Family::Simple(LogicalType::Nothing)),
];

const fn int(signed: bool, bits: IntBits) -> Family {
    Family::Int { signed, bits }
}

/// The type `name` spells with `args` written after it, `None` when it
/// spells nothing or takes a different number of arguments.
///
/// The argument list is what the parser read between the parentheses,
/// so an empty slice is the bare name and not a name written `()`.
pub fn spelled(name: &str, args: &[u32]) -> Option<LogicalType> {
    let family = &NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name))?.1;
    let digits = |ix: usize| u16::try_from(args[ix]).ok();
    Some(match (family, args.len()) {
        (Family::Simple(t), 0) => t.clone(),

        (Family::Int { signed, bits }, 0) => LogicalType::Int {
            signed: *signed,
            bits: *bits,
            precision: None,
        },
        // `INT(p)` counts decimal digits, so it picks the narrowest
        // width that holds `p` of them and keeps `p` itself, because
        // the digit count is a check the engine owes the user rather
        // than a layout: `INT(3)` is sixteen bits and still refuses
        // 999 + 1.
        (Family::Int { signed, .. }, 1) => {
            let precision = digits(0)?;
            LogicalType::Int {
                signed: *signed,
                bits: IntBits::for_digits(precision)?,
                precision: Some(precision),
            }
        }

        (Family::Float(bits), 0) => LogicalType::Float {
            bits: *bits,
            precision: None,
        },
        (Family::Float(_), 1) => {
            let precision = digits(0)?;
            LogicalType::Float {
                bits: FloatBits::for_digits(precision)?,
                precision: Some(precision),
            }
        }

        (Family::Decimal, 0) => LogicalType::Decimal {
            precision: DECIMAL_DIGITS,
            scale: 0,
        },
        (Family::Decimal, 1) => LogicalType::Decimal {
            precision: digits(0)?,
            scale: 0,
        },
        (Family::Decimal, 2) => {
            let (precision, scale) = (digits(0)?, digits(1)?);
            // A scale past the precision would name digits after the
            // point that the number has no room for.
            if scale > precision || precision > DECIMAL_DIGITS {
                return None;
            }
            LogicalType::Decimal { precision, scale }
        }

        // A fixed length is both bounds at once, which is the whole of
        // GV32: nothing downstream has to ask whether a type is fixed,
        // because a minimum equal to the maximum already says it.
        (Family::Chars { fixed: true }, n @ (0 | 1)) => {
            let len = if n == 0 { 1 } else { args[0] };
            LogicalType::Str {
                min: Some(len),
                max: Some(len),
                fixed: true,
            }
        }
        (Family::Chars { fixed: false }, n @ (0..=2)) => LogicalType::Str {
            min: if n == 2 { Some(args[0]) } else { None },
            max: match n {
                0 => None,
                1 => Some(args[0]),
                _ => Some(args[1]),
            },
            fixed: false,
        },

        (Family::Octets { fixed: true }, n @ (0 | 1)) => {
            let len = if n == 0 { 1 } else { args[0] };
            LogicalType::Bytes {
                min: Some(len),
                max: Some(len),
                fixed: true,
            }
        }
        (Family::Octets { fixed: false }, n @ (0..=2)) => LogicalType::Bytes {
            min: if n == 2 { Some(args[0]) } else { None },
            max: match n {
                0 => None,
                1 => Some(args[0]),
                _ => Some(args[1]),
            },
            fixed: false,
        },

        _ => return None,
    })
}

/// Whether a name is one this module knows at all, which is how the
/// parser tells an unknown type from a known one written wrong.
pub fn is_type_name(name: &str) -> bool {
    NAMES.iter().any(|(n, _)| n.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str) -> Option<LogicalType> {
        spelled(name, &[])
    }

    #[test]
    fn the_word_spellings_are_the_widths_they_alias() {
        assert_eq!(t("smallint"), t("INT16"));
        assert_eq!(t("BIGINT"), t("int64"));
        assert_eq!(t("UBIGINT"), t("UINT64"));
        assert_eq!(t("integer"), t("INT"));
        assert_eq!(t("double"), t("FLOAT64"));
        assert_eq!(t("double precision"), t("FLOAT64"));
        assert_eq!(t("nosuchtype"), None);
    }

    /// The declared digit count is not the width. Nine digits fit in 32
    /// bits and the type says nine, so a later check can refuse a tenth
    /// digit that the width alone would have accepted, and three digits
    /// take sixteen bits because eight hold only two in full.
    #[test]
    fn a_declared_precision_narrows_the_check_not_only_the_width() {
        assert_eq!(
            spelled("INT", &[9]),
            Some(LogicalType::Int {
                signed: true,
                bits: IntBits::B32,
                precision: Some(9),
            })
        );
        assert_eq!(
            spelled("INT", &[3]),
            Some(LogicalType::Int {
                signed: true,
                bits: IntBits::B16,
                precision: Some(3),
            })
        );
        assert_eq!(spelled("BOOL", &[3]), None);
        assert_eq!(spelled("INT", &[3, 3]), None);
    }

    #[test]
    fn a_length_is_a_pair_of_bounds_and_a_fixed_length_sets_both() {
        assert_eq!(
            spelled("STRING", &[10]),
            Some(LogicalType::Str {
                min: None,
                max: Some(10),
                fixed: false,
            })
        );
        assert_eq!(
            spelled("STRING", &[2, 10]),
            Some(LogicalType::Str {
                min: Some(2),
                max: Some(10),
                fixed: false,
            })
        );
        assert_eq!(
            spelled("CHAR", &[3]),
            Some(LogicalType::Str {
                min: Some(3),
                max: Some(3),
                fixed: true,
            })
        );
        assert_eq!(
            spelled("BINARY", &[4]),
            Some(LogicalType::Bytes {
                min: Some(4),
                max: Some(4),
                fixed: true,
            })
        );
        assert_eq!(spelled("CHAR", &[2, 10]), None);
    }

    #[test]
    fn a_decimal_takes_a_precision_and_a_scale_in_that_order() {
        assert_eq!(
            spelled("DECIMAL", &[5, 2]),
            Some(LogicalType::Decimal {
                precision: 5,
                scale: 2,
            })
        );
        assert_eq!(spelled("NUMERIC", &[5]), spelled("DEC", &[5, 0]));
        // A scale past the precision names digits the number cannot
        // hold, and 39 digits is past what the carrier holds at all.
        assert_eq!(spelled("DECIMAL", &[2, 5]), None);
        assert_eq!(spelled("DECIMAL", &[39, 0]), None);
    }

    /// GV60 and GV61 each have a long spelling and a short one, and the
    /// pair names one type, the same way a `USE` clause reads `GRAPH`
    /// and `PROPERTY GRAPH` as the one word.
    #[test]
    fn a_reference_type_has_two_spellings_and_one_meaning() {
        assert_eq!(t("GRAPH"), Some(LogicalType::Graph(None)));
        assert_eq!(t("PROPERTY GRAPH"), t("GRAPH"));
        assert_eq!(t("BINDING TABLE"), Some(LogicalType::BindingTable(None)));
        assert_eq!(t("TABLE"), t("BINDING TABLE"));
        assert_eq!(t("GRAPH TABLE"), None);
    }
}
