//! The data a suite loads before its cases run, and how it gets in.
//!
//! Everything else in the corpus is an expression. An expression says
//! what a value means on the way out of the engine and nothing about
//! how it got in, and half of what a client does is put values in. A
//! load is the other half: a table of typed columns, written in the
//! same encoding a row is, that every runner puts into a fresh
//! database through its own bulk load path before the cases run.
//!
//! It is bulk load rather than a statement because in v0 that is the
//! only way in. `CREATE` and `INSERT` need a table and no statement
//! makes one, so a case that stored a value with a statement could not
//! be written at all, while every client already has to expose a
//! loader: the Rust SDK has an appender, the CLI has `zu copy`, and
//! the C ABI grows the entry point that all the rest are built on.
//! What a runner does with a load is therefore the strongest form of
//! the corpus question, because the value crosses the boundary twice
//! and by two different mechanisms.
//!
//! A load belongs to the file rather than to the case. Every case
//! still gets a database of its own and the load is applied to each
//! one, so cases stay independent; writing it once is a property of
//! the file and not of the run.
//!
//! ```yaml
//! load:
//!   nodes: person
//!   edges: knows
//!   count: 3
//!   columns:
//!     - name: age
//!       type: INT64
//!       values:
//!         - "30"
//!         - "40"
//!         - "50"
//!   pairs:
//!     - from: 0
//!       to: 1
//! ```
//!
//! A column names its type once and its values are bare payloads under
//! it, which is the row encoding with the type factored out. The
//! quoting rule is the same one and for the same reason.

use zu::query::Value;
use zu::zu1::file::Zu1File;
use zu::zu1::graph::bulk_load_keyed;
use zu::zu1::props::{PropValues, store_props};
use zu_common::{DurationKind, Temporal};

use crate::value;
use crate::yaml::Node;

/// One node table, its columns, and the edges between its rows.
#[derive(Debug, Clone)]
pub struct Load {
    pub nodes: String,
    pub edges: String,
    /// How many rows the node table has. Written down rather than
    /// counted from the columns, so that a column with a value missing
    /// is an error in the file and not a shorter table.
    pub count: u64,
    pub columns: Vec<Column>,
    pub pairs: Vec<(u32, u32)>,
    pub line: usize,
}

/// One column: a name, the type every value in it has, and the values
/// in row order.
#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub ty: String,
    pub values: Vec<Value>,
    pub line: usize,
}

impl Load {
    /// A load, or what is wrong with the block that was meant to be
    /// one.
    pub fn parse(node: &Node) -> Result<Load, String> {
        let line = node.line();
        let at = |msg: String| format!("line {line}: {msg}");
        if node.map().is_none() {
            return Err(at(format!(
                "a load is a mapping, and this is {}",
                node.kind()
            )));
        }
        if let Some(key) = node
            .unknown(&["nodes", "edges", "count", "columns", "pairs"])
            .first()
        {
            return Err(at(format!("a load has no key {key:?}")));
        }
        let nodes = name(node, "nodes")?;
        let edges = name(node, "edges")?;
        let count: u64 = node
            .get("count")
            .and_then(Node::str)
            .ok_or_else(|| at("a load says how many rows it has, with `count:`".to_string()))?
            .parse()
            .map_err(|_| at("`count:` is a number of rows".to_string()))?;
        if count == 0 {
            return Err(at(
                "a load of no rows is a load nothing can be read back from".to_string(),
            ));
        }

        let mut columns = Vec::new();
        for col in node
            .get("columns")
            .ok_or_else(|| at("a load has `columns:`".to_string()))?
            .seq()
            .ok_or_else(|| at("`columns:` is a sequence".to_string()))?
        {
            columns.push(column(col, count)?);
        }
        if columns.is_empty() {
            return Err(at("a load with no columns holds no values".to_string()));
        }
        let mut sorted: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        sorted.sort_unstable();
        if let Some(dup) = sorted.windows(2).find(|w| w[0] == w[1]) {
            return Err(at(format!("two columns are called {:?}", dup[0])));
        }

        let mut pairs = Vec::new();
        for pair in node
            .get("pairs")
            .map(|p| {
                p.seq_or_empty()
                    .ok_or_else(|| at("`pairs:` is a sequence of edges".to_string()))
            })
            .transpose()?
            .unwrap_or_default()
        {
            pairs.push(edge(pair, count)?);
        }
        // The loader wants them sorted and unique, and sorting them
        // here rather than asking the file to is one less thing a case
        // can get wrong without the error saying so.
        pairs.sort_unstable();
        pairs.dedup();

        Ok(Load {
            nodes,
            edges,
            count,
            columns,
            pairs,
            line,
        })
    }

