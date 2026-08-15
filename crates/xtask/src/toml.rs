//! The subset of TOML `api-map.toml` is written in.
//!
//! A general TOML parser is not what this needs. The map is a
//! hand-written file that has to exist in nine repositories and be
//! read by people who did not write it, so the reader's job is to
//! refuse anything it does not understand rather than to accept as
//! much as possible. A key it silently ignored would be a mapping
//! somebody wrote and nobody applied, which is the exact failure the
//! completeness check exists to prevent.
//!
//! So this reads top-level `key = value` pairs and `[[array]]` tables
//! of them, with string, integer and boolean values, and it errors
//! with a line number on everything else: inline tables, arrays,
//! floats, dates, dotted keys, `[table]` headers, multi-line strings,
//! literal strings. Widening it is a change to this file, which is
//! where a reader would look for what the format allows.

use std::collections::BTreeMap;

/// One value. TOML has more types than this and the map needs none of
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
}

/// A set of key/value pairs: the top of the file, or one `[[array]]`
/// table. Pairs keep their written order, because the errors this
/// feeds quote line numbers and a reader is looking at the file.
#[derive(Debug, Clone, Default)]
pub struct Table {
    /// The line the table opened on, 1 based. Zero for the top of the
    /// file, which opened on nothing.
    pub line: usize,
    pub pairs: Vec<(String, Value)>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub fn int(&self, key: &str) -> Option<i64> {
        match self.get(key) {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        }
    }

    /// The keys that are not in `known`, so a caller can refuse a typo
    /// instead of dropping the pair on the floor.
    pub fn unknown(&self, known: &[&str]) -> Vec<&str> {
        self.pairs
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !known.contains(k))
            .collect()
    }
}

/// A whole file: the pairs above the first header, and the tables
/// under each `[[array]]` name.
#[derive(Debug, Clone, Default)]
pub struct Doc {
    pub root: Table,
    pub arrays: BTreeMap<String, Vec<Table>>,
}

impl Doc {
    pub fn array(&self, name: &str) -> &[Table] {
        self.arrays.get(name).map_or(&[], Vec::as_slice)
    }

    /// The array names that are not in `known`.
    pub fn unknown_arrays(&self, known: &[&str]) -> Vec<&str> {
        self.arrays
            .keys()
            .map(String::as_str)
            .filter(|k| !known.contains(k))
            .collect()
    }

    pub fn parse(text: &str) -> Result<Doc, String> {
        let mut doc = Doc::default();
        // Where the pairs being read now belong. The name is empty
        // until the first header, and the pairs go to the root.
        let mut open: Option<String> = None;
        for (n, raw) in text.lines().enumerate() {
            let line = n + 1;
            let content = strip_comment(raw)?;
            let content = content.trim();
            if content.is_empty() {
                continue;
            }
            if let Some(rest) = content.strip_prefix("[[") {
                let name = rest
                    .strip_suffix("]]")
                    .ok_or_else(|| format!("line {line}: `[[` with no `]]`"))?
                    .trim();
                if name.is_empty() || !name.chars().all(is_bare) {
                    return Err(format!("line {line}: {name:?} is not a table name"));
                }
                doc.arrays.entry(name.to_string()).or_default().push(Table {
                    line,
                    pairs: Vec::new(),
                });
                open = Some(name.to_string());
                continue;
            }
            if content.starts_with('[') {
                return Err(format!(
                    "line {line}: `[table]` is not read here, only `[[array]]`"
                ));
            }
            let (key, value) = content
                .split_once('=')
                .ok_or_else(|| format!("line {line}: no `=`, and this is not a header"))?;
            let key = key.trim();
            if key.is_empty() || !key.chars().all(is_bare) {
                return Err(format!("line {line}: {key:?} is not a bare key"));
            }
            let value = parse_value(value.trim(), line)?;
            let table = match &open {
                // `open` only ever names an array this loop pushed to,
                // so both lookups hold.
                Some(name) => doc
                    .arrays
                    .get_mut(name)
                    .and_then(|tables| tables.last_mut())
                    .expect("the open array has a table"),
                None => &mut doc.root,
            };
            if table.pairs.iter().any(|(k, _)| k == key) {
                return Err(format!("line {line}: {key} is set twice in one table"));
            }
            table.pairs.push((key.to_string(), value));
        }
        Ok(doc)
    }
}

/// Bare keys and table names here are ASCII words. TOML allows more
/// and the map wants none of it.
fn is_bare(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Everything from an unquoted `#` on is a comment. A `#` inside a
/// string is content, so this cannot be a `split_once('#')`: reasons
/// in the map are prose and prose contains the character.
fn strip_comment(line: &str) -> Result<&str, String> {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut quoted = false;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => quoted = !quoted,
            // A backslash inside a string hides the next byte, which
            // is how `\"` stays inside the string.
            b'\\' if quoted => i += 1,
            b'#' if !quoted => return Ok(&line[..i]),
            _ => {}
        }
        i += 1;
    }
    Ok(line)
}

