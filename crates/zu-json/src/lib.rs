//! A small JSON reader and writer. The CLI carries no JSON crate (T7
//! caps the binary at 15 MiB) and the frames it has to read are
//! one-line objects with string, number, bool, and flat object values,
//! so a few hundred lines here beat a dependency.
//!
//! `cargo xtask model` then arrived needing the same two things at a
//! different size: it reads rustdoc's output, which is a third of a
//! megabyte of deeply nested objects, and it writes `model.json`, which
//! is a release artifact whose bytes have to be identical run to run or
//! the CI check comparing it against the committed copy is noise. That
//! is why the reader lives in its own crate rather than inside the CLI,
//! why object fields keep insertion order instead of being sorted or
//! hashed, and why the writer never reorders anything: determinism is
//! the caller's to arrange, and the writer must not take it away.

/// A parsed JSON value. Numbers keep their integer identity when the
/// text has no fraction or exponent, because parameter binding treats
/// ints and floats differently and `7` must not arrive as `7.0`.
///
/// Objects are a `Vec` rather than a map. Duplicate keys are therefore
/// preserved rather than silently collapsed, lookup is a linear scan
/// which is the right shape for the handful of fields a frame or a
/// rustdoc item carries, and the field order a document was written in
/// survives a round trip.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
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
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) if *i >= 0 => Some(*i as u64),
            _ => None,
        }
    }

    /// The elements of an array; `None` for anything else. An empty
    /// array and a missing field are different answers, so this does
    /// not fall back to the empty slice.
    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_obj(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Obj(fields) => Some(fields),
            _ => None,
        }
    }

    /// The value on one line, no spaces. What a frame on a pipe wants.
    pub fn to_compact(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, None, 0);
        out
    }

    /// The value indented two spaces per level, one line per element,
    /// with a trailing newline. What a file under review wants, because
    /// a diff of a one-line artifact tells a reader nothing.
    pub fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, Some(2), 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, indent: Option<usize>, depth: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => out.push_str(&i.to_string()),
            // `{:?}` on f64 is the shortest text that reads back as the
            // same bits, which is the only formatting that survives a
            // round trip. JSON has no infinity and no NaN, so those
            // become null rather than text no parser will take back.
            Json::Float(f) if f.is_finite() => out.push_str(&format!("{f:?}")),
            Json::Float(_) => out.push_str("null"),
            Json::Str(s) => escape_into(s, out),
            Json::Arr(items) => write_seq(out, indent, depth, '[', ']', items.len(), |out, i| {
                items[i].write(out, indent, depth + 1);
            }),
            Json::Obj(fields) => write_seq(out, indent, depth, '{', '}', fields.len(), |out, i| {
                escape_into(&fields[i].0, out);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                fields[i].1.write(out, indent, depth + 1);
            }),
        }
    }
}

/// Writes the shared bracket, separator, and indentation shape of an
/// array and an object, so the two cannot drift into different spacing.
fn write_seq(
    out: &mut String,
    indent: Option<usize>,
    depth: usize,
    open: char,
    close: char,
    len: usize,
    mut element: impl FnMut(&mut String, usize),
) {
    out.push(open);
    // An empty container stays on one line in both modes; `[\n]` is
    // noise no reader wants and no diff is improved by.
    if len == 0 {
        out.push(close);
        return;
    }
    for i in 0..len {
        if i > 0 {
            out.push(',');
        }
        if let Some(step) = indent {
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', step * (depth + 1)));
        }
        element(out, i);
    }
    if let Some(step) = indent {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', step * depth));
    }
    out.push(close);
}

/// Appends `s` as a quoted JSON string. Escapes the two characters JSON
/// requires and the C0 controls, and leaves every other scalar as UTF-8
/// so a doc comment full of accented text stays readable in the file
/// rather than turning into a wall of `é`.
pub fn escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// How deep a document may nest before the parser calls it hostile.
const MAX_DEPTH: usize = 128;