    /// Puts the load into a database that has nothing in it, through
    /// the same bulk load path `zu copy` and the appender use.
    pub fn apply(&self, db: &mut Zu1File) -> Result<(), String> {
        bulk_load_keyed(db, &self.nodes, &self.edges, self.count, &self.pairs, None)
            .map_err(|e| format!("loading {}: {e}", self.nodes))?;
        // The property store takes a column at a time and borrows it,
        // so every column is reduced to the vector its type is kept in
        // first and the borrows are handed over after. A string column
        // needs a second vector, of one borrow per row, because the
        // store wants a slice of slices and a vector of vectors is not
        // one.
        let mut held = Vec::with_capacity(self.columns.len());
        for col in &self.columns {
            held.push((col.name.as_str(), col.stored()?));
        }
        let rows: Vec<Vec<&[u8]>> = held
            .iter()
            .map(|(_, held)| match held {
                Held::Str(v) => v.iter().map(Vec::as_slice).collect(),
                _ => Vec::new(),
            })
            .collect();
        let props: Vec<(&str, PropValues<'_>)> = held
            .iter()
            .zip(&rows)
            .map(|((name, held), rows)| {
                (
                    *name,
                    match held {
                        Held::Str(_) => PropValues::Str(rows),
                        _ => held.values(),
                    },
                )
            })
            .collect();
        if props.is_empty() {
            return Ok(());
        }
        // The directory the store hands back describes where the
        // columns went, which the loader in `zu copy` prints and a case
        // has no use for: the reader finds them through the file.
        store_props(db, &self.nodes, &props)
            .map(|_| ())
            .map_err(|e| format!("storing columns: {e}"))
    }
}

/// A column's values in the shape the property store keeps them, held
/// so that they can be borrowed.
///
/// Every arm is a vector the store takes a slice of. The alternative
/// is to build the slice inside the loop that borrows it, which does
/// not live long enough, so the two steps are separate here.
#[derive(Debug)]
pub enum Held {
    Str(Vec<Vec<u8>>),
    Int(Vec<u64>),
    Bool(Vec<bool>),
    Float(Vec<f64>),
    Date(Vec<i32>),
    LocalTime(Vec<i64>),
    LocalDatetime(Vec<i64>),
    Duration(DurationKind, Vec<i64>),
}

impl Held {
    fn values(&self) -> PropValues<'_> {
        match self {
            // The store wants a slice of slices and a `Vec<Vec<u8>>`
            // is not one, so the row pointers are built here and
            // borrowed from the same expression that uses them.
            Held::Str(_) => unreachable!("a string column goes through the row borrows"),
            Held::Int(v) => PropValues::Int(v),
            Held::Bool(v) => PropValues::Bool(v),
            Held::Float(v) => PropValues::Float(v),
            Held::Date(v) => PropValues::Date(v),
            Held::LocalTime(v) => PropValues::LocalTime(v),
            Held::LocalDatetime(v) => PropValues::LocalDatetime(v),
            Held::Duration(kind, v) => PropValues::Duration(*kind, v),
        }
    }
}

impl Column {
    /// The column reduced to the vector its type is stored in, or why
    /// its type has no column to be stored in yet.
    fn stored(&self) -> Result<Held, String> {
        let at = |msg: String| format!("line {}: column {:?} {msg}", self.line, self.name);
        let mut ints = Vec::new();
        let mut floats = Vec::new();
        let mut bools = Vec::new();
        let mut strs = Vec::new();
        let mut temporals = Vec::new();
        for v in &self.values {
            match v {
                Value::Int(n) => ints.push(*n as u64),
                Value::Float(f) => floats.push(*f),
                Value::Bool(b) => bools.push(*b),
                Value::Str(s) => strs.push(s.clone().into_bytes()),
                Value::Temporal(t) => temporals.push(*t),
                other => {
                    return Err(at(format!(
                        "holds {}, and a column of those cannot be loaded yet",
                        value::show(&value::Cell::Plain(other.clone()))
                    )));
                }
            }
        }
        match (
            ints.len(),
            floats.len(),
            bools.len(),
            strs.len(),
            temporals.len(),
        ) {
            (n, 0, 0, 0, 0) if n > 0 => Ok(Held::Int(ints)),
            (0, n, 0, 0, 0) if n > 0 => Ok(Held::Float(floats)),
            (0, 0, n, 0, 0) if n > 0 => Ok(Held::Bool(bools)),
            (0, 0, 0, n, 0) if n > 0 => Ok(Held::Str(strs)),
            (0, 0, 0, 0, n) if n > 0 => temporal(&temporals).map_err(at),
            _ => Err(at(
                "holds more than one kind of value, and a column holds one".to_string(),
            )),
        }
    }
}

