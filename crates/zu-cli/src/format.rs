//! Laying a statement out, and moving nothing but the whitespace.
//!
//! A formatter that only ever changes space cannot change what a
//! statement means. That is the whole design: the text is cut into the
//! same tokens the lexer would cut it into, and then written back out
//! with the space between them decided rather than whatever was typed.
//! The test that holds it is the one that says the token stream before
//! and after are the same list, which is a stronger promise than a
//! formatter that also renames or re-cases can make.
//!
//! It follows that this works on text that does not parse. An editor
//! formats what is in the buffer, and what is in the buffer is
//! half-written most of the time, so a formatter that needed a parse
//! tree would be a formatter that stopped working exactly when the
//! buffer was in the state a person wants tidied.
//!
//! The layout is the one every graph language settled on: one clause to
//! a line, at the left margin, indented inside a braced subquery, and a
//! blank line between statements. Nothing here wraps a long line, since
//! a wrap needs to know what an expression is and this deliberately does
//! not.

/// What a run of characters is, for the purpose of putting space around
/// it. Fewer kinds than the highlighter has, because a formatter cares
/// about a word against a bracket and not about a keyword against an
/// identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Word,
    Number,
    /// A quoted string, and a delimited identifier, which is quoted the
    /// same way and is spaced the same way.
    Text,
    Param,
    /// `//` or `--` to the end of the line.
    Line,
    /// `/* */`, which may cross lines and is written back out with the
    /// lines it had.
    Block,
    /// Everything else, one lexer token at a time.
    Punct,
}

/// One token, and whether a newline stood in front of it.
///
/// The newline is kept for one reason: a comment at the end of a line
/// is about that line and a comment on its own line is about what comes
/// after, and those two want to end up where they started. Every other
/// token's position is decided from scratch.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tok<'a> {
    /// Where the token starts in the text it was cut from. The layout
    /// never asks, since it decides every position again from nothing.
    /// A caller reading the text rather than rewriting it does ask.
    at: usize,
    text: &'a str,
    kind: Kind,
    nl_before: bool,
}

/// One token, for a caller that wants the cuts and not the layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Piece<'a> {
    pub(crate) at: usize,
    pub(crate) text: &'a str,
    /// Whether this is a word: a name, or a keyword, which is a name the
    /// language spoke for. A number, a string, a comment, a parameter and
    /// a piece of punctuation are all not.
    pub(crate) word: bool,
}

/// The text cut into tokens, the same cuts the layout works on.
///
/// The point of sharing the cut is that a caller looking for the name
/// after a colon is looking at the same colons the formatter is: one
/// inside a string is not one, and neither is one inside a comment, and
/// both of those are already decided here.
pub(crate) fn pieces(text: &str) -> Vec<Piece<'_>> {
    lex(text)
        .into_iter()
        .map(|tok| Piece {
            at: tok.at,
            text: tok.text,
            word: tok.kind == Kind::Word,
        })
        .collect()
}

/// Words after which an open parenthesis is a group and not a call.
///
/// The list is the reserved words, so anything not on it is a name, and
/// a name in front of a parenthesis is a function being called. That way
/// round because the set of function names is open and the set of
/// clauses is not: `count(*)` and `my_udf(x)` both close up, and `MATCH
/// (a)` and `WHERE (a OR b)` both keep the space they read better with.
const SPACED: &[&str] = &[
    "ALL", "AND", "ANY", "AS", "ASC", "BY", "CALL", "CASE", "CREATE", "DELETE", "DESC", "DETACH",
    "DISTINCT", "DROP", "ELSE", "END", "EXISTS", "FILTER", "FINISH", "FOR", "FROM", "IN", "INSERT",
    "IS", "LET", "LIMIT", "MATCH", "MERGE", "NEXT", "NOT", "OF", "OFFSET", "ON", "OPTIONAL", "OR",
    "ORDER", "REMOVE", "RETURN", "SET", "SKIP", "THEN", "TO", "UNION", "UNWIND", "USE", "WHEN",
    "WHERE", "WITH", "XOR", "YIELD",
];

