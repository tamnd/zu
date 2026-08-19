//! The subset of YAML the corpus is written in.
//!
//! YAML is a large language and the corpus needs a small corner of it:
//! block mappings, block sequences, and scalars. Everything else is
//! refused with a line number, for the reason the map reader gives.
//! These files are hand written, they have to be read by people in
//! nine repositories who did not write them, and a construct the
//! reader quietly reinterpreted would be a case that says one thing to
//! a reviewer and another to the runner.
//!
//! So: two space indentation and no tabs, `- ` with exactly one space,
//! plain, single quoted and double quoted scalars on one line, and
//! comments. No flow collections, no block scalars, no anchors, no
//! aliases, no tags, no document markers, no multi document streams.
//! A `doc:` field is written unwrapped on one line, which is the same
//! rule the markdown in this repository follows.
//!
//! Whether a scalar was quoted survives parsing, because the value
//! encoding turns on it. An INT64 written bare is a number that some
//! reader in some language will round, and refusing it is the whole
//! point of the encoding.

/// One node of a document, with the line it started on so that
/// everything downstream can say where.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Scalar {
        text: String,
        quoted: bool,
        line: usize,
    },
    Seq {
        items: Vec<Node>,
        line: usize,
    },
    Map {
        pairs: Vec<(String, Node)>,
        line: usize,
    },
    /// A key with nothing under it. It is a node rather than an error
    /// because a case that expects no rows back writes `rows:` and
    /// stops, and that is a real expectation which needs a spelling.
    /// Every accessor says no to it, so a `name:` left blank is still
    /// caught by whoever wanted a name.
    Empty {
        line: usize,
    },
}

impl Node {
    pub fn line(&self) -> usize {
        match self {
            Node::Scalar { line, .. }
            | Node::Seq { line, .. }
            | Node::Map { line, .. }
            | Node::Empty { line } => *line,
        }
    }