/// A temporal column reduced to its counts, or why it is not a column.
///
/// The two zoned types are not here because the store has nowhere to
/// keep an offset, which is a fact about v0 and the reason no fixture
/// has a zoned column in it.
fn temporal(values: &[Temporal]) -> Result<Held, String> {
    let kinds: Vec<&str> = values
        .iter()
        .map(|t| match t {
            Temporal::Date(_) => "DATE",
            Temporal::LocalTime(_) => "LOCALTIME",
            Temporal::ZonedTime { .. } => "ZONEDTIME",
            Temporal::LocalDatetime(_) => "LOCALDATETIME",
            Temporal::ZonedDatetime { .. } => "ZONEDDATETIME",
            Temporal::Duration(DurationKind::DayTime, _) => "DURATION day time",
            Temporal::Duration(DurationKind::YearMonth, _) => "DURATION year month",
        })
        .collect();
    if kinds.windows(2).any(|w| w[0] != w[1]) {
        return Err("mixes temporal kinds, and a column holds one".to_string());
    }
    match values[0] {
        Temporal::Date(_) => Ok(Held::Date(
            values
                .iter()
                .map(|t| match t {
                    Temporal::Date(d) => *d,
                    _ => unreachable!("the kinds were checked above"),
                })
                .collect(),
        )),
        Temporal::LocalTime(_) => Ok(Held::LocalTime(nanos(values))),
        Temporal::LocalDatetime(_) => Ok(Held::LocalDatetime(nanos(values))),
        Temporal::Duration(kind, _) => Ok(Held::Duration(kind, nanos(values))),
        Temporal::ZonedTime { .. } | Temporal::ZonedDatetime { .. } => {
            Err("is a zoned type, which a column has nowhere to keep the offset of".to_string())
        }
    }
}

fn nanos(values: &[Temporal]) -> Vec<i64> {
    values
        .iter()
        .map(|t| match t {
            Temporal::LocalTime(n) | Temporal::LocalDatetime(n) | Temporal::Duration(_, n) => *n,
            _ => unreachable!("the kinds were checked above"),
        })
        .collect()
}

fn name(node: &Node, key: &str) -> Result<String, String> {
    let text = node
        .get(key)
        .and_then(Node::str)
        .ok_or(format!("line {}: a load names its `{key}:`", node.line()))?;
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "line {}: {text:?} is a table name, which is letters, digits and underscores",
            node.line()
        ));
    }
    Ok(text.to_string())
}

fn column(node: &Node, count: u64) -> Result<Column, String> {
    let line = node.line();
    let at = |msg: String| format!("line {line}: {msg}");
    if let Some(key) = node.unknown(&["name", "type", "values"]).first() {
        return Err(at(format!("a column has no key {key:?}")));
    }
    let name = name(node, "name")?;
    let ty = node
        .get("type")
        .and_then(Node::str)
        .ok_or_else(|| at("a column names the type of its values".to_string()))?
        .to_string();
    let values: Vec<Value> = node
        .get("values")
        .ok_or_else(|| at("a column has `values:`".to_string()))?
        .seq()
        .ok_or_else(|| at("`values:` is a sequence of payloads".to_string()))?
        .iter()
        .map(|v| {
            let line = v.line();
            let cell = value::payload(&ty, v)?;
            // A column is values going into a database rather than
            // coming back out of one, and nothing puts a node into a
            // column: a node is a row, and the rows are what the load
            // is making.
            cell.plain().ok_or_else(|| {
                format!(
                    "line {line}: a column cannot hold a graph value, and this holds {}",
                    value::show(&cell)
                )
            })
        })
        .collect::<Result<_, _>>()?;
    if values.len() as u64 != count {
        return Err(at(format!(
            "column {name:?} has {} values against a count of {count}",
            values.len()
        )));
    }
    Ok(Column {
        name,
        ty,
        values,
        line,
    })
}

