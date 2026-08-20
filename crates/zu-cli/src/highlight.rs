//! Colour for the statement being typed.
//!
//! This is a scanner and not a parser. It reads the text once, left to
//! right, and gives every run of it one of six kinds, which is all a
//! colour needs and is the only thing that can be done to a statement
//! that is half typed: a parser's answer to `MATCH (a:Per` is a syntax
//! error, and a syntax error is not a colour.
//!
//! The keyword list is a display list. A word missing from it is a word
//! that comes out uncoloured, which is a cosmetic drift and not a wrong
//! answer, and that is the trade for not making the shell depend on the
//! parser's internals or the parser export a table it does not use.
//! dx/13 §1's tree-sitter and Shiki grammars are for the documentation
//! and for editors, where a real grammar earns its cost; a shell that
//! repaints on every keystroke wants the scan to stay a scan.
//!
//! Colour is off unless the terminal says otherwise. `NO_COLOR` with
//! any value at all turns it off, which is the no-color.org rule, and
//! so does `TERM=dumb`, because a terminal that says it is dumb is the
//! one place an escape sequence lands as text.

/// What a run of characters is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Keyword,
    /// A quoted string, single, double or backtick.
    Text,
    Number,
    /// A line comment, which opens with `//` or with `--`, and a
    /// `/* */` block.
    Comment,
    /// `$name`, which is the one thing in a statement a shell user is
    /// most likely to misspell, since nothing checks it until the
    /// statement runs.
    Param,
    /// Anything else: identifiers, operators, punctuation, whitespace.
    Plain,
}

impl Kind {
    /// The escape that turns colour on for this kind.
    ///
    /// Four colours and a dim, all of them from the sixteen colours
    /// every terminal has had since the 1970s, because the 256 colour
    /// and true colour forms are the ones that come out unreadable on
    /// somebody's light background or somebody's tmux.
    fn on(self) -> &'static str {
        match self {
            Kind::Keyword => "\x1b[1;34m",
            Kind::Text => "\x1b[32m",
            Kind::Number => "\x1b[36m",
            Kind::Comment => "\x1b[2m",
            Kind::Param => "\x1b[35m",
            Kind::Plain => "",
        }
    }
}

/// Whether this terminal should be sent colour at all.
pub(crate) fn wanted() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
}

/// The statement's lines, coloured, one string per line of the buffer.
///
/// Per line rather than in one piece because the editor draws a line at
/// a time and puts its own prompt in front of each, and per buffer
/// rather than per line because a string or a block comment can cross a
/// newline and the scanner has to know it is inside one.
pub(crate) fn lines(text: &str, on: bool) -> Vec<String> {
    if !on {
        return text.split('\n').map(str::to_string).collect();
    }
    let mut out = vec![String::new()];
    let mut kind = Kind::Plain;
    for (run, next) in scan(text) {
        for (i, part) in run.split('\n').enumerate() {
            if i > 0 {
                // A colour never crosses a line, because the editor
                // writes a prompt between the two and the prompt is not
                // part of the string that is being coloured.
                if kind != Kind::Plain {
                    out.last_mut().expect("a line").push_str("\x1b[0m");
                }
                out.push(String::new());
                kind = Kind::Plain;
            }
            if part.is_empty() {
                continue;
            }
            if kind != next {
                let line = out.last_mut().expect("a line");
                if kind != Kind::Plain {
                    line.push_str("\x1b[0m");
                }
                line.push_str(next.on());
                kind = next;
            }
            out.last_mut().expect("a line").push_str(part);
        }
    }
    if kind != Kind::Plain {
        out.last_mut().expect("a line").push_str("\x1b[0m");
    }
    out
}

/// What the character before `at` is part of, which is what says
/// whether the cursor is sitting inside a string or a comment.
///
/// The character before rather than the character at, because a cursor
/// is between two characters and the one that decides is the one it was
/// typed after: at the end of `'x'` the cursor is out of the string,
/// and at the end of `'x` it is still in one.
pub(crate) fn kind_at(text: &str, at: usize) -> Kind {
    if at == 0 {
        return Kind::Plain;
    }
    let mut seen = 0;
    for (run, kind) in scan(text) {
        seen += run.len();
        if seen >= at {
            return kind;
        }
    }
    Kind::Plain
}