/// The words a line starts with.
///
/// A subset of [`SPACED`]: `AND` is a reserved word and does not begin a
/// clause, and a line break in front of every `AND` would turn a three
/// term filter into three lines. What is here is what a reader scans
/// down the left margin looking for.
const CLAUSES: &[&str] = &[
    "CALL", "CREATE", "DELETE", "DETACH", "DROP", "FILTER", "FINISH", "FOR", "INSERT", "LET",
    "LIMIT", "MATCH", "MERGE", "NEXT", "OFFSET", "OPTIONAL", "ORDER", "REMOVE", "RETURN", "SET",
    "SKIP", "UNION", "UNWIND", "USE", "WHERE", "WITH", "YIELD",
];

/// The document, laid out. Ends in a newline, because a file does.
pub(crate) fn format(text: &str) -> String {
    let toks = lex(text);
    if toks.is_empty() {
        // A buffer of nothing but space formats to nothing, and a
        // formatter that answered a lone newline would make an empty
        // file dirty every time it was saved.
        return String::new();
    }
    let mut out = String::new();
    // The brace stack says whether each open brace was a block, which
    // takes clauses on their own indented lines, or a map literal,
    // which is written along one line. Nothing else needs a stack: a
    // parenthesis and a bracket never take a line break, so their depth
    // is a number.
    let mut blocks: Vec<bool> = Vec::new();
    let mut parens = 0usize;
    let mut brackets = 0usize;
    // Whether the token just written leaves the next one glued to it,
    // which is how a unary minus reaches the number it is part of.
    let mut glue_next = false;
    for i in 0..toks.len() {
        let tok = &toks[i];
        let prev = i.checked_sub(1).map(|p| &toks[p]);
        let indent = blocks.iter().filter(|block| **block).count();

        // A closing brace un-indents the line it is on, so it is popped
        // before the indent is used rather than after.
        let (mut indent, closing_block) = if tok.text == "}" {
            let block = blocks.pop().unwrap_or(false);
            (indent.saturating_sub(usize::from(block)), block)
        } else {
            (indent, false)
        };
        if tok.text == "{" {
            let block = opens_block(prev);
            blocks.push(block);
        }

        let mut newline = false;
        if let Some(prev) = prev {
            if prev.kind == Kind::Line {
                // Whatever came after a line comment was after the end
                // of that line, and still is.
                newline = true;
            } else if prev.text == ";" {
                newline = true;
                // A blank line between statements, which is the one
                // piece of vertical space worth keeping.
                out.push('\n');
                indent = 0;
            } else if closing_block || opens_line(tok, prev, parens, brackets) {
                newline = true;
            }
        }

        if newline {
            out.push('\n');
            for _ in 0..indent {
                out.push_str("  ");
            }
        } else if !out.is_empty() && !glue_next && spaced(prev, tok, &toks, i, &blocks, brackets) {
            out.push(' ');
        }
        out.push_str(tok.text);

        glue_next = unary(prev, tok);
        match tok.text {
            "(" => parens += 1,
            ")" => parens = parens.saturating_sub(1),
            "[" => brackets += 1,
            "]" => brackets = brackets.saturating_sub(1),
            _ => {}
        }
    }
    // A file ends in a newline, unless the last thing in it is a string
    // or a comment nobody closed, where the newline would land inside
    // the quotes and become part of the text. That is a buffer somebody
    // is halfway through typing, and the formatter's promise is that it
    // moves whitespace and never adds a character to a literal.
    if !toks.last().is_some_and(open_ended) {
        out.push('\n');
    }
    out
}

/// Whether a token is a string or a comment that ran off the end of the
/// text without being closed.
fn open_ended(tok: &Tok<'_>) -> bool {
    match tok.kind {
        Kind::Text => {
            let quote = tok.text.as_bytes()[0];
            tok.text.len() < 2 || !tok.text.ends_with(quote as char)
        }
        Kind::Block => !tok.text.ends_with("*/"),
        _ => false,
    }
}

/// Whether an open brace begins a subquery rather than a map literal.
///
/// A map follows a name, a label or an opening bracket, as in `{name:
/// 'ada'}` after `(p:Person `. A block follows a clause word or nothing,
/// as in `CALL {`. Getting it wrong costs a map literal spread over
/// several lines or a subquery written along one, and never a wrong
/// statement, which is why one look backwards is enough.
fn opens_block(prev: Option<&Tok<'_>>) -> bool {
    match prev {
        None => true,
        Some(prev) => {
            prev.kind == Kind::Word && SPACED.contains(&prev.text.to_ascii_uppercase().as_str())
        }
    }
}

