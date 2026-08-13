//! Value type names, the spelling half of the type lattice.
//!
//! ISO gives the integer tower nineteen optional features and about as
//! many spellings, because `SMALLINT` and `INT16` are the same type
//! asked for twice and `INT(9)` is a third way of asking. The lattice
//! in `zu_common` is the answer to all of them, so what lives here is
//! only the mapping from a name to a [`LogicalType`], kept as a table
//! rather than a chain of parser branches.
//!
//! Nullability is the one part that is not a name. A cast target is
//! nullable unless it says `NOT NULL`, which is why the parser wraps
//! what this module returns rather than this module doing it: the
//! wrapper belongs to the syntax that asked for it.

use zu_common::{FloatBits, IntBits, LogicalType};

/// The types a name on its own can spell, without a parenthesised
/// argument.
///
/// The unsigned tower has no `SIGNED`/`UNSIGNED` prefixed spellings
/// here because ISO writes those as a separate production and nothing
/// in the corpus reaches for them yet; adding them is another row.
static NAMES: [(&str, LogicalType); 30] = [
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
    ("FLOAT16", float(FloatBits::B16)),
    ("FLOAT32", float(FloatBits::B32)),
    ("FLOAT64", float(FloatBits::B64)),
    ("FLOAT128", float(FloatBits::B128)),
    ("FLOAT256", float(FloatBits::B256)),
    ("REAL", float(FloatBits::B32)),
    ("DOUBLE", float(FloatBits::B64)),
    ("FLOAT", float(FloatBits::B64)),
    ("BOOL", LogicalType::Bool),
    ("BOOLEAN", LogicalType::Bool),
    (
        "STRING",
        LogicalType::Str {
            min: None,
            max: None,
            fixed: false,
        },
    ),
];

const fn int(signed: bool, bits: IntBits) -> LogicalType {
    LogicalType::Int {
        signed,
        bits,
        precision: None,
    }
}

const fn float(bits: FloatBits) -> LogicalType {
    LogicalType::Float {
        bits,
        precision: None,
    }
}

/// The type a bare name spells, `None` when it spells nothing.
pub fn by_name(name: &str) -> Option<LogicalType> {
    NAMES
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, t)| t.clone())
}

/// The type `name(precision)` spells, for the parenthesised forms.
///
/// `INT(p)` is GV09 and `p` counts decimal digits, so it picks the
/// narrowest width that holds `p` digits and keeps `p` itself, because
/// the digit count is a check the engine owes the user rather than a
/// layout: `INT(3)` is sixteen bits wide and still refuses 999 + 1.
pub fn by_name_with_precision(name: &str, precision: u16) -> Option<LogicalType> {
    match by_name(name)? {
        LogicalType::Int { signed, .. } => Some(LogicalType::Int {
            signed,
            bits: IntBits::for_digits(precision)?,
            precision: Some(precision),
        }),
        LogicalType::Float { .. } => Some(LogicalType::Float {
            bits: FloatBits::for_digits(precision)?,
            precision: Some(precision),
        }),
        LogicalType::Str { fixed, .. } => Some(LogicalType::Str {
            min: None,
            max: Some(u32::from(precision)),
            fixed,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_word_spellings_are_the_widths_they_alias() {
        assert_eq!(by_name("smallint"), by_name("INT16"));
        assert_eq!(by_name("BIGINT"), by_name("int64"));
        assert_eq!(by_name("UBIGINT"), by_name("UINT64"));
        assert_eq!(by_name("integer"), by_name("INT"));
        assert_eq!(by_name("double"), by_name("FLOAT64"));
        assert_eq!(by_name("nosuchtype"), None);
    }

    /// The declared digit count is not the width. Nine digits fit in 32
    /// bits and the type says nine, so a later check can refuse a tenth
    /// digit that the width alone would have accepted, and three digits
    /// take sixteen bits because eight hold only two in full.
    #[test]
    fn a_declared_precision_narrows_the_check_not_only_the_width() {
        assert_eq!(
            by_name_with_precision("INT", 9),
            Some(LogicalType::Int {
                signed: true,
                bits: IntBits::B32,
                precision: Some(9),
            })
        );
        assert_eq!(
            by_name_with_precision("INT", 3),
            Some(LogicalType::Int {
                signed: true,
                bits: IntBits::B16,
                precision: Some(3),
            })
        );
        assert_eq!(by_name_with_precision("BOOL", 3), None);
    }
}