    /// What kind of node this is, for an error that has to say what it
    /// found instead of what it wanted.
    pub fn kind(&self) -> &'static str {
        match self {
            Node::Scalar { .. } => "a scalar",
            Node::Seq { .. } => "a sequence",
            Node::Map { .. } => "a mapping",
            Node::Empty { .. } => "nothing",
        }
    }

    pub fn scalar(&self) -> Option<(&str, bool)> {
        match self {
            Node::Scalar { text, quoted, .. } => Some((text, *quoted)),
            _ => None,
        }
    }

    pub fn str(&self) -> Option<&str> {
        self.scalar().map(|(text, _)| text)
    }

    pub fn seq(&self) -> Option<&[Node]> {
        match self {
            Node::Seq { items, .. } => Some(items),
            _ => None,
        }
    }

    /// A sequence, counting a key with nothing under it as the empty
    /// one. Only a caller for whom empty is a meaningful answer should
    /// reach for this; the rest want [`Node::seq`], so that a list
    /// somebody left unfinished is refused rather than read as none.
    pub fn seq_or_empty(&self) -> Option<&[Node]> {
        match self {
            Node::Empty { .. } => Some(&[]),
            _ => self.seq(),
        }
    }

    pub fn map(&self) -> Option<&[(String, Node)]> {
        match self {
            Node::Map { pairs, .. } => Some(pairs),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Node> {
        self.map()?.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// The keys that are not in `known`, so a caller can refuse a typo
    /// rather than drop the field on the floor.
    pub fn unknown(&self, known: &[&str]) -> Vec<&str> {
        self.map()
            .unwrap_or(&[])
            .iter()
            .map(|(k, _)| k.as_str())
            .filter(|k| !known.contains(k))
            .collect()
    }
}

/// A document, or the first thing in it the reader will not read.
pub fn parse(text: &str) -> Result<Node, String> {
    let lines = lex(text)?;
    if lines.is_empty() {
        return Err("the file has nothing in it".to_string());
    }
    if lines[0].indent != 0 {
        return Err(format!("line {}: the first line is indented", lines[0].no));
    }
    let mut i = 0;
    let node = node(&lines, &mut i, 0)?;
    match lines.get(i) {
        None => Ok(node),
        Some(line) => Err(format!(
            "line {}: this belongs to nothing above it",
            line.no
        )),
    }
}

/// One meaningful line: its indent, whether a `- ` opened it, what is
/// left after that, and where it was.
#[derive(Debug)]
struct Line {
    indent: usize,
    dash: bool,
    text: String,
    no: usize,
}

/// Lines, with blanks and comments dropped and every `- ` split into
/// the item it opens and the content that followed it on the same
/// line. Splitting here rather than in the parser is what lets
/// `- name: x` and a `name: x` on its own line be the same shape by
/// the time anything looks at them.
fn lex(text: &str) -> Result<Vec<Line>, String> {
    let mut out = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let no = n + 1;
        if let Some(col) = raw.find('\t') {
            return Err(format!(
                "line {no}: a tab at column {}, and indentation here is spaces",
                col + 1
            ));
        }
        let content = strip_comment(raw).trim_end();
        let indent = content.len() - content.trim_start().len();
        let mut rest = content.trim_start();
        if rest.is_empty() {
            continue;
        }
        if rest == "---" || rest == "..." {
            return Err(format!(
                "line {no}: {rest:?} opens or closes a document, and a file here holds one"
            ));
        }
        if !indent.is_multiple_of(2) {
            return Err(format!(
                "line {no}: indented {indent}, and indentation here goes two spaces at a time"
            ));
        }

        let dash = rest == "-" || rest.starts_with("- ");
        if !dash {
            out.push(Line {
                indent,
                dash: false,
                text: rest.to_string(),
                no,
            });
            continue;
        }
        rest = rest.strip_prefix('-').unwrap_or(rest);
        if rest.starts_with("  ") {
            return Err(format!(
                "line {no}: a `- ` takes exactly one space, so that what follows it lines up with \
                 the lines under it"
            ));
        }
        let rest = rest.trim_start();
        if rest.starts_with("- ") {
            return Err(format!(
                "line {no}: a sequence opening straight into another one, which nothing here needs"
            ));
        }
        out.push(Line {
            indent,
            dash: true,
            text: String::new(),
            no,
        });
        if !rest.is_empty() {
            out.push(Line {
                indent: indent + 2,
                dash: false,
                text: rest.to_string(),
                no,
            });
        }
    }
    Ok(out)
}

/// Everything from an unquoted ` #` on is a comment.
///
/// Three rules keep this from eating content. A `#` starts a comment
/// only with whitespace before it, because one inside a word is part
/// of the word. A quote opens a quoted run only with whitespace
/// before it, because a quote inside a word is part of the word too,
/// which is what lets a `doc:` say "it's" without opening a run that
/// never closes. And a quote that opens nothing that closes was not a
/// run at all, which is what lets a `query:` hold `cast('  42  ' AS
/// INT64)`: the second quote has a space before it and so looks like
/// an opening one, and nothing after it closes.
///
/// That last rule is why this cannot fail. A value that really does
/// open a quoted scalar and never close it is caught where the scalar
/// is read, because that is the only place the value's own start is
/// known, and it is the only place the difference matters.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    let opens = |i: usize| i == 0 || bytes[i - 1].is_ascii_whitespace();
    while i < bytes.len() {
        match bytes[i] {
            b'#' if opens(i) => return &line[..i],
            q @ (b'"' | b'\'') if opens(i) => {
                if let Some(end) = closing_quote(&line[i + 1..], q) {
                    i += 1 + end;
                }
            }
            _ => {}
        }
        i += 1;
    }
    line
}

/// The byte offset of the quote that closes a run whose opening quote
/// has already been passed, or `None` if the line ends first.
///
/// The two quoting styles hide a quote differently: a double quoted
/// run escapes with a backslash, and a single quoted run doubles the
/// quote, which is the only escape it has.
fn closing_quote(rest: &str, quote: u8) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && quote == b'"' {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            if quote == b'\'' && bytes.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return Some(i);
        }
        i += 1;
    }
    None
}