fn edge(node: &Node, count: u64) -> Result<(u32, u32), String> {
    let line = node.line();
    let at = |msg: String| format!("line {line}: {msg}");
    if let Some(key) = node.unknown(&["from", "to"]).first() {
        return Err(at(format!("an edge has no key {key:?}")));
    }
    let end = |key: &str| -> Result<u32, String> {
        let n: u64 = node
            .get(key)
            .and_then(Node::str)
            .ok_or_else(|| at(format!("an edge has a `{key}:`")))?
            .parse()
            .map_err(|_| at(format!("`{key}:` is a row number")))?;
        match n < count {
            true => Ok(n as u32),
            false => Err(at(format!(
                "an edge reaches row {n} of a table with {count} rows in it"
            ))),
        }
    };
    Ok((end("from")?, end("to")?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml;

    fn parse(text: &str) -> Result<Load, String> {
        Load::parse(
            yaml::parse(text)
                .expect("the fixture parses")
                .get("load")
                .expect("a load"),
        )
    }

    const HEAD: &str = "load:\n  nodes: person\n  edges: knows\n  count: 2\n";

    #[test]
    fn a_load_is_a_table_of_typed_columns_and_the_edges_between_its_rows() {
        let load = parse(&format!(
            "{HEAD}  columns:\n    - name: age\n      type: INT64\n      values:\n        - \"30\"\n        - \"40\"\n  pairs:\n    - from: 0\n      to: 1\n"
        ))
        .expect("parses");
        assert_eq!(load.nodes, "person");
        assert_eq!(load.edges, "knows");
        assert_eq!(load.count, 2);
        assert_eq!(load.columns.len(), 1);
        assert_eq!(load.columns[0].values, vec![Value::Int(30), Value::Int(40)]);
        assert_eq!(load.pairs, vec![(0, 1)]);
    }

    #[test]
    fn a_column_takes_the_same_quoting_rule_a_row_does() {
        let err = parse(&format!(
            "{HEAD}  columns:\n    - name: age\n      type: INT64\n      values:\n        - 30\n        - 40\n"
        ))
        .expect_err("refused");
        assert!(err.contains("written in quotes"), "{err}");
    }

    #[test]
    fn a_load_the_runner_cannot_read_says_which_line_and_why() {
        let col = "  columns:\n    - name: age\n      type: INT64\n      values:\n        - \"30\"\n        - \"40\"\n";
        for (text, want) in [
            (
                format!("{HEAD}{col}  pairs:\n    - from: 0\n      to: 2\n"),
                "reaches row 2",
            ),
            (
                format!(
                    "{HEAD}  columns:\n    - name: age\n      type: INT64\n      values:\n        - \"30\"\n"
                ),
                "1 values against a count of 2",
            ),
            (
                format!(
                    "{HEAD}  columns:\n    - name: age\n      type: DECIMAL\n      values:\n        - \"30\"\n        - \"40\"\n"
                ),
                "the encoding reserves",
            ),
            // A node is a row, and the rows are what the load is
            // making, so a column of them is a case that means nothing
            // however well it spells them.
            (
                format!(
                    "{HEAD}  columns:\n    - name: friend\n      type: NODE\n      values:\n        - \"person#0\"\n        - \"person#1\"\n"
                ),
                "cannot hold a graph value",
            ),
            (
                format!("load:\n  nodes: person\n  edges: knows\n  count: 0\n{col}"),
                "a load of no rows",
            ),
            (
                format!("load:\n  nodes: person\n  count: 2\n{col}"),
                "names its `edges:`",
            ),
            (
                format!("{HEAD}{col}  paris:\n    - from: 0\n      to: 1\n"),
                "no key \"paris\"",
            ),
        ] {
            let err = parse(&text).expect_err(&format!("{text:?} is refused"));
            assert!(err.contains(want), "{text:?} gave {err:?}");
        }
    }

    #[test]
    fn a_column_of_two_kinds_is_not_a_column() {
        let load = parse(&format!(
            "{HEAD}  columns:\n    - name: age\n      type: STRING\n      values:\n        - \"30\"\n        - \"40\"\n"
        ))
        .expect("parses");
        let mut mixed = load.columns[0].clone();
        mixed.values[1] = Value::Int(40);
        let err = mixed.stored().expect_err("refused");
        assert!(err.contains("more than one kind"), "{err}");
    }

    #[test]
    fn a_zoned_column_says_the_store_has_nowhere_to_put_the_offset() {
        let load = parse(&format!(
            "{HEAD}  columns:\n    - name: at\n      type: ZONEDTIME\n      values:\n        - \"12:00:00+07:00\"\n        - \"13:00:00+07:00\"\n"
        ))
        .expect("parses");
        let err = load.columns[0].stored().expect_err("refused");
        assert!(err.contains("nowhere to keep the offset"), "{err}");
    }
}