fn parse_value(text: &str, line: usize) -> Result<Value, String> {
    match text {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "" => return Err(format!("line {line}: nothing after the `=`")),
        _ => {}
    }
    if let Some(rest) = text.strip_prefix('"') {
        // The closing quote is found by scanning rather than by taking
        // the last one, so that `"x" and "y"` is refused instead of
        // read as one string with a quote in the middle of it.
        let end = closing_quote(rest)
            .ok_or_else(|| format!("line {line}: a string that does not end on its line"))?;
        if end + 1 != rest.len() {
            return Err(format!(
                "line {line}: {:?} after the string ends",
                &rest[end + 1..]
            ));
        }
        return unescape(&rest[..end], line).map(Value::Str);
    }
    // Integers only, and no underscores or signs, because the one
    // integer in the map is a tier between 1 and 3.
    if text.chars().all(|c| c.is_ascii_digit()) {
        return text
            .parse()
            .map(Value::Int)
            .map_err(|e| format!("line {line}: {text} is not an integer: {e}"));
    }
    Err(format!(
        "line {line}: {text:?} is not a string, an integer, or a boolean"
    ))
}

/// The byte offset of the quote that closes a string whose opening
/// quote has already been taken off, or `None` if the line ends first.
fn closing_quote(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'"' => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn unescape(body: &str, line: usize) -> Result<String, String> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => return Err(format!("line {line}: \\{other} is not an escape")),
            None => return Err(format!("line {line}: a string ending in a backslash")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_above_the_first_header_are_the_files_own() {
        let doc = Doc::parse("schema = 1\ntarget = \"rust\"\n").expect("parses");
        assert_eq!(doc.root.int("schema"), Some(1));
        assert_eq!(doc.root.str("target"), Some("rust"));
        assert!(doc.arrays.is_empty());
    }

    #[test]
    fn each_header_opens_a_table_and_the_pairs_under_it_are_its_own() {
        let doc = Doc::parse(
            "[[entity]]\nid = \"a\"\n\n[[entity]]\nid = \"b\"\ntier = 3\n\n[[group]]\nprefix = \"p\"\n",
        )
        .expect("parses");
        let entities = doc.array("entity");
        assert_eq!(entities.len(), 2);
        assert_eq!(entities[0].str("id"), Some("a"));
        assert_eq!(entities[1].str("id"), Some("b"));
        assert_eq!(entities[1].int("tier"), Some(3));
        assert_eq!(doc.array("group").len(), 1);
        assert_eq!(doc.array("nothing").len(), 0);
    }

    #[test]
    fn a_table_reports_its_line_so_an_error_can_point_at_it() {
        let doc = Doc::parse("schema = 1\n\n[[entity]]\nid = \"a\"\n").expect("parses");
        assert_eq!(doc.root.line, 0);
        assert_eq!(doc.array("entity")[0].line, 3);
    }

    #[test]
    fn a_hash_inside_a_string_is_prose_and_not_a_comment() {
        let doc = Doc::parse("reason = \"issue #166 says so\" # but this goes\n").expect("parses");
        assert_eq!(doc.root.str("reason"), Some("issue #166 says so"));
    }

    #[test]
    fn an_escaped_quote_stays_inside_its_string() {
        let doc = Doc::parse("reason = \"the \\\"why\\\" of it\"\n").expect("parses");
        assert_eq!(doc.root.str("reason"), Some("the \"why\" of it"));
    }

    #[test]
    fn a_key_nobody_reads_can_be_asked_for() {
        let doc = Doc::parse("id = \"a\"\nreson = \"typo\"\n").expect("parses");
        assert_eq!(doc.root.unknown(&["id", "reason"]), vec!["reson"]);
        assert!(doc.root.unknown(&["id", "reson"]).is_empty());
    }

    #[test]
    fn an_array_nobody_reads_can_be_asked_for() {
        let doc = Doc::parse("[[groups]]\nprefix = \"p\"\n").expect("parses");
        assert_eq!(doc.unknown_arrays(&["group", "entity"]), vec!["groups"]);
    }

    #[test]
    fn what_the_reader_does_not_understand_it_refuses_and_says_where() {
        for (text, want) in [
            ("[table]\n", "line 1"),
            ("id = \"a\"\nid = \"b\"\n", "line 2"),
            ("tier = 1.5\n", "line 1"),
            ("ids = [1, 2]\n", "line 1"),
            ("id =\n", "line 1"),
            ("id = \"unterminated\n", "line 1"),
            ("id = 'literal'\n", "line 1"),
            ("id = \"a\\qb\"\n", "line 1"),
            ("id = \"x\" and \"y\"\n", "line 1"),
            ("just some words\n", "line 1"),
            ("[[entity\n", "line 1"),
            ("[[two words]]\n", "line 1"),
            ("a.b = 1\n", "line 1"),
        ] {
            let err = Doc::parse(text).expect_err(&format!("{text:?} is refused"));
            assert!(
                err.starts_with(want),
                "{text:?} gave {err:?}, which does not start with {want:?}"
            );
        }
    }

    #[test]
    fn the_line_number_survives_the_lines_before_it() {
        let err = Doc::parse("schema = 1\n\n# a comment\n\nbroken\n").expect_err("refused");
        assert!(err.starts_with("line 5"), "{err}");
    }
}
