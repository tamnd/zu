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

/// Lexes an integer or float. `1..3` stays an integer before a range,
/// `1.5` and `1.5e3` and `2e8` are floats, and `1.` without a digit is
/// an error rather than a silent float.
fn lex_number(source: &str, start: usize) -> Result<(TokenKind, usize)> {
    let bytes = source.as_bytes();
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
