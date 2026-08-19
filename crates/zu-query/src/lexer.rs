//! Hand-written lexer for zuQL (docs/07 §1, grammar in
//! docs/grammar.ebnf).
//!
//! Tokens carry their byte span so the parser can report positions as
//! line and column. Keywords are not lexed specially: they are plain
//! identifiers the parser matches case-insensitively, which keeps the
//! token set small and the keyword list in one place. `<-` and `->` are
//! deliberately not tokens: `a < -1` must lex as less-than then minus,
//! so the parser assembles pattern arrows from the single characters.
//!
//! Two minus signs are a comment to the end of the line (GB02), which
//! costs the two spellings Cypher writes an edge with and GQL does not,
//! `(a)--(b)` and `(a)-->(b)`. GQL abbreviates those `(a)-(b)` and
//! `(a)->(b)`, the standard says a double minus opens a comment
//! wherever it is written, and a lexer cannot have it both ways: the
//! two readings are the same characters. A statement that writes the
//! Cypher spelling loses the rest of its line to the comment and fails
//! to parse, which is the loud way for this to go wrong.

use zu_common::gqlstatus::codes;
use zu_common::{Result, ZuError};

/// One lexed token with its byte span in the source.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// Unquoted identifier or keyword, original case preserved.
    Ident(String),
    /// Backtick-quoted identifier, quotes stripped.
    QuotedIdent(String),
    /// Integer literal. Kept unsigned; unary minus is the parser's.
    Int(u64),
    Float(f64),
    Str(String),
    /// `$name` or `$0` parameter, the `$` stripped.
    Param(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Semicolon,
    Dot,
    DotDot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Eq,
    /// `<>`
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Pipe,
    /// `||`, the concatenation operator of ISO 20.23. It is two of the
    /// character a label expression writes one of, so it is lexed here
    /// rather than in the parser: a label alternation never writes two
    /// bars against each other, and reading them as one token is what
    /// keeps a concatenation out of the pattern grammar.
    Concat,
    /// `&`, the label expression conjunction.
    Amp,
    /// `!`, the label expression negation.
    Bang,
    /// `~`, which an edge pattern writes where a direction would go to
    /// say the edge has none (GH02).
    Tilde,
}

impl TokenKind {
    /// Short human name for error messages.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident(s) | TokenKind::QuotedIdent(s) => format!("'{s}'"),
            TokenKind::Int(v) => format!("'{v}'"),
            TokenKind::Float(v) => format!("'{v}'"),
            TokenKind::Str(_) => "string literal".into(),
            TokenKind::Param(p) => format!("'${p}'"),
            TokenKind::LParen => "'('".into(),
            TokenKind::RParen => "')'".into(),
            TokenKind::LBracket => "'['".into(),
            TokenKind::RBracket => "']'".into(),
            TokenKind::LBrace => "'{'".into(),
            TokenKind::RBrace => "'}'".into(),
            TokenKind::Comma => "','".into(),
            TokenKind::Colon => "':'".into(),
            TokenKind::Semicolon => "';'".into(),
            TokenKind::Dot => "'.'".into(),
            TokenKind::DotDot => "'..'".into(),
            TokenKind::Plus => "'+'".into(),
            TokenKind::Minus => "'-'".into(),
            TokenKind::Star => "'*'".into(),
            TokenKind::Slash => "'/'".into(),
            TokenKind::Percent => "'%'".into(),
            TokenKind::Caret => "'^'".into(),
            TokenKind::Eq => "'='".into(),
            TokenKind::Ne => "'<>'".into(),
            TokenKind::Lt => "'<'".into(),
            TokenKind::Le => "'<='".into(),
            TokenKind::Gt => "'>'".into(),
            TokenKind::Ge => "'>='".into(),
            TokenKind::Pipe => "'|'".into(),
            TokenKind::Concat => "'||'".into(),
            TokenKind::Amp => "'&'".into(),
            TokenKind::Bang => "'!'".into(),
            TokenKind::Tilde => "'~'".into(),
        }
    }
}

