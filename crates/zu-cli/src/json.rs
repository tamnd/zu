//! A small JSON reader for the shell's request frames. The CLI carries
//! no JSON crate (G7 caps the binary at 15 MiB), and the frames it has
//! to read are one-line objects with string, number, bool, and flat
//! object values, so a few hundred lines here beat a dependency.

/// A parsed JSON value. Numbers keep their integer identity when the
/// text has no fraction or exponent, because parameter binding treats
/// ints and floats differently and `7` must not arrive as `7.0`.
#[derive(Debug, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// Looks a key up in an object; `None` for a missing key or a
    /// non-object.
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        }
    }
}

/// Parses one complete JSON value and rejects trailing input, so a
/// frame with junk after the closing brace is an error the sender
/// hears about instead of a silently truncated read.
pub(crate) fn parse(text: &str) -> Result<Json, String> {
    let bytes = text.as_bytes();
    let mut pos = 0;
    let value = parse_value(bytes, &mut pos)?;
    skip_ws(bytes, &mut pos);
    if pos != bytes.len() {
        return Err(format!("trailing input at byte {pos}"));
    }
    Ok(value)
}

fn skip_ws(bytes: &[u8], pos: &mut usize) {
    while let Some(b' ' | b'\t' | b'\n' | b'\r') = bytes.get(*pos) {
        *pos += 1;
    }
}

fn parse_value(bytes: &[u8], pos: &mut usize) -> Result<Json, String> {
    skip_ws(bytes, pos);
    match bytes.get(*pos) {
        None => Err("unexpected end of input".to_string()),
        Some(b'{') => parse_obj(bytes, pos),
        Some(b'[') => parse_arr(bytes, pos),
        Some(b'"') => Ok(Json::Str(parse_str(bytes, pos)?)),
        Some(b't') => parse_lit(bytes, pos, "true", Json::Bool(true)),
        Some(b'f') => parse_lit(bytes, pos, "false", Json::Bool(false)),
        Some(b'n') => parse_lit(bytes, pos, "null", Json::Null),
        Some(b'-' | b'0'..=b'9') => parse_num(bytes, pos),
        Some(c) => Err(format!("unexpected byte {c:#04x} at {pos}", pos = *pos)),
    }
}

fn parse_lit(bytes: &[u8], pos: &mut usize, word: &str, value: Json) -> Result<Json, String> {
    if bytes[*pos..].starts_with(word.as_bytes()) {
        *pos += word.len();
        Ok(value)
    } else {
        Err(format!("bad literal at byte {pos}", pos = *pos))
    }
}

fn parse_num(bytes: &[u8], pos: &mut usize) -> Result<Json, String> {
    let start = *pos;
    if bytes.get(*pos) == Some(&b'-') {
        *pos += 1;
    }
    let mut fractional = false;
    while let Some(c) = bytes.get(*pos) {
        match c {
            b'0'..=b'9' => *pos += 1,
            b'.' | b'e' | b'E' | b'+' | b'-' => {
                fractional = true;
                *pos += 1;
            }
            _ => break,
        }
    }
    let text = std::str::from_utf8(&bytes[start..*pos]).map_err(|_| "bad number".to_string())?;
    if !fractional
        && let Ok(i) = text.parse::<i64>()
    {
        return Ok(Json::Int(i));
    }
    text.parse::<f64>()
        .map(Json::Float)
        .map_err(|_| format!("bad number {text:?}"))
}

fn parse_str(bytes: &[u8], pos: &mut usize) -> Result<String, String> {
    debug_assert_eq!(bytes.get(*pos), Some(&b'"'));
    *pos += 1;
    let mut out = String::new();
    loop {
        match bytes.get(*pos) {
            None => return Err("unterminated string".to_string()),
            Some(b'"') => {
                *pos += 1;
                return Ok(out);
            }
            Some(b'\\') => {
                *pos += 1;
                match bytes.get(*pos) {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'/') => out.push('/'),
                    Some(b'n') => out.push('\n'),
                    Some(b't') => out.push('\t'),
                    Some(b'r') => out.push('\r'),
                    Some(b'b') => out.push('\u{8}'),
                    Some(b'f') => out.push('\u{c}'),
                    Some(b'u') => {
                        let hi = parse_hex4(bytes, *pos + 1)?;
                        *pos += 4;
                        // A high surrogate must pair with a \uXXXX low
                        // surrogate right behind it; anything else is
                        // not a scalar value and cannot become a char.
                        let code = if (0xD800..0xDC00).contains(&hi) {
                            if bytes.get(*pos + 1..*pos + 3) != Some(b"\\u") {
                                return Err("lone high surrogate".to_string());
                            }
                            let lo = parse_hex4(bytes, *pos + 3)?;
                            *pos += 6;
                            if !(0xDC00..0xE000).contains(&lo) {
                                return Err("bad low surrogate".to_string());
                            }
                            0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                        } else {
                            hi
                        };
                        match char::from_u32(code) {
                            Some(c) => out.push(c),
                            None => return Err(format!("bad escape \\u{code:04x}")),
                        }
                    }
                    _ => return Err(format!("bad escape at byte {pos}", pos = *pos)),
                }
                *pos += 1;
            }
            Some(_) => {
                // Step over one UTF-8 scalar, however many bytes wide.
                let rest = std::str::from_utf8(&bytes[*pos..])
                    .map_err(|_| "invalid utf-8 in string".to_string())?;
                let c = rest.chars().next().expect("non-empty");
                out.push(c);
                *pos += c.len_utf8();
            }
        }
    }
}

