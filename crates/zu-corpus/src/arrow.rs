//! What a result looks like on the way out through Arrow.
//!
//! A client that reads rows one at a time and a client that exports a
//! million of them to a dataframe are the same client, and only one of
//! those paths is covered by a case that asserts values. The other one
//! has its own contract: a column of dates is a `Date32` and not a
//! string of digits, a year-month duration is a month-day-nano interval
//! because that is the interval every reader implements, a node is a
//! struct of the name of its table and the row it is, and a time with
//! an offset is refused rather than quietly moved to UTC. None of that
//! shows up in a row a case compares.
//!
//! So a case may say what the export gives as well as what the rows
//! are, and the runner checks both against one statement. What it
//! checks is the schema, field by field and into the nested types, and
//! how many rows came back through the stream. The schema is spelled in
//! the C Data Interface's own format strings, `l` for an int64 and
//! `+s` for a struct, because that is the one spelling every language
//! sees the same: a client in Python reads it off a pyarrow field, a
//! client in C reads it out of the struct, and neither has to agree
//! with the other about what to call a type first.
//!
//! Values are not read back here. A consumer that decoded every array
//! by hand in each of nine languages would be nine new decoders under
//! test, which is more of our own code and not more of the contract;
//! the rows the case already asserts are the same values by another
//! road.

use crate::yaml::Node;

/// One field of the schema an export gives, and the fields under it
/// when it is a struct or a list.
///
/// A list has exactly one field under it, which Arrow names `item`,
/// and a case writes that out rather than leaving it implied: a client
/// that named it `element` would export something no reader lines up
/// with what another client wrote.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    /// The C Data Interface format string, `l` for an int64, `u` for a
    /// string, `tsn:` for a timestamp in nanoseconds with no zone.
    pub format: String,
    pub children: Vec<Field>,
}

/// What a case says about the export.
#[derive(Debug, Clone, PartialEq)]
pub enum Export {
    /// The columns, in the order the statement projects them.
    Columns(Vec<Field>),
    /// Arrow has no type for one of the columns, so there is no export
    /// to describe. A time with an offset is the one a statement can
    /// write today: Arrow has a time and a timestamp and nothing in
    /// between, and dropping the offset would move the value.
    Refused,
}

/// The `arrow:` of a case, or what is wrong with it.
pub fn parse(node: &Node) -> Result<Export, String> {
    if let Some(text) = node.str() {
        return match text {
            "refused" => Ok(Export::Refused),
            _ => Err(format!(
                "line {}: `arrow:` is the columns the export gives, or `refused` for a result \
                 Arrow has no type for, and this is {text:?}",
                node.line()
            )),
        };
    }
    Ok(Export::Columns(fields(node)?))
}

fn fields(node: &Node) -> Result<Vec<Field>, String> {
    let items = node.seq().ok_or(format!(
        "line {}: `arrow:` is a sequence of fields, and this is {}",
        node.line(),
        node.kind()
    ))?;
    items.iter().map(one).collect()
}

fn one(node: &Node) -> Result<Field, String> {
    let line = node.line();
    if node.map().is_none() {
        return Err(format!(
            "line {line}: an Arrow field is a mapping of `name` and `format`, and this is {}",
            node.kind()
        ));
    }
    if let Some(key) = node.unknown(&["name", "format", "children"]).first() {
        return Err(format!("line {line}: an Arrow field has no key {key:?}"));
    }
    let text = |key: &str| -> Result<String, String> {
        node.get(key)
            .and_then(Node::str)
            .map(str::to_string)
            .ok_or(format!("line {line}: an Arrow field has a `{key}:`"))
    };
    let name = text("name")?;
    let format = text("format")?;
    if format.is_empty() {
        return Err(format!(
            "line {line}: an empty format string is not a type Arrow has"
        ));
    }
    let children = match node.get("children") {
        None => Vec::new(),
        Some(list) => fields(list)?,
    };
    // A nested format is the one thing about a format string this
    // reader knows, and it is worth knowing here: a case that wrote the
    // fields of a struct under a `u` would be asserting something the
    // export cannot produce, and finding that out at load time says so
    // with a line number rather than as a failure in a report.
    match (format.starts_with('+'), children.is_empty()) {
        (true, true) => Err(format!(
            "line {line}: {format:?} is a nested type and the fields under it are part of it"
        )),
        (false, false) => Err(format!(
            "line {line}: {format:?} holds no fields, so nothing goes under it"
        )),
        _ => Ok(Field {
            name,
            format,
            children,
        }),
    }
}

/// How a report names the whole result, which is the place the columns
/// of an export are in.
pub const RESULT: &str = "the result";

