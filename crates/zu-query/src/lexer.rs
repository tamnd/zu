//! Hand-written lexer for zuQL (docs/07 §1, grammar in
//! docs/grammar.ebnf).
//!
//! Tokens carry their byte span so the parser can report positions as
//! line and column. Keywords are not lexed specially: they are plain
//! identifiers the parser matches case-insensitively, which keeps the
//! token set small and the keyword list in one place. `<-` and `->` are
//! deliberately not tokens: `a < -1` must lex as less-than then minus,
//! so the parser assembles pattern arrows from the single characters.

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
        }
    }
}

/// Renders a byte offset as `line L, column C`, both 1-based, counting
/// columns in characters so multi-byte text does not skew them.
pub fn position(source: &str, offset: usize) -> String {
    let mut line = 1usize;
    let mut col = 1usize;
    for (ix, ch) in source.char_indices() {
        if ix >= offset {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    format!("line {line}, column {col}")
}

/// Every failure the lexer can produce is a `42001 invalid syntax`:
/// the text is not GQL. Anything richer than that is the parser's job.
fn err(source: &str, offset: usize, detail: &str) -> ZuError {
    ZuError::gql(
        codes::C42001,
        format!("{}: {detail}", position(source, offset)),
    )
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
        if b == b'/' && bytes.get(ix + 1) == Some(&b'/') {
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
            b'*' => tokens.push(single(TokenKind::Star)),
            b'/' => tokens.push(single(TokenKind::Slash)),
            b'%' => tokens.push(single(TokenKind::Percent)),
            b'^' => tokens.push(single(TokenKind::Caret)),
            b'=' => tokens.push(single(TokenKind::Eq)),
            b'|' => tokens.push(single(TokenKind::Pipe)),
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
            b'\'' | b'"' => {
                let (text, end) = lex_string(source, ix)?;
                tokens.push(Token {
                    kind: TokenKind::Str(text),
                    start,
                    end,
                });
                ix = end;
                continue;
            }
            // GL11. `@` before a quote asks for the text as written, so
            // a lone `@` is still nothing at all.
            b'@' if matches!(bytes.get(ix + 1), Some(b'\'' | b'"')) => {
                let (text, end) = lex_raw_string(source, ix + 1)?;
                tokens.push(Token {
                    kind: TokenKind::Str(text),
                    start,
                    end,
                });
                ix = end;
                continue;
            }
            b'`' => {
                let close = bytes[ix + 1..]
                    .iter()
                    .position(|&c| c == b'`')
                    .ok_or_else(|| err(source, ix, "unterminated backtick identifier"))?;
                let name = &source[ix + 1..ix + 1 + close];
                if name.is_empty() {
                    return Err(err(source, ix, "empty backtick identifier"));
                }
                tokens.push(Token {
                    kind: TokenKind::QuotedIdent(name.to_string()),
                    start,
                    end: ix + close + 2,
                });
                ix += close + 2;
                continue;
            }
            b'$' => {
                let mut end = ix + 1;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                if end == ix + 1 {
                    return Err(err(source, ix, "expected a parameter name after '$'"));
                }
                tokens.push(Token {
                    kind: TokenKind::Param(source[ix + 1..end].to_string()),
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

/// Lexes a quoted string starting at `open`, handling the escape set
/// `\\ \' \" \n \r \t` and rejecting everything else by position.
fn lex_string(source: &str, open: usize) -> Result<(String, usize)> {
    let bytes = source.as_bytes();
    let quote = bytes[open];
    let mut text = String::new();
    let mut ix = open + 1;
    while ix < bytes.len() {
        match bytes[ix] {
            b'\\' => {
                let escaped = bytes
                    .get(ix + 1)
                    .ok_or_else(|| err(source, open, "unterminated string"))?;
                let ch = match escaped {
                    b'\\' => '\\',
                    b'\'' => '\'',
                    b'"' => '"',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    _ => {
                        return Err(err(source, ix, "unknown escape in string"));
                    }
                };
                text.push(ch);
                ix += 2;
            }
            c if c == quote => return Ok((text, ix + 1)),
            _ => {
                let ch = source[ix..].chars().next().expect("in-bounds char");
                text.push(ch);
                ix += ch.len_utf8();
            }
        }
    }
    Err(err(source, open, "unterminated string"))
}

/// Lexes an `@'...'` string, ISO's no-escape form (GL11, 21.3). A
/// backslash is a backslash, which is the point of the form, so the
/// only way a quote gets in is by writing it twice.
fn lex_raw_string(source: &str, open: usize) -> Result<(String, usize)> {
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
        let ch = source[ix..].chars().next().expect("in-bounds char");
        text.push(ch);
        ix += ch.len_utf8();
    }
    Err(err(source, open - 1, "unterminated string"))
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
    while ix < bytes.len() && (bytes[ix] as char).is_digit(radix) {
        ix += 1;
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
    let value = u64::from_str_radix(&source[first..ix], radix)
        .map_err(|_| err(source, start, "integer literal out of range"))?;
    Ok((TokenKind::Int(value), ix))
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
    let mut ix = start;
    while ix < bytes.len() && bytes[ix].is_ascii_digit() {
        ix += 1;
    }
    let mut is_float = false;
    if ix < bytes.len() && bytes[ix] == b'.' && bytes.get(ix + 1) != Some(&b'.') {
        if !bytes.get(ix + 1).is_some_and(u8::is_ascii_digit) {
            return Err(err(source, ix, "expected digits after the decimal point"));
        }
        is_float = true;
        ix += 1;
        while ix < bytes.len() && bytes[ix].is_ascii_digit() {
            ix += 1;
        }
    }
    if ix < bytes.len() && (bytes[ix] == b'e' || bytes[ix] == b'E') {
        let mut ex = ix + 1;
        if ex < bytes.len() && (bytes[ex] == b'+' || bytes[ex] == b'-') {
            ex += 1;
        }
        if bytes.get(ex).is_some_and(u8::is_ascii_digit) {
            is_float = true;
            ix = ex;
            while ix < bytes.len() && bytes[ix].is_ascii_digit() {
                ix += 1;
            }
        }
    }
    let text = &source[start..ix];
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

    #[test]
    fn comments_vanish() {
        assert_eq!(
            kinds("1 // line\n/* block\n */ 2"),
            vec![TokenKind::Int(1), TokenKind::Int(2)]
        );
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
}