/// Whether this token starts a line of its own.
fn opens_line(tok: &Tok<'_>, prev: &Tok<'_>, parens: usize, brackets: usize) -> bool {
    if tok.kind == Kind::Line {
        // A comment that was on its own line stays on its own line, and
        // one that trailed a statement keeps trailing it.
        return tok.nl_before;
    }
    if parens > 0 || brackets > 0 {
        // Inside an expression there are no clauses, only words that
        // are spelled like them.
        return false;
    }
    if tok.kind != Kind::Word {
        return false;
    }
    let word = tok.text.to_ascii_uppercase();
    if !CLAUSES.contains(&word.as_str()) {
        return false;
    }
    // Two clause words in a row are one clause: `OPTIONAL MATCH`,
    // `DETACH DELETE`, `ORDER BY`, `UNION ALL`.
    !(prev.kind == Kind::Word && SPACED.contains(&prev.text.to_ascii_uppercase().as_str()))
}

/// Whether a minus sign in front of this token is the sign of a number
/// rather than a subtraction, which is the one case where what comes
/// after a token decides its spacing.
fn unary(prev: Option<&Tok<'_>>, tok: &Tok<'_>) -> bool {
    if tok.text != "-" && tok.text != "+" {
        return false;
    }
    match prev {
        None => true,
        Some(prev) => match prev.kind {
            Kind::Word => SPACED.contains(&prev.text.to_ascii_uppercase().as_str()),
            Kind::Punct => !matches!(prev.text, ")" | "]" | "}"),
            _ => false,
        },
    }
}