/// Every failure the lexer can produce is a `42001 invalid syntax`:
/// the text is not GQL. Anything richer than that is the parser's job.
///
/// Raised through the source rather than through the place, because
/// the line an error is on is quoted back on the error and this is one
/// of the two places that still has the text to quote from.
fn err(source: &str, offset: usize, detail: &str) -> ZuError {
    ZuError::gql_in(codes::C42001, source, offset, detail)
}

/// Lexes the whole source into tokens.
pub fn lex(source: &str) -> Result<Vec<Token>> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut ix = 0usize;
    while ix < bytes.len() {
        let b = bytes[ix];
        // Whitespace and comments carry no tokens.
        if b.is_ascii_whitespace() {
            ix += 1;
            continue;
        }
        // GB02 and GB03: a comment to the end of the line opens with
        // either two solidi or two minus signs, and ISO gives them one
        // rule between them. Two minus signs are also two operators,
        // and `1 - -2` keeps its spaces for that reason: a subtraction
        // of a negative number written with nothing between the signs
        // is a comment in every SQL there has ever been.
        if (b == b'/' && bytes.get(ix + 1) == Some(&b'/'))
            || (b == b'-' && bytes.get(ix + 1) == Some(&b'-'))
        {
            while ix < bytes.len() && bytes[ix] != b'\n' {
                ix += 1;
            }
            continue;
        }
        if b == b'/' && bytes.get(ix + 1) == Some(&b'*') {
            let open = ix;
            ix += 2;
            loop {
                if ix + 1 >= bytes.len() {
                    return Err(err(source, open, "unterminated block comment"));
                }
                if bytes[ix] == b'*' && bytes[ix + 1] == b'/' {
                    ix += 2;
                    break;
                }
                ix += 1;
            }
            continue;
        }
        let start = ix;
        let single = |kind: TokenKind| Token {
            kind,
            start,
            end: start + 1,
        };
        match b {
            b'(' => tokens.push(single(TokenKind::LParen)),
            b')' => tokens.push(single(TokenKind::RParen)),
            b'[' => tokens.push(single(TokenKind::LBracket)),
            b']' => tokens.push(single(TokenKind::RBracket)),
            b'{' => tokens.push(single(TokenKind::LBrace)),
            b'}' => tokens.push(single(TokenKind::RBrace)),
            b',' => tokens.push(single(TokenKind::Comma)),
            b':' => tokens.push(single(TokenKind::Colon)),
            b';' => tokens.push(single(TokenKind::Semicolon)),
            b'+' => tokens.push(single(TokenKind::Plus)),
            b'-' => tokens.push(single(TokenKind::Minus)),
            b'~' => tokens.push(single(TokenKind::Tilde)),
            b'*' => tokens.push(single(TokenKind::Star)),
            b'/' => tokens.push(single(TokenKind::Slash)),
            b'%' => tokens.push(single(TokenKind::Percent)),
            b'^' => tokens.push(single(TokenKind::Caret)),
            b'=' => tokens.push(single(TokenKind::Eq)),
            b'|' => {
                let (kind, len) = match bytes.get(ix + 1) {
                    Some(b'|') => (TokenKind::Concat, 2),
                    _ => (TokenKind::Pipe, 1),
                };
                tokens.push(Token {
                    kind,
                    start,
                    end: start + len,
                });
                ix += len;
                continue;
            }
            b'&' => tokens.push(single(TokenKind::Amp)),
            b'!' => tokens.push(single(TokenKind::Bang)),
            b'<' => {
                let (kind, len) = match bytes.get(ix + 1) {
                    Some(b'=') => (TokenKind::Le, 2),
                    Some(b'>') => (TokenKind::Ne, 2),
                    _ => (TokenKind::Lt, 1),
                };
                tokens.push(Token {
                    kind,
                    start,
                    end: start + len,
                });
                ix += len;
                continue;
            }
            b'>' => {
                let (kind, len) = match bytes.get(ix + 1) {
                    Some(b'=') => (TokenKind::Ge, 2),
                    _ => (TokenKind::Gt, 1),
                };
                tokens.push(Token {
                    kind,
                    start,
                    end: start + len,
                });
                ix += len;
                continue;
            }
            b'.' => {
                if bytes.get(ix + 1) == Some(&b'.') {
                    tokens.push(Token {
                        kind: TokenKind::DotDot,
                        start,
                        end: start + 2,
                    });
                    ix += 2;
                } else {
                    tokens.push(single(TokenKind::Dot));
                    ix += 1;
                }
                continue;
            }
            // A grave accent opens a delimited identifier and the other
            // two quotes open a string, and all three are read the same
            // way. `@` in front of any of them is GL11, the form that
            // hands back the characters as written, so a lone `@` is
            // still nothing at all.
            b'\'' | b'"' | b'`' | b'@'
                if matches!(
                    bytes.get(if b == b'@' { ix + 1 } else { ix }),
                    Some(b'\'' | b'"' | b'`')
                ) =>
            {
                let open = if b == b'@' { ix + 1 } else { ix };
                let (text, end) = lex_quoted(source, open, b != b'@', start)?;
                let kind = if bytes[open] == b'`' {
                    if text.is_empty() {
                        return Err(err(source, start, "empty backtick identifier"));
                    }
                    TokenKind::QuotedIdent(text)
                } else {
                    TokenKind::Str(text)
                };
                tokens.push(Token { kind, start, end });
                ix = end;
                continue;
            }
            b'$' => {
                // ISO 21.10 writes a parameter two ways. One dollar
                // sign is a general parameter, which holds a value,
                // and two is a substituted parameter, which holds a
                // reference: a graph, a binding table, a graph type or
                // a procedure. The two spellings name the same
                // parameter here, because what a parameter holds is
                // settled by the value that arrives and not by the
                // characters in front of the name, which is the rule
                // `GRAPH $g` beside `$g` is already read under.
                let name = ix + 1 + usize::from(bytes.get(ix + 1) == Some(&b'$'));
                let mut end = name;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                if end == name {
                    return Err(err(source, ix, "expected a parameter name after '$'"));
                }
                tokens.push(Token {
                    kind: TokenKind::Param(source[name..end].to_string()),
                    start,
                    end,
                });
                ix = end;
                continue;
            }
            b'0'..=b'9' => {
                let (kind, end) = lex_number(source, ix)?;
                tokens.push(Token { kind, start, end });
                ix = end;
                continue;
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let mut end = ix + 1;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                tokens.push(Token {
                    kind: TokenKind::Ident(source[ix..end].to_string()),
                    start,
                    end,
                });
                ix = end;
                continue;
            }
            _ => {
                let ch = source[ix..].chars().next().unwrap_or('\u{fffd}');
                return Err(err(source, ix, &format!("unexpected character '{ch}'")));
            }
        }
        ix += 1;
    }
    Ok(tokens)
}

