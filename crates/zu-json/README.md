# zudb-json

A small JSON reader and writer, about 550 lines with no dependencies. It is used by the `zu` command-line tool for its line protocol and `--format json` output, and by the repository's codegen tool, which reads rustdoc's JSON output and writes the API model file.

## Why it exists

The `zu` binary has a 15 MiB size budget, and the JSON it handles is small: one-line protocol frames with strings, numbers, booleans and flat objects. A few hundred lines here cost less than a general JSON crate. The codegen tool needed the same thing at a larger size, a third of a megabyte of nested rustdoc output, and it needed the output file to be byte-for-byte identical from run to run so CI can diff it against the committed copy. That is why the reader is its own crate and why it keeps field order.

If you need JSON in your own project, use `serde_json`. This crate does not do serde, streaming, or any of the conveniences you would expect from a general library.

## Usage

```rust
use zu_json::{Json, parse};

fn main() -> Result<(), String> {
    let frame = parse(r#"{"op":"query","q":"MATCH (n) RETURN n","params":{"limit":10}}"#)?;
    assert_eq!(frame.get("op").and_then(Json::as_str), Some("query"));
    let limit = frame.get("params").and_then(|p| p.get("limit")).and_then(Json::as_i64);
    assert_eq!(limit, Some(10));

    let reply = Json::Obj(vec![
        ("gqlstatus".to_string(), Json::Str("00000".to_string())),
        ("columns".to_string(), Json::Arr(vec![Json::Str("n".to_string())])),
        ("rows".to_string(), Json::Arr(vec![])),
    ]);
    println!("{}", reply.to_compact());
    Ok(())
}
```

## How it works

Everything is in `lib.rs`.

- `Json` is the value type: `Null`, `Bool`, `Int(i64)`, `Float(f64)`, `Str`, `Arr(Vec<Json>)` and `Obj(Vec<(String, Json)>)`.
- Objects are a vector of pairs, not a map. Field order survives a round trip, duplicate keys are kept rather than collapsed, and `get` is a linear scan, which is fast enough for the handful of fields a frame has.
- Accessors return `Option`: `get`, `as_str`, `as_bool`, `as_i64`, `as_u64`, `as_arr`, `as_obj`. An empty array and a missing field are different answers.
- `parse(text)` is a recursive descent parser. It reads exactly one value and rejects trailing input, refuses nesting deeper than 128 levels, and handles `\u` escapes including surrogate pairs. A number with no fraction or exponent that fits in an `i64` becomes `Int`, and anything else becomes `Float`. Errors are a `String` with the byte position.
- `to_compact()` writes one line with no spaces, and `to_pretty()` indents two spaces per level and ends with a newline. Floats are written as the shortest text that reads back to the same bits. NaN and infinity become `null` because JSON has no spelling for them.
- `escape_into(s, out)` escapes the quote, the backslash and the control characters, and leaves all other characters as UTF-8.

## Should you depend on this?

Probably not directly. This crate is one part of [zu](https://github.com/tamnd/zu), an embedded property-graph database, and it is published so that [`zudb-cli`](https://crates.io/crates/zudb-cli) can depend on it. Its API changes whenever zu needs it to, with no deprecation period, and the versions of all the zu crates move together. To use zu from Rust, add [`zudb`](https://crates.io/crates/zudb).

The package is named `zudb-json` because `zu` was already taken on crates.io. The library keeps the name the workspace uses, so the import is `use zu_json`.

## License

Apache-2.0