/// Whether a space goes between two tokens.
///
/// Written as the exceptions rather than as the rule, because the rule
/// is one space and the exceptions are what a reader would notice. They
/// are all local: at most one token either side, which is what keeps
/// this a function and not a second parser.
fn spaced(
    prev: Option<&Tok<'_>>,
    tok: &Tok<'_>,
    toks: &[Tok<'_>],
    i: usize,
    blocks: &[bool],
    brackets: usize,
) -> bool {
    let Some(prev) = prev else {
        return false;
    };
    let next = toks.get(i + 1).map(|t| t.text);
    // Nothing follows an opening bracket and nothing precedes a closing
    // one, a comma or a semicolon.
    if matches!(prev.text, "(" | "[" | "{") {
        return false;
    }
    if matches!(tok.text, ")" | "]" | "}" | "," | ";") {
        return false;
    }
    // A property access and a range are written closed up on both
    // sides. So is the colon in front of a label; the colon in a map
    // literal takes a space after it and none before, the way it does
    // in every language that has both.
    if matches!(prev.text, "." | "..") || matches!(tok.text, "." | "..") {
        return false;
    }
    if tok.text == ":" {
        return false;
    }
    if prev.text == ":" {
        return blocks.last() == Some(&false);
    }
    // The pattern connectors. Each is a run of single tokens the lexer
    // does not join, so they are closed up a pair at a time, which is
    // the same answer without a state machine.
    if tok.text == "-" && matches!(prev.text, ")" | "]") {
        return false;
    }
    if tok.text == ">" && prev.text == "-" {
        return false;
    }
    // `a < -1` is a comparison against a negative number and `<-(` is an
    // edge pointing backwards, and what tells them apart is what comes
    // after the minus. Both tokens of the pair ask the same question, so
    // both get the same answer.
    if tok.text == "<" && next == Some("-") {
        return !matches!(toks.get(i + 2).map(|t| t.text), Some("(" | "["));
    }
    if tok.text == "-" && prev.text == "<" {
        return !matches!(next, Some("(" | "["));
    }
    if matches!(tok.text, "(" | "[") {
        if matches!(prev.text, "-" | ">") {
            return false;
        }
        // A name in front of a bracket is a call or a subscript, and a
        // reserved word in front of one is a clause.
        if prev.kind == Kind::Word {
            return SPACED.contains(&prev.text.to_ascii_uppercase().as_str());
        }
        if matches!(prev.text, ")" | "]") {
            return false;
        }
    }
    // A quantifier inside a relationship pattern belongs to the type it
    // quantifies: `[:KNOWS*1..3]`, not `[:KNOWS * 1..3]`.
    if brackets > 0 && (tok.text == "*" || prev.text == "*") {
        return false;
    }
    true
}

/// The text as tokens, cut where the lexer cuts.
///
/// This is not the lexer. It cannot be: the lexer refuses a string that
/// is not closed, and an editor buffer is a string that is not closed
/// several times a second. An unterminated string or comment here runs
/// to the end of the text and comes back as one token, which is the
/// reading that keeps the rest of the buffer from being re-interpreted
/// as code halfway through a word.
fn lex(text: &str) -> Vec<Tok<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    let mut nl = false;
    while at < bytes.len() {
        let b = bytes[at];
        if b.is_ascii_whitespace() {
            nl |= b == b'\n';
            at += 1;
            continue;
        }
        let start = at;
        let kind = if (b == b'/' && bytes.get(at + 1) == Some(&b'/'))
            || (b == b'-' && bytes.get(at + 1) == Some(&b'-'))
        {
            while at < bytes.len() && bytes[at] != b'\n' {
                at += 1;
            }
            Kind::Line
        } else if b == b'/' && bytes.get(at + 1) == Some(&b'*') {
            at += 2;
            while at < bytes.len() && !(bytes[at - 1] == b'*' && bytes[at] == b'/') {
                at += 1;
            }
            at = (at + 1).min(bytes.len());
            Kind::Block
        } else if b == b'\'' || b == b'"' || b == b'`' {
            at += 1;
            while at < bytes.len() {
                if bytes[at] == b'\\' {
                    at += 2;
                    continue;
                }
                if bytes[at] == b {
                    // A doubled quote is the standard's escape for a
                    // quote, so it closes nothing.
                    if bytes.get(at + 1) == Some(&b) {
                        at += 2;
                        continue;
                    }
                    at += 1;
                    break;
                }
                at += 1;
            }
            at = at.min(bytes.len());
            Kind::Text
        } else if b == b'$' {
            at += 1;
            at = word_end(bytes, at);
            Kind::Param
        } else if b.is_ascii_digit() {
            at = number_end(bytes, at);
            Kind::Number
        } else if is_word(b) {
            at = word_end(bytes, at);
            Kind::Word
        } else {
            at += match (b, bytes.get(at + 1)) {
                (b'|', Some(b'|')) | (b'<', Some(b'=' | b'>')) | (b'>', Some(b'=')) => 2,
                (b'.', Some(b'.')) => 2,
                _ => 1,
            };
            Kind::Punct
        };
        out.push(Tok {
            at: start,
            text: &text[start..at],
            kind,
            nl_before: nl,
        });
        nl = false;
    }
    out
}

/// Whether a byte can be in a word. Everything outside ASCII counts,
/// because a name in the file may be in any script and cutting one in
/// half would put a space in the middle of somebody's table.
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn word_end(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && is_word(bytes[at]) {
        at += 1;
    }
    at
}