/// One escape of ISO 21.3, the reverse solidus at `at` unread, pushing
/// what it stands for onto `text` and answering the index after it.
///
/// The four quote escapes are here rather than in the caller because
/// the set does not depend on which quote the sequence was opened with:
/// `'\`'` is a legal way to write a grave accent inside a string, and
/// the reader of a statement should not have to remember which of the
/// three characters the escape list changes with.
fn escape(source: &str, at: usize, text: &mut String) -> Result<usize> {
    let bytes = source.as_bytes();
    let escaped = bytes
        .get(at + 1)
        .ok_or_else(|| err(source, at, "unterminated escape"))?;
    // The two unicode escapes name a code point by its hexadecimal
    // digits, four of them or six, and the digits have to make a
    // character: a surrogate half is a number that no text contains.
    if matches!(escaped, b'u' | b'U') {
        let width = if *escaped == b'u' { 4 } else { 6 };
        let end = at + 2 + width;
        let digits = source
            .get(at + 2..end)
            .filter(|d| d.chars().all(|c| c.is_ascii_hexdigit()))
            .ok_or_else(|| {
                err(
                    source,
                    at,
                    &format!(
                        "expected {width} hexadecimal digits after '\\{}'",
                        *escaped as char
                    ),
                )
            })?;
        let point = u32::from_str_radix(digits, 16).expect("hexadecimal digits");
        let ch =
            char::from_u32(point).ok_or_else(|| err(source, at, "escape names no character"))?;
        text.push(ch);
        return Ok(end);
    }
    let ch = match escaped {
        b'\\' => '\\',
        b'\'' => '\'',
        b'"' => '"',
        b'`' => '`',
        b'n' => '\n',
        b'r' => '\r',
        b't' => '\t',
        b'b' => '\u{8}',
        b'f' => '\u{c}',
        _ => return Err(err(source, at, "unknown escape in string")),
    };
    text.push(ch);
    Ok(at + 2)
}

