//! The text a staged list column holds: a JSON array of scalars.
//!
//! A list has to cross sqlite to reach a zu1 file, and sqlite has no
//! list. A JSON array in a text column is the form that crossing takes,
//! because it is the one every writer of a staging file already has a
//! library for, and the element type is not read out of it: the column's
//! declaration says what the elements are and this module is told, so a
//! `[1, 2]` under a REALLIST declaration loads as two doubles rather
//! than guessing from the spelling.
//!
//! The reader is deliberately small. It accepts an array of the one
//! scalar kind it was asked for and nothing else, so no nesting, no
//! objects and no mixed arrays, which is the whole of what a list
//! property column can hold anyway.

use zu_common::{Result, ZuError};

/// One element of a staged list, in the kind its column was declared
/// with.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Int(i64),
    Real(f64),
    Text(String),
    Bool(bool),
}

/// The element kind a list column holds, which its declared type names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Real,
    Text,
    Bool,
}

/// Reads a JSON array of `kind` elements.
pub fn parse(kind: Kind, text: &str) -> Result<Vec<Item>> {
    let mut p = Reader {
        bytes: text.as_bytes(),
        at: 0,
        kind,
    };
    p.space();
    p.want(b'[')?;
    let mut out = Vec::new();
    p.space();
    if p.peek() == Some(b']') {
        p.at += 1;
        p.space();
        return p.end(out);
    }
    loop {
        p.space();
        out.push(p.item()?);
        p.space();
        match p.peek() {
            Some(b',') => p.at += 1,
            Some(b']') => {
                p.at += 1;
                break;
            }
            _ => return Err(p.bad("a comma or a closing bracket")),
        }
    }
    p.space();
    p.end(out)
}

/// Writes elements back as a JSON array, the inverse of [`parse`].
pub fn write(items: &[Item]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            Item::Int(v) => out.push_str(&v.to_string()),
            // A JSON number has no infinity and no NaN, so a float that
            // is neither would be written as something no reader takes
            // back. They cannot reach here from a lane read of a stored
            // list, and if one ever does it is better as a null the
            // loader refuses than as a token nothing parses.
            Item::Real(v) if !v.is_finite() => out.push_str("null"),
            Item::Real(v) => out.push_str(&format!("{v:?}")),
            Item::Bool(v) => out.push_str(if *v { "true" } else { "false" }),
            Item::Text(v) => write_string(v, &mut out),
        }
    }
    out.push(']');
    out
}

/// A JSON string: the two escapes JSON requires, the control characters
/// as `\u`, and everything else as itself.
fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    kind: Kind,
}