fn parse_hex4(bytes: &[u8], at: usize) -> Result<u32, String> {
    let hex = bytes
        .get(at..at + 4)
        .and_then(|b| std::str::from_utf8(b).ok())
        .ok_or_else(|| "short \\u escape".to_string())?;
    u32::from_str_radix(hex, 16).map_err(|_| format!("bad \\u escape {hex:?}"))
}

fn parse_arr(bytes: &[u8], pos: &mut usize) -> Result<Json, String> {
    *pos += 1;
    let mut items = Vec::new();
    skip_ws(bytes, pos);
    if bytes.get(*pos) == Some(&b']') {
        *pos += 1;
        return Ok(Json::Arr(items));
    }
    loop {
        items.push(parse_value(bytes, pos)?);
        skip_ws(bytes, pos);
        match bytes.get(*pos) {
            Some(b',') => *pos += 1,
            Some(b']') => {
                *pos += 1;
                return Ok(Json::Arr(items));
            }
            _ => return Err(format!("expected ',' or ']' at byte {pos}", pos = *pos)),
        }
    }
}

fn parse_obj(bytes: &[u8], pos: &mut usize) -> Result<Json, String> {
    *pos += 1;
    let mut fields = Vec::new();
    skip_ws(bytes, pos);
    if bytes.get(*pos) == Some(&b'}') {
        *pos += 1;
        return Ok(Json::Obj(fields));
    }
    loop {
        skip_ws(bytes, pos);
        if bytes.get(*pos) != Some(&b'"') {
            return Err(format!("expected key at byte {pos}", pos = *pos));
        }
        let key = parse_str(bytes, pos)?;
        skip_ws(bytes, pos);
        if bytes.get(*pos) != Some(&b':') {
            return Err(format!("expected ':' at byte {pos}", pos = *pos));
        }
        *pos += 1;
        fields.push((key, parse_value(bytes, pos)?));
        skip_ws(bytes, pos);
        match bytes.get(*pos) {
            Some(b',') => *pos += 1,
            Some(b'}') => {
                *pos += 1;
                return Ok(Json::Obj(fields));
            }
            _ => return Err(format!("expected ',' or '}}' at byte {pos}", pos = *pos)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_parse_with_their_types_intact() {
        let v = parse(r#"{"op":"execute","stmt":3,"params":{"src":7,"w":0.5,"name":"Ada"}}"#)
            .expect("frame");
        assert_eq!(v.get("op").and_then(Json::as_str), Some("execute"));
        assert_eq!(v.get("stmt").and_then(Json::as_u64), Some(3));
        let params = v.get("params").expect("params");
        assert_eq!(params.get("src"), Some(&Json::Int(7)));
        assert_eq!(params.get("w"), Some(&Json::Float(0.5)));
        assert_eq!(params.get("name"), Some(&Json::Str("Ada".into())));
    }

    #[test]
    fn scalars_arrays_and_nesting() {
        assert_eq!(parse("null"), Ok(Json::Null));
        assert_eq!(parse(" true "), Ok(Json::Bool(true)));
        assert_eq!(parse("-42"), Ok(Json::Int(-42)));
        assert_eq!(parse("1e3"), Ok(Json::Float(1000.0)));
        assert_eq!(
            parse(r#"[1,[2,"x"],{}]"#),
            Ok(Json::Arr(vec![
                Json::Int(1),
                Json::Arr(vec![Json::Int(2), Json::Str("x".into())]),
                Json::Obj(vec![]),
            ]))
        );
    }

    #[test]
    fn string_escapes_round_trip() {
        assert_eq!(
            parse(r#""a\"b\\c\ndAé""#),
            Ok(Json::Str("a\"b\\c\ndA\u{e9}".into()))
        );
        // A surrogate pair is one scalar, not two.
        assert_eq!(parse(r#""😀""#), Ok(Json::Str("\u{1f600}".into())));
        assert!(parse(r#""\ud83d""#).is_err(), "lone surrogate");
    }

    #[test]
    fn junk_is_an_error_not_a_guess() {
        assert!(parse("").is_err());
        assert!(parse("{\"a\":1} extra").is_err());
        assert!(parse("{\"a\"1}").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("\"open").is_err());
        assert!(parse("truthy").is_err());
    }
}