/// The text, split into runs that each have one kind.
///
/// Runs are returned in order and cover every byte, so joining them
/// gives the input back. That is what the test checks, and it is the
/// property that keeps a colouring bug from becoming a lost character.
pub(crate) fn scan(text: &str) -> Vec<(&str, Kind)> {
    if let Some(runs) = command(text) {
        return runs;
    }
    let mut runs = Vec::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &text[at..];
        let len = match bytes[at] {
            b'\'' | b'"' | b'`' => {
                let end = quoted(rest);
                runs.push((&rest[..end], Kind::Text));
                end
            }
            b'/' | b'-' if rest.starts_with("//") || rest.starts_with("--") => {
                let end = rest.find('\n').unwrap_or(rest.len());
                runs.push((&rest[..end], Kind::Comment));
                end
            }
            b'/' if rest.starts_with("/*") => {
                let end = rest[2..]
                    .find("*/")
                    .map_or(rest.len(), |i| i + 4)
                    .min(rest.len());
                runs.push((&rest[..end], Kind::Comment));
                end
            }
            b'$' => {
                let end = 1 + word_len(&rest[1..]);
                runs.push((&rest[..end], Kind::Param));
                end
            }
            b'0'..=b'9' => {
                let end = number_len(rest);
                runs.push((&rest[..end], Kind::Number));
                end
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let end = word_len(rest);
                let word = &rest[..end];
                let kind = if is_keyword(word) {
                    Kind::Keyword
                } else {
                    Kind::Plain
                };
                runs.push((word, kind));
                end
            }
            _ => {
                // Everything else in one run per character boundary,
                // which keeps the scanner from having to know what an
                // operator is.
                let end = rest
                    .char_indices()
                    .nth(1)
                    .map_or(rest.len(), |(i, _)| i)
                    .max(1);
                runs.push((&rest[..end], Kind::Plain));
                end
            }
        };
        at += len;
    }
    runs
}

/// A backslash line, if this is one: the space in front of it, the
/// command itself, and everything after, and `None` for the statements
/// that are everything else.
///
/// A backslash line goes to [`crate::meta`] rather than to the engine,
/// so nothing in it is coloured as the language. `\d node` names a
/// table, and a `node` painted like a keyword would be the shell
/// telling the user it had read the line as a statement.
fn command(text: &str) -> Option<Vec<(&str, Kind)>> {
    let start = text.len() - text.trim_start().len();
    if !text[start..].starts_with('\\') {
        return None;
    }
    let after = &text[start + 1..];
    // The name is a word, except for `\?`, which is a command spelled
    // in one character that is not one.
    let name = match word_len(after) {
        0 => after
            .chars()
            .next()
            .filter(|c| !c.is_whitespace())
            .map_or(0, char::len_utf8),
        len => len,
    };
    let end = start + 1 + name;
    let mut runs = Vec::with_capacity(3);
    if start > 0 {
        runs.push((&text[..start], Kind::Plain));
    }
    runs.push((&text[start..end], Kind::Keyword));
    if end < text.len() {
        runs.push((&text[end..], Kind::Plain));
    }
    Some(runs)
}

/// How long a quoted string is, counting its quotes.
///
/// A doubled quote is the standard's escape and does not end it, and a
/// backslash escapes the character after it. An unterminated string
/// runs to the end of the text, which is what a half typed one is.
fn quoted(text: &str) -> usize {
    let quote = text.as_bytes()[0];
    let bytes = text.as_bytes();
    let mut at = 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b if b == quote => {
                if bytes.get(at + 1) == Some(&quote) {
                    at += 2;
                } else {
                    return (at + 1).min(bytes.len());
                }
            }
            _ => at += 1,
        }
    }
    bytes.len()
}

/// How long an identifier or keyword is.
fn word_len(text: &str) -> usize {
    text.find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(text.len())
}