/// The node that starts at `*i` and is indented `indent`, leaving `*i`
/// on the first line that is not part of it.
fn node(lines: &[Line], i: &mut usize, indent: usize) -> Result<Node, String> {
    if lines[*i].dash {
        return seq(lines, i, indent);
    }
    // A mapping key is a bare word and a `:`. Anything else at this
    // position is a scalar standing on its own, which is what the
    // items of a sequence of scalars are.
    if split_key(&lines[*i].text).is_some() {
        return map(lines, i, indent);
    }
    let line = &lines[*i];
    *i += 1;
    scalar(&line.text, line.no)
}

fn seq(lines: &[Line], i: &mut usize, indent: usize) -> Result<Node, String> {
    let line = lines[*i].no;
    let mut items = Vec::new();
    while lines.get(*i).is_some_and(|l| l.dash && l.indent == indent) {
        let at = lines[*i].no;
        *i += 1;
        match lines.get(*i) {
            Some(next) if next.indent == indent + 2 => items.push(node(lines, i, indent + 2)?),
            Some(next) if next.indent > indent => {
                return Err(format!(
                    "line {}: indented {}, where an item of the sequence on line {at} is indented \
                     {}",
                    next.no,
                    next.indent,
                    indent + 2
                ));
            }
            _ => return Err(format!("line {at}: a `-` with nothing after it")),
        }
    }
    Ok(Node::Seq { items, line })
}

fn map(lines: &[Line], i: &mut usize, indent: usize) -> Result<Node, String> {
    let line = lines[*i].no;
    let mut pairs: Vec<(String, Node)> = Vec::new();
    while lines
        .get(*i)
        .is_some_and(|l| !l.dash && l.indent == indent && split_key(&l.text).is_some())
    {
        let at = lines[*i].no;
        let (key, rest) = split_key(&lines[*i].text).expect("the loop just checked");
        let (key, rest) = (key.to_string(), rest.to_string());
        *i += 1;
        let value = if !rest.is_empty() {
            scalar(&rest, at)?
        } else {
            match lines.get(*i) {
                Some(next) if next.indent == indent + 2 => node(lines, i, indent + 2)?,
                Some(next) if next.indent > indent => {
                    return Err(format!(
                        "line {}: indented {}, where what is under `{key}:` on line {at} is \
                         indented {}",
                        next.no,
                        next.indent,
                        indent + 2
                    ));
                }
                _ => Node::Empty { line: at },
            }
        };
        if pairs.iter().any(|(k, _)| *k == key) {
            return Err(format!("line {at}: {key} is set twice in one mapping"));
        }
        pairs.push((key, value));
    }
    Ok(Node::Map { pairs, line })
}

/// The key and the rest of the line, when the line opens a mapping
/// entry. A key is a bare word, and the `:` after it ends the line or
/// has a space after it, so that a plain scalar holding a colon is
/// still a scalar.
fn split_key(text: &str) -> Option<(&str, &str)> {
    let (key, rest) = match text.split_once(": ") {
        Some((key, rest)) => (key, rest.trim_start()),
        None => (text.strip_suffix(':')?, ""),
    };
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some((key, rest))
}

