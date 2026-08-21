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

/// A value as a case spells it.
///
/// This is not [`Value`] and cannot be, for one reason: a node value
/// in the engine is a table id and a row offset, and the id is a number
/// the file decided. A case is read by nine clients against a database
/// each of them built, so what a case can assert is the table's name,
/// and the name is not in the value. Everything else a case can write
/// is a `Value` already and rides here as one.
///
/// A comparison therefore happens in this type and not in the engine's:
/// what came back is turned into a case's spelling with [`from_engine`]
/// and the two are compared as equals. That is what the C runner does
/// as well, for the same reason and with the same shape, so the two
/// agree by construction rather than by review.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    /// Everything the engine spells and a case spells the same way.
    Plain(Value),
    /// A node, as the table it is a row of and the offset of that row.
    Node { table: String, offset: u64 },
    /// An edge, as its table and the two rows it runs between.
    Edge { table: String, src: u64, dst: u64 },
    /// Nodes and edges alternating, beginning and ending with a node,
    /// so there are always an odd number of them.
    Path(Vec<Cell>),
    /// A list, which is here rather than in `Plain` because a list may
    /// hold a node and a `Value::List` cannot hold one that is named.
    List(Vec<Cell>),
}

impl Cell {
    /// The engine value this spells, or nothing when it spells one the
    /// engine has no parameter and no column for.
    ///
    /// A parameter and a load column are values going in rather than
    /// coming back, and nothing puts a node in either: a node is a row
    /// that exists, so naming one in a parameter would be naming a row
    /// the case has not written yet.
    pub fn plain(&self) -> Option<Value> {
        match self {
            Cell::Plain(value) => Some(value.clone()),
            Cell::List(items) => items
                .iter()
                .map(Cell::plain)
                .collect::<Option<Vec<Value>>>()
                .map(Value::List),
            Cell::Node { .. } | Cell::Edge { .. } | Cell::Path(_) => None,
        }
    }
}

/// What turning an engine value into a case's spelling needs from the
/// database that value came out of, which is one thing: the name of a
/// table id.
///
/// A trait rather than the catalog itself so that this file keeps
/// knowing about the encoding and nothing about storage, and so that a
/// test here can answer for a table without opening a database.
pub trait Tables {
    fn name(&self, table: u32) -> Option<&str>;
}

/// What came back, in the spelling a case is written in.
///
/// A table with no name is spelled `#7` after its id, which is what a
/// node column of an Arrow export is named when there is no catalog to
/// ask. It cannot happen here, since the catalog is right there, and it
/// is a spelling rather than a panic because a corpus runner reporting
/// "the case wants person#1 and this is #7" is more use to whoever has
/// to fix it than one that died.
pub fn from_engine(value: &Value, tables: &dyn Tables) -> Cell {
    let named = |table: u32| match tables.name(table) {
        Some(name) => name.to_string(),
        None => format!("#{table}"),
    };
    match value {
        Value::Node { table, offset } => Cell::Node {
            table: named(*table),
            offset: *offset,
        },
        Value::Rel {
            table, src, dst, ..
        } => Cell::Edge {
            table: named(*table),
            src: *src,
            dst: *dst,
        },
        Value::Path(items) => Cell::Path(items.iter().map(|v| from_engine(v, tables)).collect()),
        Value::List(items) => Cell::List(items.iter().map(|v| from_engine(v, tables)).collect()),
        other => Cell::Plain(other.clone()),
    }
}

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
const TYPES: [(&str, Form); 24] = [
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
    // A byte string is written in quotes because its hexits are digits
    // as often as not: a bare 0041 is a number with a leading zero in
    // one reader and the string it looks like in another, and neither
    // of them is the two octets the case meant.
    ("BYTES", Form::Text),
    ("DATE", Form::Text),
    ("LOCALTIME", Form::Text),
    ("ZONEDTIME", Form::Text),
    ("LOCALDATETIME", Form::Text),
    ("ZONEDDATETIME", Form::Text),
    ("DURATION", Form::Text),
    ("LIST", Form::Exact),
    // A node and an edge are written in quotes because what a case
    // spells is a name and two numbers with punctuation between them,
    // which is text in every reader and a number in none.
    ("NODE", Form::Text),
    ("EDGE", Form::Text),
    // A path is a sequence, like a list, because that is what it is:
    // the nodes and edges of a walk, in the order they were walked.
    ("PATH", Form::Exact),
];