/// How long a number is, decimal point and exponent included.
fn number_len(text: &str) -> usize {
    let mut end = 0;
    let bytes = text.as_bytes();
    let mut seen_dot = false;
    while end < bytes.len() {
        match bytes[end] {
            b'0'..=b'9' => end += 1,
            // The separator a long number may be written with, which
            // stands between two digits and nowhere else.
            b'_' if bytes.get(end + 1).is_some_and(u8::is_ascii_digit) => end += 2,
            b'.' if !seen_dot && bytes.get(end + 1).is_some_and(u8::is_ascii_digit) => {
                seen_dot = true;
                end += 1;
            }
            b'e' | b'E' => {
                let after = if matches!(bytes.get(end + 1), Some(b'+' | b'-')) {
                    end + 2
                } else {
                    end + 1
                };
                if bytes.get(after).is_some_and(u8::is_ascii_digit) {
                    end = after + 1;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    end
}

/// Whether a word is one of the language's, ignoring case.
pub(crate) fn is_keyword(word: &str) -> bool {
    KEYWORDS
        .binary_search_by(|k| {
            k.len()
                .cmp(&word.len())
                .then_with(|| k.chars().cmp(word.chars().map(|c| c.to_ascii_uppercase())))
        })
        .is_ok()
}

/// The words the parser matches, plus the literals and the built-in
/// functions, sorted by length and then by spelling so the lookup is a
/// binary search over a table with no allocation behind it.
///
/// Sorted by length first because that is the comparison
/// [`is_keyword`] can make without upper-casing the word into a string
/// of its own, and a keystroke is not a place to allocate.
///
/// A word here is upper case, since that is what the lookup compares
/// against, and LOG10 shows that a digit may stand in one. The sort
/// puts the digit where its byte falls, which is before every letter,
/// and the scanner reads a digit as part of a word already, so LOG10 is
/// one word and not LOG with a number behind it.
pub(crate) const KEYWORDS: &[&str] = &[
    "AS",
    "BY",
    "ID",
    "IF",
    "IN",
    "IS",
    "LN",
    "NO",
    "OF",
    "OR",
    "TO",
    "ABS",
    "ALL",
    "AND",
    "ANY",
    "ASC",
    "AVG",
    "COS",
    "COT",
    "DAY",
    "END",
    "EXP",
    "FOR",
    "LET",
    "LOG",
    "MAX",
    "MIN",
    "MOD",
    "NFC",
    "NFD",
    "NOT",
    "SET",
    "SIN",
    "SUM",
    "TAN",
    "USE",
    "XOR",
    "ACOS",
    "ASIN",
    "ATAN",
    "BOTH",
    "CALL",
    "CASE",
    "CAST",
    "CEIL",
    "COPY",
    "DESC",
    "DROP",
    "EDGE",
    "ELSE",
    "ENDS",
    "FROM",
    "HOUR",
    "KEEP",
    "LAST",
    "LEFT",
    "LIKE",
    "LIST",
    "NEXT",
    "NFKC",
    "NFKD",
    "NODE",
    "NULL",
    "SAME",
    "SIGN",
    "SIZE",
    "SKIP",
    "SQRT",
    "THEN",
    "TRIM",
    "TRUE",
    "TYPE",
    "WALK",
    "WHEN",
    "WITH",
    "YEAR",
    "ARRAY",
    "BTRIM",
    "COUNT",
    "EDGES",
    "FALSE",
    "FIRST",
    "FLOOR",
    "GRAPH",
    "GROUP",
    "LIMIT",
    "LOG10",
    "LOWER",
    "LTRIM",
    "MATCH",
    "MERGE",
    "MONTH",
    "NULLS",
    "ORDER",
    "PATHS",
    "POWER",
    "RIGHT",
    "ROUND",
    "RTRIM",
    "START",
    "TRAIL",
    "TYPED",
    "UNION",
    "UPPER",
    "VALUE",
    "WHERE",
    "YIELD",
    "COMMIT",
    "CREATE",
    "DELETE",
    "DETACH",
    "EXCEPT",
    "EXISTS",
    "FILTER",
    "FINISH",
    "GROUPS",
    "INSERT",
    "MINUTE",
    "NULLIF",
    "OFFSET",
    "RECORD",
    "REMOVE",
    "RETURN",
    "SCHEMA",
    "SECOND",
    "SIMPLE",
    "SOURCE",
    "STARTS",
    "UNWIND",
    "ACYCLIC",
    "CEILING",
    "COLLECT",
    "DEGREES",
    "ELEMENT",
    "LABELED",
    "LEADING",
    "RADIANS",
    "REPLACE",
    "SESSION",
    "BINDINGS",
    "COALESCE",
    "CONTAINS",
    "DIRECTED",
    "DISTINCT",
    "ELEMENTS",
    "NODETACH",
    "OPTIONAL",
    "PROPERTY",
    "ROLLBACK",
    "SHORTEST",
    "TRAILING",
    "ASCENDING",
    "DIFFERENT",
    "INTERSECT",
    "NORMALIZE",
    "OTHERWISE",
    "DESCENDING",
    "ELEMENT_ID",
    "HOME_GRAPH",
    "LOCAL_TIME",
    "NORMALIZED",
    "ORDINALITY",
    "PROPERTIES",
    "REPEATABLE",
    "CARDINALITY",
    "CHAR_LENGTH",
    "DESTINATION",
    "PATH_LENGTH",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "OCTET_LENGTH",
    "RELATIONSHIP",
    "ALL_DIFFERENT",
    "CURRENT_GRAPH",
    "LOCAL_TIMESTAMP",
    "PROPERTY_EXISTS",
    "CHARACTER_LENGTH",
    "DURATION_BETWEEN",
    "CURRENT_TIMESTAMP",
    "HOME_PROPERTY_GRAPH",
    "CURRENT_PROPERTY_GRAPH",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_keyword_table_is_sorted_the_way_the_lookup_reads_it() {
        for pair in KEYWORDS.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                (a.len(), a) < (b.len(), b),
                "{a} and {b} are out of order, and the search is a binary one"
            );
            assert!(
                a.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
                "{a} is not upper case, and the lookup upper-cases the word it is given"
            );
        }
    }

    #[test]
    fn keywords_are_keywords_in_any_case_and_identifiers_are_not() {
        assert!(is_keyword("MATCH"));
        assert!(is_keyword("match"));
        assert!(is_keyword("Match"));
        assert!(!is_keyword("matched"));
        assert!(!is_keyword("mat"));
        assert!(!is_keyword("person"));
        assert!(!is_keyword(""));
    }

    /// Joining the runs gives the text back, which is the property that
    /// stops a colouring bug from eating a character.
    fn covers(text: &str) {
        let joined: String = scan(text).into_iter().map(|(run, _)| run).collect();
        assert_eq!(joined, text, "the runs do not cover the text");
    }

    #[test]
    fn every_byte_lands_in_exactly_one_run() {
        for text in [
            "MATCH (a:Person {name: 'x'}) RETURN a.age + 1",
            "RETURN 'it''s', \"two\", `three`",
            "RETURN 1.5e-3, 0.25, 42",
            "// a comment\nRETURN $p",
            "/* block */ RETURN 1",
            "RETURN 'unterminated",
            "/* unterminated",
            "RETURN 中 + '中'",
            "\\d node",
            "  \\?",
            "\\",
            "",
        ] {
            covers(text);
        }
    }

    #[test]
    fn a_backslash_line_is_a_command_and_not_a_statement() {
        assert_eq!(
            scan("\\d node"),
            [("\\d", Kind::Keyword), (" node", Kind::Plain)]
        );
        assert_eq!(scan("\\?"), [("\\?", Kind::Keyword)]);
        assert_eq!(
            scan("  \\timing on"),
            [
                ("  ", Kind::Plain),
                ("\\timing", Kind::Keyword),
                (" on", Kind::Plain)
            ]
        );
        // A backslash inside a statement is not a command, so the
        // statement is coloured the way it always was.
        assert!(scan("RETURN 'a\\tb'").contains(&("'a\\tb'", Kind::Text)));
    }

    #[test]
    fn the_kinds_are_what_the_text_says_they_are() {
        let runs = scan("MATCH (a:Person) WHERE a.age > 30 RETURN $p, 'x' // why");
        let kinds: Vec<_> = runs
            .iter()
            .filter(|(run, _)| !run.trim().is_empty())
            .map(|(run, kind)| (*run, *kind))
            .collect();
        assert!(kinds.contains(&("MATCH", Kind::Keyword)));
        assert!(kinds.contains(&("WHERE", Kind::Keyword)));
        assert!(kinds.contains(&("Person", Kind::Plain)));
        assert!(kinds.contains(&("30", Kind::Number)));
        assert!(kinds.contains(&("$p", Kind::Param)));
        assert!(kinds.contains(&("'x'", Kind::Text)));
        assert!(kinds.contains(&("// why", Kind::Comment)));
        // Two minus signs open a comment as surely as two solidi do
        // (GB02), so the rest of the line is dim and a subtraction of a
        // negative number is written with its spaces.
        assert!(scan("RETURN 1 -- 2").contains(&("-- 2", Kind::Comment)));
        assert!(
            scan("RETURN 1 - -2")
                .iter()
                .all(|(_, k)| *k != Kind::Comment)
        );
    }

    #[test]
    fn a_number_stops_where_it_stops_being_one() {
        assert_eq!(number_len("42"), 2);
        assert_eq!(number_len("1.5"), 3);
        assert_eq!(number_len("1.5e-3 "), 6);
        assert_eq!(number_len("1e"), 1);
        // A property access, not a decimal point.
        assert_eq!(number_len("1.name"), 1);
        assert_eq!(number_len("1_000_000"), 9);
        // A separator with a name behind it is where the number ends.
        assert_eq!(number_len("1_x"), 1);
    }

    #[test]
    fn the_cursor_knows_when_it_is_inside_a_string_or_a_comment() {
        assert_eq!(kind_at("RETURN 'x", 9), Kind::Text);
        assert_eq!(kind_at("RETURN 'x' ", 11), Kind::Plain);
        assert_eq!(kind_at("// why", 6), Kind::Comment);
        assert_eq!(kind_at("/* why", 6), Kind::Comment);
        assert_eq!(kind_at("/* why */ RE", 12), Kind::Plain);
        assert_eq!(kind_at("RETURN", 0), Kind::Plain);
        assert_eq!(kind_at("", 0), Kind::Plain);
    }

    #[test]
    fn colour_is_off_when_it_is_off_and_the_text_survives_either_way() {
        let plain = lines("MATCH (a)\nRETURN a", false);
        assert_eq!(plain, ["MATCH (a)", "RETURN a"]);
        let painted = lines("MATCH (a)\nRETURN a", true);
        assert_eq!(painted.len(), 2);
        assert!(
            painted[0].starts_with("\x1b[1;34mMATCH\x1b[0m"),
            "{painted:?}"
        );
        assert!(painted[1].contains("RETURN"), "{painted:?}");
        // Every colour a line opened, that line closed, so the prompt
        // drawn after it is not painted with it.
        for line in &painted {
            let opened = line.matches('\x1b').count() - line.matches("\x1b[0m").count();
            assert_eq!(opened, line.matches("\x1b[0m").count(), "{line:?}");
        }
    }

    #[test]
    fn a_string_across_two_lines_is_coloured_on_both_and_closed_on_each() {
        let painted = lines("RETURN 'one\ntwo'", true);
        assert_eq!(painted.len(), 2);
        assert!(painted[0].contains("\x1b[32m'one"), "{painted:?}");
        assert!(painted[0].ends_with("\x1b[0m"), "{painted:?}");
        assert!(painted[1].starts_with("\x1b[32mtwo'"), "{painted:?}");
    }

    #[test]
    fn the_painted_text_is_the_text_with_escapes_taken_out() {
        let text = "MATCH (a:Person {n: 'x'}) // why\nRETURN a.age, $p, 1.5";
        let painted = lines(text, true).join("\n");
        let mut stripped = String::new();
        let mut rest = painted.as_str();
        while let Some(at) = rest.find('\x1b') {
            stripped.push_str(&rest[..at]);
            let end = rest[at..].find('m').expect("an sgr escape ends in m");
            rest = &rest[at + end + 1..];
        }
        stripped.push_str(rest);
        assert_eq!(stripped, text);
    }
}
