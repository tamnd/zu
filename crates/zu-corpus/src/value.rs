//! The `{type, value}` encoding a case writes its values in.
//!
//! Every value in the corpus is a mapping with a `type` naming the GQL
//! type and a `value` holding the payload. The type is written down
//! rather than inferred because the corpus is read by nine languages
//! and inference is where they differ: a bare `1` is an integer in
//! YAML, and which integer it becomes is a decision each host language
//! makes on its own.
//!
//! The payload is a YAML scalar where a YAML scalar is exact, and a
//! string where it is not. That line falls in a specific place and
//! every case on the far side of it is a bug somebody would otherwise
//! ship:
//!
//! - An integer wider than 53 bits, because most YAML readers hand a
//!   number to a double and `9223372036854775807` comes back as
//!   `9223372036854775808`. So INT64 and UINT64 are strings.
//! - A decimal, because it is exact and a double is not.
//! - A float, for the same reason and for `NaN`, `Infinity` and `-0.0`,
//!   which YAML either spells differently in each reader or does not
//!   spell at all.
//! - A temporal value, which has no YAML type that keeps an offset, and
//!   whose readers otherwise reach for the host language's date.
//!
//! Refusing the wrong form is half the point. An INT64 written as a
//! bare number is not read leniently and rounded, it is refused, and
//! the case is fixed. The reader keeps whether a scalar was quoted so
//! that this can be checked at all.

use zu::query::Value;
use zu_common::{DurationKind, LogicalType, Temporal};

use crate::yaml::Node;

/// Whether a type's payload is written as a quoted string, and why the
/// answer is not "whatever the writer felt like".
///
/// `Exact` is a type a YAML scalar carries without loss. `Text` is one
/// it does not, listed above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    Exact,
    Text,
}

/// The types a case may name.
///
/// This is the GQL type of the value, not the Rust one. INT8 and INT64
/// are both `Value::Int` here, and they are still two entries, because
/// the corpus is a contract for languages where they are not: a
/// TypeScript client returns `number` for one and `bigint` for the
/// other, and a case that did not say which meant nothing to it.
const TYPES: [(&str, Form); 20] = [
    ("NULL", Form::Exact),
    ("BOOL", Form::Exact),
    ("INT8", Form::Exact),
    ("INT16", Form::Exact),
    ("INT32", Form::Exact),
    ("INT64", Form::Text),
    ("UINT8", Form::Exact),
    ("UINT16", Form::Exact),
    ("UINT32", Form::Exact),
    ("UINT64", Form::Text),
    ("FLOAT32", Form::Text),
    ("FLOAT64", Form::Text),
    ("STRING", Form::Exact),
    ("DATE", Form::Text),
    ("LOCALTIME", Form::Text),
    ("ZONEDTIME", Form::Text),
    ("LOCALDATETIME", Form::Text),
    ("ZONEDDATETIME", Form::Text),
    ("DURATION", Form::Text),
    ("LIST", Form::Exact),
];

/// The types the encoding reserves a name for and the engine has no
/// runtime value for yet, kept apart from an outright typo so that the
/// error says which of the two it is.
const RESERVED: [&str; 5] = ["DECIMAL", "BYTES", "NODE", "EDGE", "PATH"];

pub fn form(ty: &str) -> Option<Form> {
    TYPES.iter().find(|(name, _)| *name == ty).map(|(_, f)| *f)
}

/// What to say about a name that is not a type, which is one of two
/// things and worth telling apart.
fn unknown(ty: &str) -> String {
    match RESERVED.contains(&ty) {
        true => format!("{ty} is a type the encoding reserves and the engine has no value for"),
        false => format!("{ty} is not a type this encoding knows"),
    }
}

/// The value a `{type, value}` mapping describes, or what is wrong
/// with it.
pub fn decode(node: &Node) -> Result<Value, String> {
    let line = node.line();
    let at = |msg: String| format!("line {line}: {msg}");
    if node.map().is_none() {
        return Err(at(format!(
            "a value is a mapping of `type` and `value`, and this is {}",
            node.kind()
        )));
    }
    if let Some(key) = node.unknown(&["type", "value"]).first() {
        return Err(at(format!("a value has no key {key:?}")));
    }
    let ty = node
        .get("type")
        .ok_or_else(|| at("a value with no `type`".to_string()))?
        .str()
        .ok_or_else(|| at("a `type` that is not a name".to_string()))?;

    // Checked here as well as in `payload`, because a value whose type
    // is not a type and which also has no `value` under it should be
    // told about the type first: that is the mistake, and the missing
    // payload is a consequence of it.
    if form(ty).is_none() {
        return Err(at(unknown(ty)));
    }

    if ty == "NULL" {
        return match node.get("value") {
            None => Ok(Value::Null),
            Some(_) => Err(at("NULL carries no `value`".to_string())),
        };
    }
    let value = node
        .get("value")
        .ok_or_else(|| at(format!("a {ty} with no `value`")))?;
    payload(ty, value)
}