/// The types the encoding reserves a name for and the engine has no
/// runtime value for yet, kept apart from an outright typo so that the
/// error says which of the two it is.
const RESERVED: [&str; 1] = ["DECIMAL"];

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
pub fn decode(node: &Node) -> Result<Cell, String> {
    if node.map().is_none() {
        return Err(format!(
            "line {}: a value is a mapping of `type` and `value`, and this is {}",
            node.line(),
            node.kind()
        ));
    }
    if let Some(key) = node.unknown(&["type", "value"]).first() {
        return Err(format!("line {}: a value has no key {key:?}", node.line()));
    }
    typed(node)
}

/// The `type` and `value` of a mapping that carries more than those
/// two, which is a parameter: it is a value with a name, and the name
/// belongs to the case rather than to the encoding. The keys are the
/// caller's to check, since only the caller knows which others it
/// allows.
pub fn typed(node: &Node) -> Result<Cell, String> {
    let line = node.line();
    let at = |msg: String| format!("line {line}: {msg}");
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
            None => Ok(Cell::Plain(Value::Null)),
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
pub fn payload(ty: &str, value: &Node) -> Result<Cell, String> {
    let Some(form) = form(ty) else {
        return Err(format!("line {}: {}", value.line(), unknown(ty)));
    };

    if ty == "LIST" || ty == "PATH" {
        // The empty list is a value worth a case and needs a spelling,
        // which is a `value:` with nothing under it.
        let items = value.seq_or_empty().ok_or_else(|| {
            format!(
                "line {}: a {ty} holds a sequence of values, and this is {}",
                value.line(),
                value.kind()
            )
        })?;
        let items: Vec<Cell> = items.iter().map(decode).collect::<Result<_, _>>()?;
        return match ty {
            "LIST" => Ok(Cell::List(items)),
            _ => walk(items, value.line()),
        };
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
        // A node and an edge are quoted for a different reason from the
        // numbers, so they are told a different reason. Both reasons
        // are the same rule: a payload is quoted where a bare one would
        // read as something else in some reader of this file.
        (Form::Text, false) if ty == "NODE" || ty == "EDGE" => {
            return Err(format!(
                "line {line}: {ty} is written in quotes, because {text} is a name and two numbers \
                 and no reader has a scalar for that"
            ));
        }
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
    match ty {
        "NODE" => node_at(text),
        "EDGE" => edge_at(text),
        _ => scalar(ty, text).map(Cell::Plain),
    }
    .ok_or_else(|| format!("line {line}: {text:?} is not a {ty}"))
}

/// The nodes and edges of a walk, or what is wrong with the sequence
/// somebody wrote.
///
/// A path alternates and ends at both ends with a node, so a sequence
/// that does not is a case that could never pass. Refusing it here
/// rather than at the comparison is the difference between a message
/// naming the line and a report saying the row differs.
fn walk(items: Vec<Cell>, line: usize) -> Result<Cell, String> {
    if items.len().is_multiple_of(2) {
        return Err(format!(
            "line {line}: a PATH is a node, then an edge and a node for each hop, so it holds an \
             odd number of values and this holds {}",
            items.len()
        ));
    }
    for (i, item) in items.iter().enumerate() {
        let want_node = i % 2 == 0;
        let ok = match item {
            Cell::Node { .. } => want_node,
            Cell::Edge { .. } => !want_node,
            _ => false,
        };
        if !ok {
            return Err(format!(
                "line {line}: a PATH alternates, so value {} is {} where it should be {}",
                i + 1,
                match item {
                    Cell::Node { .. } => "a NODE",
                    Cell::Edge { .. } => "an EDGE",
                    _ => "neither a NODE nor an EDGE",
                },
                match want_node {
                    true => "a NODE",
                    false => "an EDGE",
                }
            ));
        }
    }
    Ok(Cell::Path(items))
}

/// A node, written as its table and the offset of its row: `person#1`.
///
/// The table's name rather than its id, because the id is a number the
/// file decided and every client builds its own file. Split from the
/// right, so that a table whose name holds a `#` is still readable.
fn node_at(text: &str) -> Option<Cell> {
    let (table, offset) = text.rsplit_once('#')?;
    match table.is_empty() {
        true => None,
        false => Some(Cell::Node {
            table: table.to_string(),
            offset: offset.parse().ok()?,
        }),
    }
}

/// An edge, written as its table and the rows it runs between:
/// `knows#0->1`.
///
/// What is not written is which edge of that pair it is. A pair may run
/// more than once and the engine tells the copies apart by the row
/// their properties sit in, which is their place in the load order, and
/// that is a number the loader chose rather than one the case did. A
/// case that has to tell two parallel edges apart asserts a property of
/// them instead.
fn edge_at(text: &str) -> Option<Cell> {
    let (table, ends) = text.rsplit_once('#')?;
    let (src, dst) = ends.split_once("->")?;
    match table.is_empty() {
        true => None,
        false => Some(Cell::Edge {
            table: table.to_string(),
            src: src.parse().ok()?,
            dst: dst.parse().ok()?,
        }),
    }
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
        // The hexits and nothing else: the X and the quotes belong to
        // the statement that writes a literal, and what a case spells
        // here is the value, which is the octets.
        "BYTES" => zu_common::bytes::from_hexits(text).map(Value::Bytes),
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
pub fn show(cell: &Cell) -> String {
    match cell {
        Cell::Plain(value) => plain(value),
        Cell::Node { table, offset } => format!("NODE \"{table}#{offset}\""),
        Cell::Edge { table, src, dst } => format!("EDGE \"{table}#{src}->{dst}\""),
        Cell::Path(items) => {
            let items: Vec<String> = items.iter().map(show).collect();
            format!("PATH [{}]", items.join(", "))
        }
        Cell::List(items) => {
            let items: Vec<String> = items.iter().map(show).collect();
            format!("LIST [{}]", items.join(", "))
        }
    }
}

/// The same for the values that ride in a [`Cell::Plain`], which is
/// every one the engine and a case spell alike.
fn plain(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => format!("BOOL {b}"),
        Value::Int(n) => format!("INT64 \"{n}\""),
        Value::Float(f) => format!("FLOAT64 \"{}\"", show_float(*f)),
        Value::Str(s) => format!("STRING {s:?}"),
        Value::Bytes(b) => format!("BYTES \"{}\"", zu_common::bytes::hexits(b)),
        Value::Temporal(t) => format!("{} \"{t}\"", type_name(&t.logical_type())),
        Value::List(items) => {
            let items: Vec<String> = items.iter().map(plain).collect();
            format!("LIST [{}]", items.join(", "))
        }
        Value::Record(fields) => {
            let fields: Vec<String> = fields
                .iter()
                .map(|(name, v)| format!("{name}: {}", plain(v)))
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
/// and not another. Comparing the bits answers the second.
///
/// It does not answer the first, because a NaN is a family of bit
/// patterns rather than one. `inf - inf` is a NaN with the sign bit set
/// on x86-64 and clear on aarch64, and neither of those is the constant
/// a case that writes `NaN` decodes to. The sign and the payload of a
/// NaN are the hardware's business, so every NaN is the same NaN here.
pub fn same(a: &Cell, b: &Cell) -> bool {
    match (a, b) {
        (Cell::Plain(x), Cell::Plain(y)) => alike(x, y),
        (Cell::List(x), Cell::List(y)) | (Cell::Path(x), Cell::Path(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        _ => a == b,
    }
}

/// The same for two engine values, which is where the float rule lives
/// because the engine's value is where a float is.
fn alike(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) if x.is_nan() && y.is_nan() => true,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::List(x), Value::List(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| alike(a, b))
        }
        (Value::Record(x), Value::Record(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((xn, xv), (yn, yv))| xn == yn && alike(xv, yv))
        }
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml;

    fn read(text: &str) -> Result<Cell, String> {
        decode(&yaml::parse(text).expect("the fixture parses"))
    }

    fn cell(text: &str) -> Cell {
        read(text).unwrap_or_else(|e| panic!("{text:?} should decode: {e}"))
    }

    /// The engine value a case spells, for the assertions about the
    /// types the engine and a case spell alike, which is most of them.
    fn ok(text: &str) -> Value {
        let cell = cell(text);
        cell.plain().unwrap_or_else(|| {
            panic!(
                "{text:?} decodes to {}, which is not a plain value",
                show(&cell)
            )
        })
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
        let minus = cell("type: FLOAT64\nvalue: \"-0.0\"\n");
        assert!(!same(&minus, &Cell::Plain(Value::Float(0.0))));
        assert!(same(&minus, &Cell::Plain(Value::Float(-0.0))));
    }

    #[test]
    fn a_nan_matches_a_nan_whatever_the_hardware_put_in_its_sign_and_payload() {
        let want = cell("type: FLOAT64\nvalue: \"NaN\"\n");
        let negative = Value::Float(f64::from_bits(f64::NAN.to_bits() | 1 << 63));
        let payload = Value::Float(f64::from_bits(f64::NAN.to_bits() | 7));
        assert!(same(&want, &Cell::Plain(Value::Float(f64::NAN))));
        assert!(same(&want, &Cell::Plain(negative)));
        assert!(same(&want, &Cell::Plain(payload)));
        assert!(!same(&want, &Cell::Plain(Value::Float(0.0))));
    }

    #[test]
    fn a_float32_is_narrowed_so_a_case_asserts_what_the_narrower_type_can_hold() {
        assert_eq!(
            ok("type: FLOAT32\nvalue: \"0.1\"\n"),
            Value::Float(0.1f32 as f64)
        );
        assert!(!same(
            &Cell::Plain(ok("type: FLOAT32\nvalue: \"0.1\"\n")),
            &Cell::Plain(Value::Float(0.1))
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

    /// A stub catalog, which is what the trait is for: the encoding
    /// can be tested without a database and the runner is the only
    /// place a real catalog reaches.
    struct Tiny;

    impl Tables for Tiny {
        fn name(&self, table: u32) -> Option<&str> {
            match table {
                0 => Some("person"),
                1 => Some("knows"),
                _ => None,
            }
        }
    }

    #[test]
    fn a_node_and_an_edge_are_written_as_a_table_and_the_rows_they_are() {
        assert_eq!(
            cell("type: NODE\nvalue: \"person#1\"\n"),
            Cell::Node {
                table: "person".into(),
                offset: 1
            }
        );
        assert_eq!(
            cell("type: EDGE\nvalue: \"knows#0->1\"\n"),
            Cell::Edge {
                table: "knows".into(),
                src: 0,
                dst: 1
            }
        );
        // The table's name is what a case can assert, so a case that
        // wrote the id instead is a case about this file rather than
        // about the engine, and there is nothing here to catch it. What
        // is caught is the spelling: no table, no offset, or an offset
        // that is not a number.
        for text in [
            "type: NODE\nvalue: \"person\"\n",
            "type: NODE\nvalue: \"#1\"\n",
            "type: NODE\nvalue: \"person#-1\"\n",
            "type: EDGE\nvalue: \"knows#0\"\n",
            "type: EDGE\nvalue: \"knows#0->\"\n",
        ] {
            let err = read(text).expect_err(&format!("{text:?} is refused"));
            assert!(err.contains("is not a"), "{text:?} gave {err:?}");
        }
        // Bare rather than quoted, which is the rule the whole encoding
        // exists for, told with the reason that applies to these two.
        let err = read("type: NODE\nvalue: person#1\n").expect_err("refused");
        assert!(err.contains("no reader has a scalar for that"), "{err}");
    }

    #[test]
    fn a_path_is_a_walk_and_a_sequence_that_is_not_one_is_refused() {
        let hop = "type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n  - type: EDGE\n    value: \"knows#0->1\"\n  - type: NODE\n    value: \"person#1\"\n";
        let Cell::Path(items) = cell(hop) else {
            panic!("a path");
        };
        assert_eq!(items.len(), 3);
        // A path of one node is the shortest there is, and it is a
        // path: a walk starts somewhere.
        assert_eq!(
            cell("type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n"),
            Cell::Path(vec![Cell::Node {
                table: "person".into(),
                offset: 0
            }])
        );
        let even = "type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n  - type: EDGE\n    value: \"knows#0->1\"\n";
        let err = read(even).expect_err("refused");
        assert!(err.contains("odd number of values"), "{err}");
        let wrong = "type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n  - type: NODE\n    value: \"person#1\"\n  - type: NODE\n    value: \"person#2\"\n";
        let err = read(wrong).expect_err("refused");
        assert!(err.contains("value 2 is a NODE"), "{err}");
    }

    #[test]
    fn what_came_back_is_turned_into_the_spelling_a_case_is_written_in() {
        let path = Value::Path(vec![
            Value::Node {
                table: 0,
                offset: 0,
            },
            Value::Rel {
                table: 1,
                src: 0,
                dst: 1,
                ord: 3,
            },
            Value::Node {
                table: 0,
                offset: 1,
            },
        ]);
        let hop = "type: PATH\nvalue:\n  - type: NODE\n    value: \"person#0\"\n  - type: EDGE\n    value: \"knows#0->1\"\n  - type: NODE\n    value: \"person#1\"\n";
        // The ordinal is not spelled and so is not compared: a case
        // that had to tell two parallel edges apart would be asserting
        // the order they were loaded in.
        assert!(same(&cell(hop), &from_engine(&path, &Tiny)));
        assert_eq!(
            show(&from_engine(&path, &Tiny)),
            "PATH [NODE \"person#0\", EDGE \"knows#0->1\", NODE \"person#1\"]"
        );
        // A table the catalog does not name is spelled after its id
        // rather than reported as a name, which is a report somebody
        // can act on and not a comparison that quietly passed.
        assert_eq!(
            show(&from_engine(
                &Value::Node {
                    table: 7,
                    offset: 2
                },
                &Tiny
            )),
            "NODE \"#7#2\""
        );
    }

    #[test]
    fn what_a_report_prints_is_what_a_case_would_be_written_as() {
        assert_eq!(show(&Cell::Plain(Value::Int(7))), "INT64 \"7\"");
        assert_eq!(show(&Cell::Plain(Value::Float(1.0))), "FLOAT64 \"1.0\"");
        assert_eq!(
            show(&Cell::Plain(Value::Float(f64::NAN))),
            "FLOAT64 \"NaN\""
        );
        assert_eq!(show(&Cell::Plain(Value::Null)), "NULL");
        assert_eq!(
            show(&Cell::List(vec![
                Cell::Plain(Value::Bool(true)),
                Cell::Plain(Value::Str("a".into()))
            ])),
            "LIST [BOOL true, STRING \"a\"]"
        );
        assert_eq!(
            show(&Cell::Node {
                table: "person".into(),
                offset: 1
            }),
            "NODE \"person#1\""
        );
        assert_eq!(
            show(&Cell::Edge {
                table: "knows".into(),
                src: 0,
                dst: 1
            }),
            "EDGE \"knows#0->1\""
        );
    }
}