fn scalar(text: &str, line: usize) -> Result<Node, String> {
    for quote in *b"\"'" {
        let Some(body) = text.strip_prefix(quote as char) else {
            continue;
        };
        // The closing quote is found by scanning rather than by taking
        // the last one on the line, so that `"a" and "b"` is refused
        // instead of read as one scalar with quotes in the middle.
        let end = closing_quote(body, quote).ok_or(format!(
            "line {line}: a {} that opens and does not close on its line",
            quote as char
        ))?;
        if end + 1 != body.len() {
            return Err(format!(
                "line {line}: {:?} after the scalar ends",
                &body[end + 1..]
            ));
        }
        let body = &body[..end];
        // A single quoted run has one escape, the doubled quote, and a
        // backslash in it is a backslash.
        let text = match quote {
            b'"' => unescape(body, line)?,
            _ => body.replace("''", "'"),
        };
        return Ok(Node::Scalar {
            text,
            quoted: true,
            line,
        });
    }
    if let Some(c) = text.chars().next()
        && "[]{}&*!|>%@`".contains(c)
    {
        return Err(format!(
            "line {line}: a plain scalar opening with {c:?}, which is a construct this reader does \
             not read"
        ));
    }
    Ok(Node::Scalar {
        text: text.to_string(),
        quoted: false,
        line,
    })
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
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('0') => out.push('\0'),
            Some(other) => return Err(format!("line {line}: \\{other} is not an escape")),
            None => return Err(format!("line {line}: a scalar ending in a backslash")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(text: &str) -> Node {
        parse(text).unwrap_or_else(|e| panic!("{text:?} should parse: {e}"))
    }

    #[test]
    fn a_mapping_of_scalars_is_the_shape_everything_else_is_made_of() {
        let doc = ok("schema: 1\nsuite: int\n");
        assert_eq!(doc.get("schema").and_then(Node::str), Some("1"));
        assert_eq!(doc.get("suite").and_then(Node::str), Some("int"));
        assert_eq!(doc.get("nothing"), None);
    }

    #[test]
    fn a_sequence_item_carrying_a_mapping_is_the_same_shape_as_one_written_out() {
        let doc =
            ok("cases:\n  - name: a\n    query: RETURN 1\n  - name: b\n    query: RETURN 2\n");
        let cases = doc.get("cases").and_then(Node::seq).expect("a sequence");
        assert_eq!(cases.len(), 2);
        assert_eq!(cases[0].get("name").and_then(Node::str), Some("a"));
        assert_eq!(cases[0].get("query").and_then(Node::str), Some("RETURN 1"));
        assert_eq!(cases[1].get("name").and_then(Node::str), Some("b"));
    }

    #[test]
    fn a_sequence_of_scalars_is_not_read_as_anything_cleverer() {
        let doc = ok("columns:\n  - n\n  - m\n");
        let columns = doc.get("columns").and_then(Node::seq).expect("a sequence");
        assert_eq!(
            columns.iter().filter_map(Node::str).collect::<Vec<_>>(),
            ["n", "m"]
        );
    }

    #[test]
    fn nesting_goes_as_deep_as_a_list_of_records_needs() {
        let doc = ok(
            "rows:\n  - values:\n      - type: LIST\n        value:\n          - type: INT64\n            value: \"1\"\n",
        );
        let rows = doc.get("rows").and_then(Node::seq).expect("rows");
        let values = rows[0].get("values").and_then(Node::seq).expect("values");
        assert_eq!(values[0].get("type").and_then(Node::str), Some("LIST"));
        let inner = values[0].get("value").and_then(Node::seq).expect("a list");
        assert_eq!(inner[0].get("value").and_then(Node::str), Some("1"));
    }

    #[test]
    fn whether_a_scalar_was_quoted_survives_because_the_encoding_turns_on_it() {
        let doc = ok("bare: 42\nquoted: \"42\"\nsingle: '42'\n");
        assert_eq!(doc.get("bare").and_then(Node::scalar), Some(("42", false)));
        assert_eq!(doc.get("quoted").and_then(Node::scalar), Some(("42", true)));
        assert_eq!(doc.get("single").and_then(Node::scalar), Some(("42", true)));
    }

    #[test]
    fn a_colon_inside_a_value_is_part_of_it_and_not_another_key() {
        let doc = ok("query: RETURN datetime('2024-01-01T00:00:00')\n");
        assert_eq!(
            doc.get("query").and_then(Node::str),
            Some("RETURN datetime('2024-01-01T00:00:00')")
        );
    }

    #[test]
    fn a_hash_is_a_comment_only_where_a_comment_can_start() {
        let doc = ok(
            "# the whole line\nname: a  # and the end of this one\nhash: \"a # b\"\nword: c#d\n",
        );
        assert_eq!(doc.get("name").and_then(Node::str), Some("a"));
        assert_eq!(doc.get("hash").and_then(Node::str), Some("a # b"));
        assert_eq!(doc.get("word").and_then(Node::str), Some("c#d"));
    }

    #[test]
    fn a_quote_that_never_closes_is_an_ordinary_character_inside_a_plain_scalar() {
        // The second quote has a space before it, so it looks like the
        // start of a run, and there is nothing after it to close one.
        let doc = ok("query: RETURN cast('  42  ' AS INT64) AS n  # a note\n");
        assert_eq!(
            doc.get("query").and_then(Node::str),
            Some("RETURN cast('  42  ' AS INT64) AS n")
        );
    }

    #[test]
    fn a_quoted_scalar_that_never_closes_is_still_refused() {
        let err = parse("doc: \"and then\n").expect_err("refused");
        assert!(err.contains("does not close"), "{err}");
    }

    #[test]
    fn an_escape_and_a_doubled_quote_are_the_two_ways_a_quote_gets_in() {
        let doc = ok("a: \"say \\\"no\\\"\"\nb: 'say ''no'''\n");
        assert_eq!(doc.get("a").and_then(Node::str), Some("say \"no\""));
        assert_eq!(doc.get("b").and_then(Node::str), Some("say 'no'"));
    }

    #[test]
    fn the_control_characters_a_query_can_hold_all_have_an_escape() {
        let doc = ok("a: \"one\\ntwo\\rthree\\tfour\\0five\"\n");
        assert_eq!(
            doc.get("a").and_then(Node::str),
            Some("one\ntwo\rthree\tfour\0five")
        );
    }

    #[test]
    fn a_negative_number_is_a_scalar_and_not_a_sequence() {
        let doc = ok("value: -1\n");
        assert_eq!(doc.get("value").and_then(Node::str), Some("-1"));
    }

    #[test]
    fn every_node_says_which_line_it_started_on() {
        let doc = ok("schema: 1\n\ncases:\n  - name: a\n    query: RETURN 1\n");
        let cases = doc.get("cases").and_then(Node::seq).expect("cases");
        assert_eq!(cases[0].line(), 4);
        assert_eq!(cases[0].get("query").expect("query").line(), 5);
    }

    #[test]
    fn a_key_with_nothing_under_it_is_a_node_and_every_accessor_says_no_to_it() {
        let doc = ok("rows:\n");
        let rows = doc.get("rows").expect("a node");
        assert_eq!(rows.kind(), "nothing");
        assert_eq!(rows.str(), None);
        assert_eq!(rows.seq(), None);
        // The one caller for whom empty is an answer, which is a case
        // that expects no rows back.
        assert_eq!(rows.seq_or_empty(), Some(&[][..]));
    }

    #[test]
    fn a_key_nobody_reads_can_be_asked_for() {
        let doc = ok("name: a\nqeury: RETURN 1\n");
        assert_eq!(doc.unknown(&["name", "query"]), vec!["qeury"]);
    }

    #[test]
    fn what_the_reader_does_not_read_it_refuses_and_says_where() {
        for (text, want) in [
            ("", "nothing in it"),
            ("  name: a\n", "line 1"),
            ("name:\ta\n", "line 1"),
            ("name: a\n   doc: b\n", "line 2"),
            ("a: 1\n b: 2\n", "line 2"),
            ("name: a\nname: b\n", "line 2"),
            ("cases:\n  -\n", "line 2"),
            ("cases:\n  -  name: a\n", "line 2"),
            ("cases:\n  - - a\n", "line 2"),
            ("columns: [n, m]\n", "line 1"),
            ("doc: >\n  folded\n", "line 1"),
            ("doc: |\n  literal\n", "line 1"),
            ("anchor: &a 1\n", "line 1"),
            ("---\nname: a\n", "line 1"),
            ("a: \"unterminated\n", "line 1"),
            ("a: \"bad \\q escape\"\n", "line 1"),
            ("a: 'unterminated\n", "line 1"),
            ("a: 1\ncases:\n      - b\n", "line 3"),
        ] {
            let err = parse(text).expect_err(&format!("{text:?} is refused"));
            assert!(
                err.contains(want),
                "{text:?} gave {err:?}, which does not mention {want:?}"
            );
        }
    }
}