/// The value a payload spells under a type that has already been read.
///
/// A row of a case names its type beside every value. A column of a
/// load names it once at the top and every value under it is a bare
/// payload, which is the same encoding with the type factored out, so
/// it is the same function reading it.
pub fn payload(ty: &str, value: &Node) -> Result<Value, String> {
    let Some(form) = form(ty) else {
        return Err(format!("line {}: {}", value.line(), unknown(ty)));
    };

    if ty == "LIST" {
        // The empty list is a value worth a case and needs a spelling,
        // which is a `value:` with nothing under it.
        let items = value.seq_or_empty().ok_or_else(|| {
            format!(
                "line {}: a LIST holds a sequence of values, and this is {}",
                value.line(),
                value.kind()
            )
        })?;
        return items
            .iter()
            .map(decode)
            .collect::<Result<_, _>>()
            .map(Value::List);
    }

    let (text, quoted) = value.scalar().ok_or_else(|| {
        format!(
            "line {}: a {ty} holds one scalar, and this is {}",
            value.line(),
            value.kind()
        )
    })?;
    // The one rule the whole encoding exists for, checked before the
    // text is looked at, because a value that parses is exactly the
    // case where a silent misread would survive review.
    let line = value.line();
    match (form, quoted) {
        (Form::Text, false) => {
            return Err(format!(
                "line {line}: {ty} is written in quotes, because a bare {text} is a number and \
                 some reader of this file will round it"
            ));
        }
        (Form::Exact, true) if ty != "STRING" => {
            return Err(format!(
                "line {line}: {ty} is written without quotes, so that a reader cannot take it for \
                 a string"
            ));
        }
        _ => {}
    }
    scalar(ty, text).ok_or_else(|| format!("line {line}: {text:?} is not a {ty}"))
}

/// The value a scalar payload spells, or `None` if it does not spell
/// one of that type.
fn scalar(ty: &str, text: &str) -> Option<Value> {
    let temporal = |lt: LogicalType| Temporal::parse(&lt, text).map(Value::Temporal);
    match ty {
        "BOOL" => match text {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        "STRING" => Some(Value::Str(text.to_string())),
        "INT8" => text.parse::<i8>().ok().map(|n| Value::Int(n.into())),
        "INT16" => text.parse::<i16>().ok().map(|n| Value::Int(n.into())),
        "INT32" => text.parse::<i32>().ok().map(|n| Value::Int(n.into())),
        "INT64" => text.parse::<i64>().ok().map(Value::Int),
        "UINT8" => text.parse::<u8>().ok().map(|n| Value::Int(n.into())),
        "UINT16" => text.parse::<u16>().ok().map(|n| Value::Int(n.into())),
        "UINT32" => text.parse::<u32>().ok().map(|n| Value::Int(n.into())),
        // The engine's integer is signed and 64 bits wide, so the top
        // half of UINT64 has nowhere to go. Refusing it here is better
        // than wrapping it into a negative, which is a case that would
        // pass while meaning the opposite of what it says.
        "UINT64" => text
            .parse::<u64>()
            .ok()
            .and_then(|n| i64::try_from(n).ok())
            .map(Value::Int),
        "FLOAT32" => float(text).map(|f| Value::Float(f as f32 as f64)),
        "FLOAT64" => float(text).map(Value::Float),
        "DATE" => temporal(LogicalType::Date),
        "LOCALTIME" => temporal(LogicalType::LocalTime),
        "ZONEDTIME" => temporal(LogicalType::ZonedTime),
        "LOCALDATETIME" => temporal(LogicalType::LocalDatetime),
        "ZONEDDATETIME" => temporal(LogicalType::ZonedDatetime),
        "DURATION" => temporal(LogicalType::Duration(DurationKind::DayTime))
            .or_else(|| temporal(LogicalType::Duration(DurationKind::YearMonth))),
        _ => None,
    }
}

/// A float, including the three spellings YAML has no opinion about.
///
/// They are spelled the way the standard library prints them, so that
/// what a case writes and what a failure report prints are the same
/// text and a reader can compare them by eye.
fn float(text: &str) -> Option<f64> {
    match text {
        "NaN" => Some(f64::NAN),
        "inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        // A float is exact here, so `1` is not a FLOAT64 and neither is
        // `1e400`. The first is an integer somebody meant to write as
        // `1.0` and the second is `inf` under another name.
        _ if text.contains(['.', 'e', 'E']) => text.parse().ok().filter(|f: &f64| f.is_finite()),
        _ => None,
    }
}

/// How a value reads in a failure report, in the encoding's own
/// spelling so that it can be pasted into a case.
pub fn show(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => format!("BOOL {b}"),
        Value::Int(n) => format!("INT64 \"{n}\""),
        Value::Float(f) => format!("FLOAT64 \"{}\"", show_float(*f)),
        Value::Str(s) => format!("STRING {s:?}"),
        Value::Temporal(t) => format!("{} \"{t}\"", type_name(&t.logical_type())),
        Value::List(items) => {
            let items: Vec<String> = items.iter().map(show).collect();
            format!("LIST [{}]", items.join(", "))
        }
        Value::Record(fields) => {
            let fields: Vec<String> = fields
                .iter()
                .map(|(name, v)| format!("{name}: {}", show(v)))
                .collect();
            format!("RECORD {{{}}}", fields.join(", "))
        }
        other => format!("{other:?}"),
    }
}