/// Parses one complete JSON value and rejects trailing input, so a
/// frame with junk after the closing brace is an error the sender
/// hears about instead of a silently truncated read.
pub fn parse(text: &str) -> Result<Json, String> {
    let bytes = text.as_bytes();
    let mut pos = 0;
    let value = parse_value(bytes, &mut pos, 0)?;
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

fn parse_value(bytes: &[u8], pos: &mut usize, depth: usize) -> Result<Json, String> {
    // Nesting is bounded because the parser recurses and the shell
    // reads frames from whoever is on the other end of the pipe. A
    // thousand open brackets should be a rejected frame, not a stack
    // overflow, and no real document comes near this.
    if depth > MAX_DEPTH {
        return Err(format!("nested deeper than {MAX_DEPTH}"));
    }
    skip_ws(bytes, pos);
    match bytes.get(*pos) {
        None => Err("unexpected end of input".to_string()),
        Some(b'{') => parse_obj(bytes, pos, depth),
        Some(b'[') => parse_arr(bytes, pos, depth),
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
    if !fractional && let Ok(i) = text.parse::<i64>() {
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
                // Ordinary bytes arrive in runs, and the run is copied
                // whole. Validating one scalar at a time would mean
                // calling `from_utf8` on the entire tail of the input
                // per character, which walks the tail every time and
                // turns reading a document into quadratic work. The
                // two bytes that end a run are both ASCII, so a run
                // boundary can never fall inside a multi-byte scalar.
                let start = *pos;
                while let Some(c) = bytes.get(*pos) {
                    if matches!(c, b'"' | b'\\') {
                        break;
                    }
                    *pos += 1;
                }
                // The input arrived as a `&str` and both run terminators
                // are ASCII, so this slice is valid UTF-8 by
                // construction; the check is here because
                // `from_utf8_unchecked` would trade a proof for unsafe.
                let run = std::str::from_utf8(&bytes[start..*pos])
                    .map_err(|_| "invalid utf-8 in string".to_string())?;
                out.push_str(run);
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

fn parse_arr(bytes: &[u8], pos: &mut usize, depth: usize) -> Result<Json, String> {
    *pos += 1;
    let mut items = Vec::new();
    skip_ws(bytes, pos);
    if bytes.get(*pos) == Some(&b']') {
        *pos += 1;
        return Ok(Json::Arr(items));
    }
    loop {
        items.push(parse_value(bytes, pos, depth + 1)?);
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

fn parse_obj(bytes: &[u8], pos: &mut usize, depth: usize) -> Result<Json, String> {
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
        fields.push((key, parse_value(bytes, pos, depth + 1)?));
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
    fn runs_of_plain_text_are_copied_whole_and_still_land_where_they_should() {
        // The reader copies unescaped text a run at a time rather than a
        // scalar at a time, so what needs proving is that a run ends in
        // the right place: at an escape, at the closing quote, and never
        // inside a multi-byte scalar.
        let text = "\"écrit 😀 par Ada\\tet relu\\u00e9 par 日本\"";
        assert_eq!(
            parse(text),
            Ok(Json::Str("écrit 😀 par Ada\tet relu\u{e9} par 日本".into()))
        );
        // Escapes back to back leave runs of length zero between them.
        assert_eq!(parse(r#""\t\t\t""#), Ok(Json::Str("\t\t\t".into())));
        assert_eq!(parse(r#""""#), Ok(Json::Str(String::new())));
        // A run that reaches the end of the input is an unterminated
        // string, not a string.
        assert!(parse("\"日本").is_err());
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

    #[test]
    fn a_hostile_frame_is_rejected_rather_than_overflowing_the_stack() {
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert!(parse(&deep).is_err(), "unbounded nesting was accepted");
        // The bound is a ceiling on hostility, not on real documents:
        // rustdoc's deepest nesting is a generic type a dozen levels in.
        let fine = "[".repeat(MAX_DEPTH - 1) + &"]".repeat(MAX_DEPTH - 1);
        assert!(parse(&fine).is_ok(), "a legal depth was rejected");
    }

    /// The round trip is the property the writer has to hold: whatever
    /// went in has to come back out with its types and its field order
    /// intact, in both modes, because `model.json` is read by tools in
    /// six other languages that will not forgive a reordered object.
    #[test]
    fn both_modes_round_trip_and_keep_field_order() {
        let text = r#"{"z":1,"a":[true,null,-2,0.5,"x"],"nested":{"b":{},"c":[]},"m":"a\"b\n"}"#;
        let value = parse(text).expect("fixture");
        for rendered in [value.to_compact(), value.to_pretty()] {
            assert_eq!(parse(&rendered), Ok(value.clone()), "in {rendered}");
        }
        // Sorting the keys would be a silent change to a document the
        // caller ordered on purpose, so the writer does not.
        assert!(value.to_compact().starts_with(r#"{"z":1,"a":"#));
    }

    #[test]
    fn the_two_modes_differ_only_in_whitespace() {
        let value = parse(r#"{"a":[1,{"b":2}],"c":{}}"#).expect("fixture");
        assert_eq!(value.to_compact(), r#"{"a":[1,{"b":2}],"c":{}}"#);
        assert_eq!(
            value.to_pretty(),
            "{\n  \"a\": [\n    1,\n    {\n      \"b\": 2\n    }\n  ],\n  \"c\": {}\n}\n"
        );
    }

    #[test]
    fn writing_the_same_value_twice_gives_the_same_bytes() {
        // The CI check compares a regenerated model against the copy in
        // the tree byte for byte, so a writer that varied at all would
        // turn that check into a coin toss.
        let value = parse(r#"{"b":2,"a":1,"list":[3,4,5]}"#).expect("fixture");
        let once = value.to_pretty();
        for _ in 0..8 {
            assert_eq!(value.to_pretty(), once);
        }
    }

    #[test]
    fn escapes_cover_what_json_forbids_and_leave_the_rest_alone() {
        let mut out = String::new();
        escape_into("tab\there\u{1}\"q\\", &mut out);
        // Tab has a spelling of its own, U+0001 has none and takes
        // the numeric form, and the quote and the backslash have to
        // go or the string ends early.
        assert_eq!(out, "\"tab\\there\\u0001\\\"q\\\\\"");
        // Text outside the C0 range stays as UTF-8. A model full of
        // é is legal and unreadable, and this file gets read.
        assert_eq!(Json::Str("café 😀".into()).to_compact(), "\"café 😀\"");
    }

    #[test]
    fn numbers_keep_their_kind_and_their_value() {
        assert_eq!(
            Json::Int(-9007199254740993).to_compact(),
            "-9007199254740993"
        );
        // Shortest round-trip formatting, so 0.1 does not come back as
        // 0.1000000000000000055511151231257827.
        assert_eq!(Json::Float(0.1).to_compact(), "0.1");
        assert_eq!(Json::Float(1e300).to_compact(), "1e300");
        // JSON has no spelling for these, and emitting one anyway would
        // hand every consumer a parse error instead of a value.
        assert_eq!(Json::Float(f64::NAN).to_compact(), "null");
        assert_eq!(Json::Float(f64::INFINITY).to_compact(), "null");
    }
}