impl Reader<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn want(&mut self, byte: u8) -> Result<()> {
        if self.peek() == Some(byte) {
            self.at += 1;
            return Ok(());
        }
        Err(self.bad(&format!("'{}'", byte as char)))
    }

    fn end(&self, out: Vec<Item>) -> Result<Vec<Item>> {
        if self.at != self.bytes.len() {
            return Err(self.bad("the end of the array"));
        }
        Ok(out)
    }

    fn bad(&self, wanted: &str) -> ZuError {
        ZuError::InvalidArgument(format!(
            "a staged list holds a JSON array; expected {wanted} at byte {} of {}",
            self.at,
            String::from_utf8_lossy(self.bytes)
        ))
    }

    fn item(&mut self) -> Result<Item> {
        match self.kind {
            Kind::Text => Ok(Item::Text(self.string()?)),
            Kind::Bool => {
                for (word, value) in [("true", true), ("false", false)] {
                    if self.bytes[self.at..].starts_with(word.as_bytes()) {
                        self.at += word.len();
                        return Ok(Item::Bool(value));
                    }
                }
                Err(self.bad("true or false"))
            }
            Kind::Int | Kind::Real => {
                let start = self.at;
                if matches!(self.peek(), Some(b'-' | b'+')) {
                    self.at += 1;
                }
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()
                    || matches!(c, b'.' | b'e' | b'E' | b'-' | b'+'))
                {
                    self.at += 1;
                }
                let text = std::str::from_utf8(&self.bytes[start..self.at]).expect("utf-8 in");
                if self.kind == Kind::Int {
                    return text
                        .parse::<i64>()
                        .map(Item::Int)
                        .map_err(|_| self.bad("an integer"));
                }
                text.parse::<f64>()
                    .map(Item::Real)
                    .map_err(|_| self.bad("a number"))
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        self.want(b'"')?;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.bad("a closing quote")),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    let escape = self.peek().ok_or_else(|| self.bad("an escape"))?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hex = self
                                .bytes
                                .get(self.at..self.at + 4)
                                .ok_or_else(|| self.bad("four hex digits"))?;
                            let hex = std::str::from_utf8(hex).map_err(|_| self.bad("hex"))?;
                            let code = u32::from_str_radix(hex, 16).map_err(|_| self.bad("hex"))?;
                            // A lone surrogate is not a character, and a
                            // pair of them is a spelling this reader does
                            // not need to accept, because it reads what
                            // it wrote and it never writes one.
                            out.push(char::from_u32(code).ok_or_else(|| self.bad("a character"))?);
                            self.at += 4;
                        }
                        other => return Err(self.bad(&format!("an escape, not '{other}'"))),
                    }
                }
                Some(_) => {
                    // The rest of this run of plain bytes at once, so a
                    // long string is one copy rather than one per char.
                    let start = self.at;
                    while matches!(self.peek(), Some(c) if c != b'"' && c != b'\\') {
                        self.at += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.bytes[start..self.at]).map_err(
                        |_| {
                            // The input came in as a &str, so a split
                            // here means the scan stopped mid character,
                            // which the two stop bytes cannot do.
                            self.bad("valid text")
                        },
                    )?);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_array_of_each_kind_round_trips() {
        for (kind, items) in [
            (Kind::Int, vec![Item::Int(1), Item::Int(-2), Item::Int(0)]),
            (Kind::Real, vec![Item::Real(1.5), Item::Real(-0.25)]),
            (Kind::Bool, vec![Item::Bool(true), Item::Bool(false)]),
            (
                Kind::Text,
                vec![
                    Item::Text("a".into()),
                    Item::Text("say \"hi\"\n".into()),
                    Item::Text("back\\slash".into()),
                    Item::Text("héllo".into()),
                ],
            ),
        ] {
            let text = write(&items);
            assert_eq!(parse(kind, &text).unwrap(), items, "{text}");
        }
    }

    #[test]
    fn whitespace_and_the_empty_array_are_read() {
        assert_eq!(parse(Kind::Int, "[]").unwrap(), Vec::new());
        assert_eq!(parse(Kind::Int, "  [ ]  ").unwrap(), Vec::new());
        assert_eq!(
            parse(Kind::Int, " [ 1 ,\n2 ] ").unwrap(),
            vec![Item::Int(1), Item::Int(2)]
        );
        assert_eq!(write(&[]), "[]");
    }

    #[test]
    fn what_a_list_column_cannot_hold_is_refused_rather_than_guessed() {
        for (kind, text) in [
            (Kind::Int, "[1, 2"),
            (Kind::Int, "1, 2]"),
            (Kind::Int, "[[1], [2]]"),
            (Kind::Int, "[1.5]"),
            (Kind::Int, "[\"a\"]"),
            (Kind::Int, "[1] [2]"),
            (Kind::Int, "[1,]"),
            (Kind::Text, "[a]"),
            (Kind::Text, "[\"a]"),
            (Kind::Bool, "[yes]"),
            (Kind::Real, "[]]"),
        ] {
            assert!(parse(kind, text).is_err(), "{text} was accepted");
        }
    }

    /// A float written by `write` has to read back bit for bit, because
    /// the staging file is on the path a zu1 file round trips through.
    #[test]
    fn a_float_survives_the_text_it_is_written_as() {
        let items: Vec<Item> = [0.1, 1.0 / 3.0, f64::MAX, f64::MIN_POSITIVE, -0.0]
            .iter()
            .map(|&f| Item::Real(f))
            .collect();
        let back = parse(Kind::Real, &write(&items)).unwrap();
        for (a, b) in items.iter().zip(&back) {
            let (Item::Real(a), Item::Real(b)) = (a, b) else {
                unreachable!()
            };
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }
}