/// The end of a number, decimal point and exponent included. A pair of
/// dots after the digits is a range and not a point, so `1..3` is three
/// tokens and not a number with something odd after it.
fn number_end(bytes: &[u8], mut at: usize) -> usize {
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }
    if bytes.get(at) == Some(&b'.') && bytes.get(at + 1).is_some_and(u8::is_ascii_digit) {
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        let mut ex = at + 1;
        if matches!(bytes.get(ex), Some(b'+' | b'-')) {
            ex += 1;
        }
        if bytes.get(ex).is_some_and(u8::is_ascii_digit) {
            at = ex;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
        }
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tokens, as the strings they are, which is what has to come
    /// through a format unchanged.
    fn words(text: &str) -> Vec<String> {
        lex(text).into_iter().map(|t| t.text.to_string()).collect()
    }

    #[test]
    fn a_clause_starts_a_line_and_a_pattern_stays_closed_up() {
        assert_eq!(
            format("match (a:Person)-[:KNOWS]->(b) where a.age>21 return b.name"),
            "match (a:Person)-[:KNOWS]->(b)\nwhere a.age > 21\nreturn b.name\n"
        );
    }

    #[test]
    fn two_clause_words_in_a_row_are_one_clause() {
        assert_eq!(
            format("optional match (a) detach delete a"),
            "optional match (a)\ndetach delete a\n"
        );
        assert_eq!(
            format("match (a) return a order   by a.id limit 10"),
            "match (a)\nreturn a\norder by a.id\nlimit 10\n"
        );
    }

    #[test]
    fn a_call_closes_up_and_a_clause_does_not() {
        assert_eq!(format("return count(*)"), "return count(*)\n");
        assert_eq!(format("return my_udf ( 1 , 2 )"), "return my_udf(1, 2)\n");
        assert_eq!(format("match(a)return a"), "match (a)\nreturn a\n");
        assert_eq!(format("return (1+2)*3"), "return (1 + 2) * 3\n");
    }

    #[test]
    fn a_minus_is_a_sign_or_a_subtraction_depending_on_what_is_in_front() {
        assert_eq!(format("return -1"), "return -1\n");
        assert_eq!(format("return a.x-1"), "return a.x - 1\n");
        assert_eq!(format("return [ -1 , 2 ]"), "return [-1, 2]\n");
        assert_eq!(format("match (a) where a.x < -1 return a"), {
            "match (a)\nwhere a.x < -1\nreturn a\n"
        });
        assert_eq!(
            format("match (a)<-[:KNOWS]-(b) return a"),
            "match (a)<-[:KNOWS]-(b)\nreturn a\n"
        );
    }

    #[test]
    fn a_colon_is_a_label_or_a_map_key_depending_on_the_brace_it_is_in() {
        assert_eq!(
            format("match (p:Person{name:'ada'}) return p"),
            "match (p:Person {name: 'ada'})\nreturn p\n"
        );
    }

    #[test]
    fn a_subquery_brace_indents_and_a_map_brace_does_not() {
        assert_eq!(
            format("match (a) call { match (b) return b } return a"),
            "match (a)\ncall {\n  match (b)\n  return b\n}\nreturn a\n"
        );
    }

    #[test]
    fn a_quantifier_belongs_to_the_type_it_quantifies() {
        assert_eq!(
            format("match (a)-[:KNOWS*1..3]->(b) return b"),
            "match (a)-[:KNOWS*1..3]->(b)\nreturn b\n"
        );
    }

    #[test]
    fn statements_are_separated_by_a_blank_line() {
        assert_eq!(format("return 1; return 2"), "return 1;\n\nreturn 2\n");
    }

    #[test]
    fn a_comment_stays_on_the_line_it_was_written_for() {
        assert_eq!(
            format("return 1 // the trailing one\nreturn 2"),
            "return 1 // the trailing one\nreturn 2\n"
        );
        assert_eq!(
            format("// the standalone one\nreturn 1"),
            "// the standalone one\nreturn 1\n"
        );
        assert_eq!(format("return /* mid */ 1"), "return /* mid */ 1\n");
    }

    #[test]
    fn nothing_but_space_formats_to_nothing() {
        assert_eq!(format(""), "");
        assert_eq!(format("  \n\n  "), "");
    }

    /// The promise the whole design rests on. Every case above, and the
    /// awkward ones no case above covers, put through the formatter and
    /// checked token for token.
    #[test]
    fn formatting_moves_whitespace_and_nothing_else() {
        let cases = [
            "match (a:Person)-[:KNOWS]->(b) where a.age>21 return b.name",
            "return 'a;b' + \"c'd\" + `an odd name`",
            "return 'it''s doubled'",
            "match (a) where a.x IN [1,2,3] and a.y IS NOT NULL return a",
            "return 1.5e-3, -1, 2*-3, 1..3",
            "call { match (b) return b } return 1",
            "// leading\nreturn $p // trailing\n/* block\n   spanning */ return 2",
            "return 'unterminated",
            "match (",
            "return a.b.c[0].d",
            "insert (:Person {name: 'ada', age: 36})",
            "match (a)--(b) return a",
            "return 中文表.属性",
        ];
        for case in cases {
            let once = format(case);
            assert_eq!(words(&once), words(case), "tokens changed for {case:?}");
            assert_eq!(format(&once), once, "not idempotent for {case:?}");
        }
    }
}