/// Lexes a quoted sequence starting at `open`, which is a string when
/// the quote is `'` or `"` and an identifier when it is a grave accent.
/// All three are the same shape in ISO 21.3, so they are one function:
/// the quote may be written twice to mean itself, and `escapes` is off
/// for the `@` form of GL11, where a backslash is a backslash and
/// doubling the quote is the only way one gets in.
///
/// `report` is where a failure points, which is the `@` rather than the
/// quote when there is one, since that is where the literal began.
fn lex_quoted(source: &str, open: usize, escapes: bool, report: usize) -> Result<(String, usize)> {
    let bytes = source.as_bytes();
    let quote = bytes[open];
    let mut text = String::new();
    let mut ix = open + 1;
    while ix < bytes.len() {
        if bytes[ix] == quote {
            if bytes.get(ix + 1) == Some(&quote) {
                text.push(quote as char);
                ix += 2;
                continue;
            }
            return Ok((text, ix + 1));
        }
        if escapes && bytes[ix] == b'\\' {
            ix = escape(source, ix, &mut text)?;
            continue;
        }
        let ch = source[ix..].chars().next().expect("in-bounds char");
        text.push(ch);
        ix += ch.len_utf8();
    }
    let noun = if quote == b'`' {
        "identifier"
    } else {
        "string"
    };
    Err(err(source, report, &format!("unterminated {noun}")))
}

/// The radix a `0x`, `0o` or `0b` prefix asks for, and the word for it
/// so a failure can say which digits it wanted.
fn radix_of(marker: u8) -> Option<(u32, &'static str)> {
    match marker {
        b'x' | b'X' => Some((16, "hexadecimal")),
        b'o' | b'O' => Some((8, "octal")),
        b'b' | b'B' => Some((2, "binary")),
        _ => None,
    }
}

/// Lexes GL01 to GL03: an integer written in a radix other than ten.
/// The prefix decides the digits, so `0b19` is not a number followed by
/// a nine, it is one word that is not a number.
fn lex_radix_integer(
    source: &str,
    start: usize,
    (radix, name): (u32, &str),
) -> Result<(TokenKind, usize)> {
    let bytes = source.as_bytes();
    let first = start + 2;
    let mut ix = first;
    // ISO 21.3 writes the digits of a radix integer as `{ [_] digit }`,
    // so a separator may stand in front of the first digit here where a
    // decimal integer has to begin with one. Two in a row is still two
    // separators with no digit between them.
    while ix < bytes.len() {
        if bytes[ix] == b'_' && (bytes.get(ix + 1)).is_some_and(|&c| (c as char).is_digit(radix)) {
            ix += 2;
            continue;
        }
        if (bytes[ix] as char).is_digit(radix) {
            ix += 1;
            continue;
        }
        break;
    }
    let trailing = bytes
        .get(ix)
        .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_');
    if ix == first || trailing {
        let prefix = &source[start..first];
        return Err(err(
            source,
            start,
            &format!("expected {name} digits after '{prefix}'"),
        ));
    }
    let digits: String = source[first..ix].chars().filter(|&c| c != '_').collect();
    let value = u64::from_str_radix(&digits, radix)
        .map_err(|_| err(source, start, "integer literal out of range"))?;
    Ok((TokenKind::Int(value), ix))
}

/// Runs of decimal digits from `from`, which is a digit, answering the
/// index after the last of them. A separator is allowed between two
/// digits and nowhere else, which is what makes `1_000` a thousand and
/// `1_` the number one followed by a name: ISO 21.3 writes the run as
/// `digit { [_] digit }`, so the underscore never ends the number and
/// never doubles.
fn digits(bytes: &[u8], from: usize) -> usize {
    let mut ix = from;
    while ix < bytes.len() {
        if bytes[ix].is_ascii_digit() {
            ix += 1;
            continue;
        }
        if bytes[ix] == b'_' && bytes.get(ix + 1).is_some_and(u8::is_ascii_digit) {
            ix += 2;
            continue;
        }
        break;
    }
    ix
}