/// What the export gave that the case did not want, or `None` when the
/// two agree.
///
/// `got` is the schema of the stream, which is a struct of the columns,
/// so what is compared against the case is the fields under it. It is
/// the schema as the C Data Interface carries it rather than as arrow's
/// Rust types hold it, because that is what the C runner has at this
/// point too: the two then compare the same bytes and report them in
/// the same words.
///
/// The comparison walks the schema and the case's fields together and
/// stops at the first difference, for the reason the row comparison
/// does: the first is nearly always the cause of the rest.
#[cfg(feature = "arrow")]
pub fn schema(got: &arrow::ffi::FFI_ArrowSchema, want: &[Field]) -> Option<String> {
    let columns: Vec<&arrow::ffi::FFI_ArrowSchema> = got.children().collect();
    fields_of("", &columns, want)
}

/// The fields under one place, where the place is the dotted path of
/// the field they are under and the empty one is the result itself.
#[cfg(feature = "arrow")]
fn fields_of(prefix: &str, got: &[&arrow::ffi::FFI_ArrowSchema], want: &[Field]) -> Option<String> {
    let place = match prefix.is_empty() {
        true => RESULT.to_string(),
        false => format!("{prefix:?}"),
    };
    if got.len() != want.len() {
        return Some(format!(
            "arrow gives {} fields in {place} where the case wants {}",
            got.len(),
            want.len()
        ));
    }
    for (i, (got, want)) in got.iter().zip(want).enumerate() {
        let name = got.name().unwrap_or("");
        if name != want.name {
            return Some(format!(
                "arrow field {} in {place} is named {name:?} where the case wants {:?}",
                i + 1,
                want.name
            ));
        }
        // The path is the case's own names joined with dots, which is
        // how a field inside a path inside a column is pointed at
        // without printing the whole schema at somebody.
        let path = match prefix.is_empty() {
            true => want.name.clone(),
            false => format!("{prefix}.{}", want.name),
        };
        if got.format() != want.format {
            return Some(format!(
                "arrow field {path:?} is {:?} where the case wants {:?}",
                got.format(),
                want.format
            ));
        }
        let children: Vec<&arrow::ffi::FFI_ArrowSchema> = got.children().collect();
        if let Some(why) = fields_of(&path, &children, &want.children) {
            return Some(why);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Result<Export, String> {
        parse(&crate::yaml::parse(text).expect("the fixture parses"))
    }

    #[test]
    fn a_column_is_a_name_and_a_format_string() {
        let export = read("- name: n\n  format: l\n").expect("it reads");
        assert_eq!(
            export,
            Export::Columns(vec![Field {
                name: "n".to_string(),
                format: "l".to_string(),
                children: Vec::new(),
            }])
        );
    }

    #[test]
    fn a_struct_carries_the_fields_under_it() {
        let export = read(
            "- name: n\n  format: \"+s\"\n  children:\n    - name: table\n      format: u\n    - \
             name: offset\n      format: L\n",
        )
        .expect("it reads");
        let Export::Columns(columns) = export else {
            panic!("columns");
        };
        assert_eq!(columns[0].children.len(), 2);
        assert_eq!(columns[0].children[1].name, "offset");
    }

    #[test]
    fn a_result_arrow_has_no_type_for_is_written_as_a_refusal() {
        assert_eq!(read("refused\n").expect("it reads"), Export::Refused);
    }

    #[test]
    fn a_word_that_is_not_refused_is_not_a_schema() {
        let why = read("no\n").expect_err("refused or the columns");
        assert!(why.contains("`arrow:` is the columns"), "{why}");
    }

    #[test]
    fn a_flat_type_holds_no_fields_and_a_nested_one_is_its_fields() {
        let why = read("- name: n\n  format: l\n  children:\n    - name: item\n      format: l\n")
            .expect_err("a flat type holds nothing");
        assert!(why.contains("holds no fields"), "{why}");
        let why = read("- name: n\n  format: \"+s\"\n").expect_err("a struct is its fields");
        assert!(why.contains("is a nested type"), "{why}");
    }

    #[test]
    fn a_field_that_is_missing_half_of_itself_is_refused() {
        let why = read("- name: n\n").expect_err("a field has a format");
        assert!(why.contains("has a `format:`"), "{why}");
        let why = read("- format: l\n").expect_err("a field has a name");
        assert!(why.contains("has a `name:`"), "{why}");
        let why = read("- name: n\n  format: l\n  type: INT64\n").expect_err("no key type");
        assert!(why.contains("no key \"type\""), "{why}");
    }
}