fn show_float(f: f64) -> String {
    match f {
        _ if f.is_nan() => "NaN".to_string(),
        f64::INFINITY => "inf".to_string(),
        f64::NEG_INFINITY => "-inf".to_string(),
        // `{:?}` is the shortest text that reads back as the same
        // double, which is the property a corpus needs and `{}` does
        // not have: `{}` prints 1 for 1.0.
        _ => format!("{f:?}"),
    }
}

/// The encoding's name for a logical type, for a report that has to
/// name the type it found.
fn type_name(ty: &LogicalType) -> &'static str {
    match ty {
        LogicalType::Date => "DATE",
        LogicalType::LocalTime => "LOCALTIME",
        LogicalType::ZonedTime => "ZONEDTIME",
        LogicalType::LocalDatetime => "LOCALDATETIME",
        LogicalType::ZonedDatetime => "ZONEDDATETIME",
        LogicalType::Duration(_) => "DURATION",
        _ => "?",
    }
}

/// Whether two values are the same value.
///
/// Not `PartialEq`, for one reason: a float. `NaN` is not equal to
/// itself and a case asserting `NaN` has to pass, and `0.0` is equal to
/// `-0.0` and a case asserting `-0.0` has to fail on `0.0`, because the
/// sign of zero is exactly the sort of thing that survives one binding
/// and not another. Comparing the bits answers both.
pub fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Record(x), Value::Record(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((xn, xv), (yn, yv))| xn == yn && same(xv, yv))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml;

    fn read(text: &str) -> Result<Value, String> {
        decode(&yaml::parse(text).expect("the fixture parses"))
    }

    fn ok(text: &str) -> Value {
        read(text).unwrap_or_else(|e| panic!("{text:?} should decode: {e}"))
    }

    #[test]
    fn the_widths_a_yaml_number_carries_are_written_as_numbers() {
        assert_eq!(ok("type: INT8\nvalue: -128\n"), Value::Int(-128));
        assert_eq!(
            ok("type: INT32\nvalue: 2147483647\n"),
            Value::Int(2147483647)
        );
        assert_eq!(ok("type: BOOL\nvalue: true\n"), Value::Bool(true));
        assert_eq!(ok("type: NULL\n"), Value::Null);
    }

    #[test]
    fn the_widths_it_does_not_carry_are_written_as_strings() {
        assert_eq!(
            ok("type: INT64\nvalue: \"9223372036854775807\"\n"),
            Value::Int(i64::MAX)
        );
        assert_eq!(
            ok("type: INT64\nvalue: \"-9223372036854775808\"\n"),
            Value::Int(i64::MIN)
        );
    }

    #[test]
    fn an_int64_written_bare_is_the_defect_the_encoding_exists_to_stop() {
        let err = read("type: INT64\nvalue: 9223372036854775807\n").expect_err("refused");
        assert!(err.contains("written in quotes"), "{err}");
        assert!(err.contains("will round it"), "{err}");
    }

    #[test]
    fn a_number_written_in_quotes_is_refused_the_other_way_round() {
        let err = read("type: INT8\nvalue: \"42\"\n").expect_err("refused");
        assert!(err.contains("without quotes"), "{err}");
        // A string is the one type whose payload is quoted or not as
        // YAML pleases, because either way it is the same text.
        assert_eq!(ok("type: STRING\nvalue: \"42\"\n"), Value::Str("42".into()));
        assert_eq!(ok("type: STRING\nvalue: 42\n"), Value::Str("42".into()));
    }

    #[test]
    fn a_value_too_wide_for_the_type_it_claims_is_not_quietly_widened() {
        for text in [
            "type: INT8\nvalue: 128\n",
            "type: UINT8\nvalue: -1\n",
            "type: INT32\nvalue: 2147483648\n",
            "type: UINT64\nvalue: \"18446744073709551615\"\n",
        ] {
            let err = read(text).expect_err(&format!("{text:?} is refused"));
            assert!(err.contains("is not a"), "{text:?} gave {err:?}");
        }
    }

    #[test]
    fn a_float_says_which_float_because_the_three_awkward_ones_have_no_yaml_spelling() {
        assert_eq!(ok("type: FLOAT64\nvalue: \"1.5\"\n"), Value::Float(1.5));
        assert!(matches!(ok("type: FLOAT64\nvalue: \"NaN\"\n"), Value::Float(f) if f.is_nan()));
        assert_eq!(
            ok("type: FLOAT64\nvalue: \"inf\"\n"),
            Value::Float(f64::INFINITY)
        );
        assert_eq!(
            ok("type: FLOAT64\nvalue: \"-inf\"\n"),
            Value::Float(f64::NEG_INFINITY)
        );
        // A negative zero is a different value from a zero, and saying
        // so is why `same` compares bits.
        let minus = ok("type: FLOAT64\nvalue: \"-0.0\"\n");
        assert!(!same(&minus, &Value::Float(0.0)));
        assert!(same(&minus, &Value::Float(-0.0)));
    }

    #[test]
    fn a_float32_is_narrowed_so_a_case_asserts_what_the_narrower_type_can_hold() {
        assert_eq!(
            ok("type: FLOAT32\nvalue: \"0.1\"\n"),
            Value::Float(0.1f32 as f64)
        );
        assert!(!same(
            &ok("type: FLOAT32\nvalue: \"0.1\"\n"),
            &Value::Float(0.1)
        ));
    }

    #[test]
    fn an_integer_written_as_a_float_is_refused_rather_than_promoted() {
        let err = read("type: FLOAT64\nvalue: \"1\"\n").expect_err("refused");
        assert!(err.contains("is not a FLOAT64"), "{err}");
    }

    #[test]
    fn a_temporal_is_written_the_way_the_engine_prints_it() {
        for (text, want) in [
            ("type: DATE\nvalue: \"2024-02-29\"\n", "2024-02-29"),
            ("type: LOCALTIME\nvalue: \"12:34:56\"\n", "12:34:56"),
            (
                "type: ZONEDDATETIME\nvalue: \"2024-01-01T00:00:00+07:00\"\n",
                "2024-01-01T00:00:00+07:00",
            ),
            ("type: DURATION\nvalue: \"P1Y2M\"\n", "P1Y2M"),
        ] {
            let Value::Temporal(t) = ok(text) else {
                panic!("{text:?} should decode to a temporal");
            };
            assert_eq!(t.to_string(), want, "{text:?}");
        }
    }

    #[test]
    fn a_list_holds_encoded_values_and_not_bare_ones() {
        let list = ok(
            "type: LIST\nvalue:\n  - type: INT8\n    value: 1\n  - type: STRING\n    value: two\n",
        );
        assert_eq!(
            list,
            Value::List(vec![Value::Int(1), Value::Str("two".into())])
        );
        let err = read("type: LIST\nvalue:\n  - 1\n").expect_err("refused");
        assert!(err.contains("a value is a mapping"), "{err}");
        // The empty list is a value, and a `value:` with nothing under
        // it is how it is written.
        assert_eq!(ok("type: LIST\nvalue:\n"), Value::List(Vec::new()));
    }

    #[test]
    fn a_type_the_engine_cannot_hold_yet_says_so_rather_than_looking_like_a_typo() {
        let err = read("type: DECIMAL\nvalue: \"1.00\"\n").expect_err("refused");
        assert!(err.contains("reserves"), "{err}");
        let err = read("type: INT65\nvalue: \"1\"\n").expect_err("refused");
        assert!(err.contains("not a type this encoding knows"), "{err}");
    }

    #[test]
    fn a_mapping_that_is_not_a_value_is_refused_with_its_line() {
        for (text, want) in [
            ("type: INT8\n", "with no `value`"),
            ("value: 1\n", "with no `type`"),
            ("type: NULL\nvalue: 1\n", "carries no `value`"),
            ("type: INT8\nvalue: 1\nnote: hi\n", "no key \"note\""),
            ("type: INT8\nvalue:\n  - 1\n", "holds one scalar"),
        ] {
            let err = read(text).expect_err(&format!("{text:?} is refused"));
            assert!(err.contains(want), "{text:?} gave {err:?}");
        }
    }

    #[test]
    fn what_a_report_prints_is_what_a_case_would_be_written_as() {
        assert_eq!(show(&Value::Int(7)), "INT64 \"7\"");
        assert_eq!(show(&Value::Float(1.0)), "FLOAT64 \"1.0\"");
        assert_eq!(show(&Value::Float(f64::NAN)), "FLOAT64 \"NaN\"");
        assert_eq!(show(&Value::Null), "NULL");
        assert_eq!(
            show(&Value::List(vec![
                Value::Bool(true),
                Value::Str("a".into())
            ])),
            "LIST [BOOL true, STRING \"a\"]"
        );
    }
}
