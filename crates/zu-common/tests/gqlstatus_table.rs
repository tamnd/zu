//! Regenerates `src/gqlstatus/generated.rs` from the ISO/IEC 39075:2024
//! conditions artifact and fails on drift, the same way the TCK
//! scoreboard test works. Run with `ZU_UPDATE_GQLSTATUS=1` to rewrite the
//! file after the artifact changes.
//!
//! The artifact is checked in at `artifacts/gql-conditions.xml` with its
//! SHA-256 next to it, and `make check-artifacts` verifies the hash. Two
//! guards rather than one, because they catch different mistakes: the
//! hash catches a swapped artifact, the drift test catches a hand-edited
//! table.
//!
//! The parser here is deliberately small and deliberately literal about
//! one thing: `<class .../>` and `<class ...>` are different elements.
//! Pairing a self-closing class with the next `</class>` is the obvious
//! bug and it silently reassigns four codes to the wrong classes, so the
//! scanner handles the two spellings apart and a test pins the four codes
//! that would move.

use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
struct Row {
    code: String,
    severity: &'static str,
    class: String,
    subclass: Option<String>,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn severity_for(category: &str) -> &'static str {
    match category {
        "S" => "Severity::Success",
        "N" => "Severity::NoData",
        "W" => "Severity::Warning",
        "I" => "Severity::Informational",
        "X" => "Severity::Exception",
        other => panic!("unknown class category {other:?} in the artifact"),
    }
}

/// Pulls `key="value"` out of the inside of a tag. Attribute values in
/// this artifact never contain a quote, and the DTD forbids one, so a
/// scan to the next `"` is enough.
fn attr(tag: &str, key: &str) -> Option<String> {
    let mut rest = tag;
    while let Some(at) = rest.find(key) {
        let after = &rest[at + key.len()..];
        let trimmed = after.trim_start();
        if !trimmed.starts_with('=') {
            rest = after;
            continue;
        }
        let value = trimmed[1..].trim_start();
        let value = value.strip_prefix('"')?;
        let end = value.find('"')?;
        return Some(value[..end].to_string());
    }
    None
}

/// Yields `(tag_name, inside, self_closing)` for every element start in
/// the document body, in order.
fn scan_tags(body: &str) -> Vec<(String, String, bool)> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let Some(end) = body[i..].find('>') else {
            break;
        };
        let inside = &body[i + 1..i + end];
        i += end + 1;
        if inside.starts_with('/') || inside.starts_with('!') || inside.starts_with('?') {
            continue;
        }
        let self_closing = inside.ends_with('/');
        let inside = inside.trim_end_matches('/');
        let name = inside
            .split(|c: char| c.is_ascii_whitespace())
            .next()
            .unwrap_or("")
            .to_string();
        out.push((name, inside.to_string(), self_closing));
    }
    out
}

fn parse_artifact(xml: &str) -> Vec<Row> {
    // Everything before <conditions> is the DTD, which declares elements
    // named `class` and `subclass` and would otherwise be parsed as data.
    let body_at = xml
        .find("<conditions>")
        .expect("<conditions> in the artifact");
    let body = &xml[body_at..];

    let mut rows = Vec::new();
    let mut open: Option<(String, String, String)> = None; // category, code, name
    for (name, inside, self_closing) in scan_tags(body) {
        match name.as_str() {
            "class" => {
                let category = attr(&inside, "category").expect("class category");
                let code = attr(&inside, "code").expect("class code");
                let class_name = attr(&inside, "name").expect("class name");
                assert_eq!(code.len(), 2, "class code {code:?} is not two characters");
                if self_closing {
                    // A class with no subclasses still has a GQLSTATUS
                    // value: its code followed by `000`.
                    rows.push(Row {
                        code: format!("{code}000"),
                        severity: severity_for(&category),
                        class: class_name,
                        subclass: None,
                    });
                    open = None;
                } else {
                    open = Some((category, code, class_name));
                }
            }
            "subclass" => {
                let (category, class_code, class_name) =
                    open.as_ref().expect("subclass outside a class");
                let sub_code = attr(&inside, "code").expect("subclass code");
                let sub_name = attr(&inside, "name").expect("subclass name");
                assert_eq!(
                    sub_code.len(),
                    3,
                    "subclass code {sub_code:?} is not three characters"
                );
                rows.push(Row {
                    code: format!("{class_code}{sub_code}"),
                    severity: severity_for(category),
                    class: class_name.clone(),
                    subclass: Some(sub_name),
                });
            }
            _ => {}
        }
    }
    rows.sort_by(|a, b| a.code.cmp(&b.code));
    rows
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    out.push_str(
        "//! Generated from `artifacts/gql-conditions.xml`, the ISO/IEC\n\
         //! 39075:2024 conditions artifact. Do not edit by hand: run\n\
         //! `ZU_UPDATE_GQLSTATUS=1 cargo test -p zu-common --test gqlstatus_table`.\n\
         //!\n\
         //! Codes and natural-language names are the standard's, verbatim.\n\
         \n\
         use super::{Condition, GqlStatus, Severity};\n\
         \n",
    );
    out.push_str(
        "/// Every GQLSTATUS value the standard defines, in code order.\n\
         pub(super) static CONDITIONS: &[Condition] = &[\n",
    );
    for row in rows {
        let subclass = match &row.subclass {
            Some(s) => format!("Some(\"{}\")", escape(s)),
            None => "None".to_string(),
        };
        out.push_str(&format!(
            "    Condition {{ code: \"{}\", severity: {}, class: \"{}\", subclass: {} }},\n",
            row.code,
            row.severity,
            escape(&row.class),
            subclass,
        ));
    }
    out.push_str("];\n\n");

    out.push_str(
        "/// One constant per condition, named `C` followed by the code.\n\
         /// The doc comment on each is the standard's own wording.\n\
         pub mod codes {\n    use super::GqlStatus;\n\n",
    );
    for (i, row) in rows.iter().enumerate() {
        let text = match &row.subclass {
            Some(s) => format!("{}, {s}", row.class),
            None => row.class.clone(),
        };
        out.push_str(&format!(
            "    /// `{}` {}\n    pub const C{}: GqlStatus = GqlStatus({i});\n",
            row.code,
            escape(&text),
            row.code,
        ));
    }
    out.push_str("}\n");
    out
}