/// Lexes an integer or float. `1..3` stays an integer before a range,
/// `1.5` and `1.5e3` and `2e8` are floats, and `1.` without a digit is
/// an error rather than a silent float. A `0x`, `0o` or `0b` prefix
/// changes the radix, and an `M`, `F` or `D` suffix says which kind of
/// number the text means.
fn lex_number(source: &str, start: usize) -> Result<(TokenKind, usize)> {
    let bytes = source.as_bytes();
    if bytes[start] == b'0'
        && let Some(radix) = bytes.get(start + 1).copied().and_then(radix_of)
    {
        return lex_radix_integer(source, start, radix);
    }
    let mut ix = digits(bytes, start);
    let mut is_float = false;
    if ix < bytes.len() && bytes[ix] == b'.' && bytes.get(ix + 1) != Some(&b'.') {
        if !bytes.get(ix + 1).is_some_and(u8::is_ascii_digit) {
            return Err(err(source, ix, "expected digits after the decimal point"));
        }
        is_float = true;
        ix = digits(bytes, ix + 1);
    }
    if ix < bytes.len() && (bytes[ix] == b'e' || bytes[ix] == b'E') {
        let mut ex = ix + 1;
        if ex < bytes.len() && (bytes[ex] == b'+' || bytes[ex] == b'-') {
            ex += 1;
        }
        if bytes.get(ex).is_some_and(u8::is_ascii_digit) {
            is_float = true;
            ix = digits(bytes, ex);
        }
    }
    let written = &source[start..ix];
    let stripped: String;
    let text = if written.contains('_') {
        stripped = written.chars().filter(|&c| c != '_').collect();
        stripped.as_str()
    } else {
        written
    };
    // GL05 to GL10. A number may say which kind it is: `M` for an exact
    // number, `F` and `D` for an approximate one. The suffix is one
    // letter and nothing may follow it, or `1Fx` would be the number one
    // and a name called Fx read as a float and a name called x.
    if let Some(&marker) = bytes.get(ix)
        && matches!(marker, b'M' | b'm' | b'F' | b'f' | b'D' | b'd')
        && !bytes
            .get(ix + 1)
            .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_')
    {
        ix += 1;
        // zu holds an exact number as the integer it is when it has no
        // fraction, which is what `M` asks for. It has no decimal type,
        // so a fraction written exactly is widened to a double and the
        // scale the suffix declares is not kept.
        is_float |= marker.eq_ignore_ascii_case(&b'F') || marker.eq_ignore_ascii_case(&b'D');
    }
    if is_float {
        let value: f64 = text
            .parse()
            .map_err(|_| err(source, start, "float literal out of range"))?;
        if !value.is_finite() {
            return Err(err(source, start, "float literal out of range"));
        }
        Ok((TokenKind::Float(value), ix))
    } else {
        let value: u64 = text
            .parse()
            .map_err(|_| err(source, start, "integer literal out of range"))?;
        Ok((TokenKind::Int(value), ix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(source: &str) -> Vec<TokenKind> {
        lex(source)
            .expect("lex")
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn keywords_are_plain_identifiers() {
        assert_eq!(
            kinds("MATCH (n) RETURN n"),
            vec![
                TokenKind::Ident("MATCH".into()),
                TokenKind::LParen,
                TokenKind::Ident("n".into()),
                TokenKind::RParen,
                TokenKind::Ident("RETURN".into()),
                TokenKind::Ident("n".into()),
            ]
        );
    }

    #[test]
    fn less_than_negative_is_not_an_arrow() {
        assert_eq!(
            kinds("a < -1"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::Lt,
                TokenKind::Minus,
                TokenKind::Int(1),
            ]
        );
    }

    #[test]
    fn numbers_split_from_ranges() {
        assert_eq!(
            kinds("*1..3 1.5 2e8 1.5e-3"),
            vec![
                TokenKind::Star,
                TokenKind::Int(1),
                TokenKind::DotDot,
                TokenKind::Int(3),
                TokenKind::Float(1.5),
                TokenKind::Float(2e8),
                TokenKind::Float(1.5e-3),
            ]
        );
    }

    /// GL01, GL02, GL03. The prefix picks the digits, and the digits it
    /// did not pick end the number rather than continuing it.
    #[test]
    fn integers_come_in_four_radixes() {
        assert_eq!(
            kinds("0xFF 0o17 0b1010 0X10 0B1 10"),
            vec![
                TokenKind::Int(255),
                TokenKind::Int(15),
                TokenKind::Int(10),
                TokenKind::Int(16),
                TokenKind::Int(1),
                TokenKind::Int(10),
            ]
        );
        // Zero is still zero, and a range after one still splits.
        assert_eq!(
            kinds("0 0.5 0..3"),
            vec![
                TokenKind::Int(0),
                TokenKind::Float(0.5),
                TokenKind::Int(0),
                TokenKind::DotDot,
                TokenKind::Int(3),
            ]
        );
        for bad in ["0b19", "0xFG", "0o8", "0x"] {
            let e = lex(bad).expect_err(bad);
            assert!(e.to_string().contains("digits after"), "{bad}: {e}");
        }
    }

    /// GL05 to GL10. The suffix says which kind of number the text
    /// means, and an exact number with no fraction stays an integer.
    #[test]
    fn numbers_may_name_their_own_kind() {
        assert_eq!(
            kinds("1.25M 1.5E2M 2.5F 1.5E2F 1.5D 7M 7F"),
            vec![
                TokenKind::Float(1.25),
                TokenKind::Float(150.0),
                TokenKind::Float(2.5),
                TokenKind::Float(150.0),
                TokenKind::Float(1.5),
                TokenKind::Int(7),
                TokenKind::Float(7.0),
            ]
        );
        // A longer name after the digits is a name and not a suffix.
        assert_eq!(
            kinds("1Fx"),
            vec![TokenKind::Int(1), TokenKind::Ident("Fx".into())]
        );
    }

    /// GL11. The `@` form hands back the characters that were written,
    /// which is the only way to get a backslash through.
    #[test]
    fn an_at_sign_turns_off_escapes() {
        assert_eq!(
            kinds(r#"@'a\nb' @"c\td" @'it''s'"#),
            vec![
                TokenKind::Str(r"a\nb".into()),
                TokenKind::Str(r"c\td".into()),
                TokenKind::Str("it's".into()),
            ]
        );
        // Without the quote it is still nothing, and an unterminated one
        // reports the `@` rather than the quote.
        let e = lex("RETURN @x").expect_err("bare at sign");
        assert!(e.to_string().contains("unexpected character '@'"), "{e}");
        let e = lex("@'open").expect_err("unterminated");
        assert!(e.to_string().contains("column 1"), "{e}");
    }

    #[test]
    fn strings_escape_and_quote_both_ways() {
        assert_eq!(
            kinds(r#"'it\'s' "a\nb" '図'"#),
            vec![
                TokenKind::Str("it's".into()),
                TokenKind::Str("a\nb".into()),
                TokenKind::Str("図".into()),
            ]
        );
    }

    /// The separator ISO 21.3 puts between digits, which is mandatory
    /// and reads in every radix.
    #[test]
    fn digits_may_be_grouped() {
        assert_eq!(
            kinds("1_000_000 1_0.5_0 1_0e1_0 0xF_F 0b1_0 0o1_7 0x_FF 1_000M"),
            vec![
                TokenKind::Int(1_000_000),
                TokenKind::Float(10.50),
                TokenKind::Float(1.0e11),
                TokenKind::Int(255),
                TokenKind::Int(2),
                TokenKind::Int(15),
                TokenKind::Int(255),
                TokenKind::Int(1000),
            ]
        );
        // A separator lives between two digits, so one with a name
        // behind it ends the number and begins the name.
        assert_eq!(
            kinds("1_x 1_"),
            vec![
                TokenKind::Int(1),
                TokenKind::Ident("_x".into()),
                TokenKind::Int(1),
                TokenKind::Ident("_".into()),
            ]
        );
        for bad in ["0x__F", "0b_", "0o_9"] {
            let e = lex(bad).expect_err(bad);
            assert!(e.to_string().contains("digits after"), "{bad}: {e}");
        }
    }

    /// GB02 and GB03, and the bracketed form that is mandatory. Two
    /// minus signs are a comment wherever they are written, which is
    /// why a subtraction of a negative number needs its spaces.
    #[test]
    fn comments_vanish() {
        assert_eq!(
            kinds("1 // line\n/* block\n */ 2 -- dash\n3"),
            vec![TokenKind::Int(1), TokenKind::Int(2), TokenKind::Int(3)]
        );
        assert_eq!(
            kinds("1 - -2"),
            vec![
                TokenKind::Int(1),
                TokenKind::Minus,
                TokenKind::Minus,
                TokenKind::Int(2)
            ]
        );
        assert_eq!(kinds("1--2"), vec![TokenKind::Int(1)]);
    }

    /// A delimited identifier is a quoted sequence like a string is,
    /// down to the doubling and the escapes (ISO 21.3), and the `@`
    /// form turns the escapes off for either of them.
    #[test]
    fn identifiers_quote_like_strings_do() {
        assert_eq!(
            kinds(r#"`odd``name` `a\tb` @`raw\name` `it's`"#),
            vec![
                TokenKind::QuotedIdent("odd`name".into()),
                TokenKind::QuotedIdent("a\tb".into()),
                TokenKind::QuotedIdent(r"raw\name".into()),
                TokenKind::QuotedIdent("it's".into()),
            ]
        );
        let e = lex("RETURN ``").expect_err("empty");
        assert!(e.to_string().contains("empty backtick"), "{e}");
        let e = lex("RETURN `open").expect_err("unterminated");
        assert!(e.to_string().contains("unterminated identifier"), "{e}");
    }

    /// The rest of ISO's escape set, and the two ways to name a
    /// character by its code point.
    #[test]
    fn strings_take_the_whole_escape_set() {
        assert_eq!(
            kinds(r#"'a\bb' 'a\fb' 'a\`b' 'it''s' "say ""hi""" 'A\U01F600'"#),
            vec![
                TokenKind::Str("a\u{8}b".into()),
                TokenKind::Str("a\u{c}b".into()),
                TokenKind::Str("a`b".into()),
                TokenKind::Str("it's".into()),
                TokenKind::Str("say \"hi\"".into()),
                TokenKind::Str("A\u{1F600}".into()),
            ]
        );
        for (bad, want) in [
            (r"'\uZZZZ'", "hexadecimal digits"),
            (r"'\u00'", "hexadecimal digits"),
            (r"'\UD800AA'", "no character"),
            (r"'\q'", "unknown escape"),
        ] {
            let e = lex(bad).expect_err(bad);
            assert!(e.to_string().contains(want), "{bad}: {e}");
        }
    }

    #[test]
    fn errors_carry_line_and_column() {
        let e = lex("RETURN\n  'open").expect_err("unterminated");
        assert!(e.to_string().contains("line 2, column 3"), "got: {e}");
        let e = lex("RETURN 99999999999999999999999").expect_err("overflow");
        assert!(e.to_string().contains("out of range"), "got: {e}");
        let e = lex("1.e3").expect_err("bare point");
        assert!(e.to_string().contains("decimal point"), "got: {e}");
    }

    #[test]
    fn params_and_quoted_identifiers() {
        assert_eq!(
            kinds("$id $0 `odd name`"),
            vec![
                TokenKind::Param("id".into()),
                TokenKind::Param("0".into()),
                TokenKind::QuotedIdent("odd name".into()),
            ]
        );
    }

    /// ISO 21.10's two parameter spellings, the general one and the
    /// substituted one a reference is written with. They name the same
    /// parameter, so the name is what comes out either way, and a
    /// dollar sign with nothing behind it is still an error.
    #[test]
    fn a_substituted_parameter_names_the_parameter_the_general_one_names() {
        assert_eq!(
            kinds("$g $$g $$long_name"),
            vec![
                TokenKind::Param("g".into()),
                TokenKind::Param("g".into()),
                TokenKind::Param("long_name".into()),
            ]
        );
        let e = lex("RETURN $$ AS n").expect_err("no name");
        assert!(e.to_string().contains("parameter name"), "got: {e}");
    }
}