/// Runs the rendered table through rustfmt so the checked-in file is
/// formatted like every other file in the tree and `cargo fmt --check`
/// has nothing to say about it. Without this the two tools disagree
/// forever: fmt reformats the generated file, then the drift test calls
/// the reformatted file stale.
fn rustfmt(source: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("rustfmt")
        .args(["--edition", "2024", "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("rustfmt on PATH; it is in rust-toolchain.toml components");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(source.as_bytes())
        .expect("write to rustfmt");
    let out = child.wait_with_output().expect("rustfmt output");
    assert!(out.status.success(), "rustfmt rejected the generated table");
    String::from_utf8(out.stdout).expect("rustfmt emits utf-8")
}

#[test]
fn generated_table_matches_the_artifact() {
    let xml = std::fs::read_to_string(manifest_dir().join("artifacts/gql-conditions.xml"))
        .expect("conditions artifact");
    let rows = parse_artifact(&xml);
    let rendered = rustfmt(&render(&rows));

    let path = manifest_dir().join("src/gqlstatus/generated.rs");
    if std::env::var_os("ZU_UPDATE_GQLSTATUS").is_some() {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, &rendered).expect("write generated table");
        return;
    }
    // Checkout on Windows rewrites the line endings, so compare the
    // content rather than the bytes, the same way the TCK scoreboard
    // test does.
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .replace("\r\n", "\n");
    assert_eq!(
        on_disk, rendered,
        "src/gqlstatus/generated.rs is stale; rerun with ZU_UPDATE_GQLSTATUS=1"
    );
}

#[test]
fn the_artifact_has_the_shape_the_denominator_assumes() {
    let xml = std::fs::read_to_string(manifest_dir().join("artifacts/gql-conditions.xml"))
        .expect("conditions artifact");
    let rows = parse_artifact(&xml);
    assert_eq!(rows.len(), 72, "72 GQLSTATUS values");
    assert_eq!(
        rows.iter().filter(|r| r.subclass.is_some()).count(),
        68,
        "68 subclass rows, the conformance denominator"
    );
    // 63 of the 68 subclass rows are exceptions; 2D and G2 are the two
    // class-only exception codes. 02 and 03 are class-only but are no
    // data and informational, which is why 72 minus 4 is not 68 minus 4.
    let by_severity = |s: &str| rows.iter().filter(|r| r.severity.ends_with(s)).count();
    assert_eq!(
        by_severity("Exception"),
        65,
        "63 subclass plus 2 class-only"
    );
    assert_eq!(by_severity("Warning"), 4);
    assert_eq!(by_severity("Success"), 1);
    assert_eq!(by_severity("NoData"), 1);
    assert_eq!(by_severity("Informational"), 1);
    assert_eq!(
        rows.iter()
            .filter(|r| r.subclass.is_some() && r.severity.ends_with("Exception"))
            .count(),
        63,
        "63 exception subclass rows"
    );
}

#[test]
fn self_closing_classes_do_not_steal_the_next_classs_subclasses() {
    // `02` and `03` are self-closing in the artifact and `08` and `2D`
    // follow them. A scanner that pairs `<class .../>` with the next
    // `</class>` reads 08007 as `02007` and 40003 as `2D003`, which are
    // codes that do not exist. These four assertions are the regression.
    let xml = std::fs::read_to_string(manifest_dir().join("artifacts/gql-conditions.xml"))
        .expect("conditions artifact");
    let rows = parse_artifact(&xml);
    let find = |code: &str| rows.iter().find(|r| r.code == code);

    let connection = find("08007").expect("08007 exists");
    assert_eq!(connection.class, "connection exception");
    assert_eq!(
        connection.subclass.as_deref(),
        Some("transaction resolution unknown")
    );

    let rollback = find("40003").expect("40003 exists");
    assert_eq!(rollback.class, "transaction rollback");
    assert_eq!(
        rollback.subclass.as_deref(),
        Some("statement completion unknown")
    );

    assert!(find("02007").is_none(), "02007 is not a GQLSTATUS value");
    assert!(find("2D003").is_none(), "2D003 is not a GQLSTATUS value");

    // The two class-only codes that the same bug would have swallowed.
    assert_eq!(find("02000").expect("02000").subclass, None);
    assert_eq!(find("2D000").expect("2D000").subclass, None);
}
